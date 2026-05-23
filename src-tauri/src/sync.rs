//! FreshRSS synchronisation over the Google Reader compatible API.
//!
//! Flow: `ClientLogin` for an auth token, push any queued local read/starred
//! changes via `edit-tag`, then pull the subscription list (to subscribe to
//! new feeds) and the recent reading-list (to reconcile read/starred state,
//! matched to local articles by URL).

use crate::db;
use crate::error::{AppError, AppResult};
use crate::ingestion::parse;
use crate::state::AppState;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use tauri::{AppHandle, Manager};

const READ_TAG: &str = "user/-/state/com.google/read";
const STARRED_TAG: &str = "user/-/state/com.google/starred";
const READING_LIST: &str = "user/-/state/com.google/reading-list";

/// Normalise a user-supplied FreshRSS URL to its GReader API root.
fn greader_base(url: &str) -> String {
    let t = url.trim().trim_end_matches('/');
    if t.contains("/api/greader.php") {
        t.to_string()
    } else {
        format!("{t}/api/greader.php")
    }
}

/// An authenticated FreshRSS session.
struct Session {
    base: String,
    auth: String,
    token: String,
}

impl Session {
    fn get(&self, http: &Client, path: &str) -> RequestBuilder {
        http.get(format!("{}/reader/api/0/{path}", self.base))
            .header("Authorization", format!("GoogleLogin auth={}", self.auth))
    }
    fn post(&self, http: &Client, path: &str) -> RequestBuilder {
        http.post(format!("{}/reader/api/0/{path}", self.base))
            .header("Authorization", format!("GoogleLogin auth={}", self.auth))
    }
}

/// Exchange username + password for a long-lived auth token.
async fn client_login(http: &Client, base: &str, user: &str, pass: &str) -> AppResult<String> {
    let resp = http
        .post(format!("{base}/accounts/ClientLogin"))
        .form(&[("Email", user), ("Passwd", pass)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AppError::code("freshrssLoginFailed"));
    }
    let body = resp.text().await?;
    body.lines()
        .find_map(|l| l.strip_prefix("Auth="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::code("freshrssNoToken"))
}

/// Build a session from an existing auth token by fetching a fresh write
/// (edit-tag) token. Fails fast if the auth token is no longer valid.
async fn session_with_token(http: &Client, base: &str, auth: String) -> AppResult<Session> {
    let token = http
        .get(format!("{base}/reader/api/0/token"))
        .header("Authorization", format!("GoogleLogin auth={auth}"))
        .send()
        .await?
        .error_for_status()
        .map_err(|_| AppError::code("freshrssLoginFailed"))?
        .text()
        .await?
        .trim()
        .to_string();
    Ok(Session {
        base: base.to_string(),
        auth,
        token,
    })
}

/// Log in with username + password and obtain a full session.
async fn login(http: &Client, base: &str, user: &str, pass: &str) -> AppResult<Session> {
    let auth = client_login(http, base, user, pass).await?;
    session_with_token(http, base, auth).await
}

#[derive(Deserialize)]
struct SubList {
    #[serde(default)]
    subscriptions: Vec<Sub>,
}
#[derive(Deserialize)]
struct Sub {
    url: Option<String>,
    title: Option<String>,
    /// The feed's folders. FreshRSS files a feed under a single category, but
    /// the GReader API models this as a list (`user/-/label/<name>` ids with a
    /// human `label`); we take the first non-empty label as the folder name.
    #[serde(default)]
    categories: Vec<Category>,
}
#[derive(Deserialize)]
struct Category {
    label: Option<String>,
}

impl Sub {
    /// The feed URL to subscribe to, ignoring an empty string.
    fn feed_url(&self) -> Option<&str> {
        self.url.as_deref().filter(|u| !u.is_empty())
    }
    /// The folder this feed belongs in: the first category with a non-empty
    /// (trimmed) `label`, or `None` when the feed is uncategorised.
    fn folder(&self) -> Option<&str> {
        self.categories
            .iter()
            .filter_map(|c| c.label.as_deref())
            .map(str::trim)
            .find(|l| !l.is_empty())
    }
}

#[derive(Deserialize)]
struct Contents {
    #[serde(default)]
    items: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    canonical: Vec<Href>,
    #[serde(default)]
    alternate: Vec<Href>,
}
#[derive(Deserialize)]
struct Href {
    href: String,
}

/// Stored FreshRSS connection. We persist the long-lived GReader auth token
/// rather than the password — a leaked token is revocable server-side and
/// can't be replayed against the user's other accounts. `legacy_pass` holds
/// a plaintext password from an older install, awaiting one-time migration.
struct Creds {
    url: String,
    user: String,
    auth: Option<String>,
    legacy_pass: Option<String>,
}

/// Stored FreshRSS credentials, if a server is configured.
async fn creds(app: &AppHandle) -> AppResult<Option<Creds>> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    let url = db::get_setting(&conn, "freshrss_url")?.unwrap_or_default();
    let user = db::get_setting(&conn, "freshrss_user")?.unwrap_or_default();
    let nonempty = |k| db::get_setting(&conn, k).map(|v| v.filter(|s| !s.is_empty()));
    let auth = nonempty("freshrss_auth")?;
    let legacy_pass = nonempty("freshrss_pass")?;
    if url.trim().is_empty() || user.is_empty() || (auth.is_none() && legacy_pass.is_none()) {
        return Ok(None);
    }
    Ok(Some(Creds { url, user, auth, legacy_pass }))
}

/// The configured FreshRSS server URL, or `None` when not connected.
pub async fn connected_url(app: &AppHandle) -> AppResult<Option<String>> {
    Ok(creds(app).await?.map(|c| c.url))
}

/// Persist a verified connection, storing the auth token and never the
/// password (any legacy stored password is also cleared).
async fn persist_session(app: &AppHandle, url: &str, user: &str, auth: &str) -> AppResult<()> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    db::set_setting(&conn, "freshrss_url", url.trim())?;
    db::set_setting(&conn, "freshrss_user", user)?;
    db::set_setting(&conn, "freshrss_auth", auth)?;
    db::set_setting(&conn, "freshrss_pass", "")?;
    Ok(())
}

/// Verify credentials against the server and, on success, persist them.
pub async fn connect(app: &AppHandle, url: &str, user: &str, pass: &str) -> AppResult<()> {
    let base = greader_base(url);
    let http = app.state::<AppState>().http();
    let session = login(&http, &base, user, pass).await?; // verifies credentials
    persist_session(app, url, user, &session.auth).await
}

/// Forget the stored FreshRSS credentials.
pub async fn disconnect(app: &AppHandle) -> AppResult<()> {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    for key in ["freshrss_url", "freshrss_user", "freshrss_auth", "freshrss_pass"] {
        db::set_setting(&conn, key, "")?;
    }
    Ok(())
}

/// Run a full sync if a server is connected. Returns `true` when a sync
/// actually ran, so the caller can refresh the UI for the reconciled state.
pub async fn run_if_connected(app: &AppHandle) -> AppResult<bool> {
    if creds(app).await?.is_some() {
        sync_now(app).await.map(|_| true)
    } else {
        Ok(false)
    }
}

/// Reconcile one FreshRSS subscription into the local feed list: subscribe to
/// a feed we don't yet have and file it under its FreshRSS category folder
/// (creating the folder when needed). An already-known feed is filed only when
/// it currently has no folder, so a folder the user set inside Papr is kept
/// rather than being overwritten by the server's organisation on every sync.
fn reconcile_subscription(
    conn: &rusqlite::Connection,
    feed_url: &str,
    title: Option<&str>,
    folder: Option<&str>,
) -> AppResult<()> {
    match db::find_feed_by_url(conn, feed_url)? {
        None => {
            let folder_id = match folder {
                Some(name) => Some(db::folder_id_by_name(conn, name)?),
                None => None,
            };
            let title = title.unwrap_or(feed_url);
            let source_type = parse::detect_source_type(feed_url);
            db::insert_feed(conn, feed_url, None, title, None, source_type, folder_id)?;
        }
        Some(feed_id) => {
            if let Some(name) = folder {
                if db::feed_folder_id(conn, feed_id)?.is_none() {
                    let folder_id = db::folder_id_by_name(conn, name)?;
                    db::move_feed(conn, feed_id, Some(folder_id))?;
                }
            }
        }
    }
    Ok(())
}

/// Push queued changes, then pull subscriptions and read/starred state.
/// Returns the number of local articles whose state was reconciled.
pub async fn sync_now(app: &AppHandle) -> AppResult<usize> {
    let creds = creds(app)
        .await?
        .ok_or_else(|| AppError::code("freshrssNotConnected"))?;
    let base = greader_base(&creds.url);
    let http = app.state::<AppState>().http();
    let session = match &creds.auth {
        Some(auth) => session_with_token(&http, &base, auth.clone()).await?,
        None => {
            // Legacy install: exchange the plaintext password for a token,
            // then migrate so the password is no longer kept on disk.
            let pass = creds.legacy_pass.as_deref().unwrap_or_default();
            let session = login(&http, &base, &creds.user, pass).await?;
            persist_session(app, &creds.url, &creds.user, &session.auth).await?;
            session
        }
    };

    // 1 ── push: flush queued local read/starred changes. `take_sync_queue`
    // removes the rows up front, so any push that fails must be re-queued —
    // otherwise a network blip silently drops the user's change forever.
    let queue = {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        db::take_sync_queue(&conn)?
    };
    let mut failed: Vec<db::SyncEntry> = Vec::new();
    for entry in queue {
        let tag = if entry.field == "starred" {
            STARRED_TAG
        } else {
            READ_TAG
        };
        let action = if entry.value { "a" } else { "r" };
        let pushed = session
            .post(&http, "edit-tag")
            .form(&[
                ("i", entry.remote_id.as_str()),
                (action, tag),
                ("T", session.token.as_str()),
            ])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .is_ok();
        if !pushed {
            failed.push(entry);
        }
    }
    if !failed.is_empty() {
        log::warn!("sync: {} change(s) failed to push, re-queued", failed.len());
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        for entry in &failed {
            let _ = db::requeue_sync(&conn, entry.article_id, &entry.field, entry.value);
        }
    }

    // 2 ── pull subscriptions: subscribe locally to any feed we don't have.
    let subs: SubList = session
        .get(&http, "subscription/list?output=json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        for sub in &subs.subscriptions {
            let Some(feed_url) = sub.feed_url() else {
                continue;
            };
            // One bad subscription must not abort the whole pull.
            let _ = reconcile_subscription(&conn, feed_url, sub.title.as_deref(), sub.folder());
        }
    }

    // 3 ── pull read/starred state for recent items, matched by URL.
    let contents: Contents = session
        .get(
            &http,
            &format!("stream/contents/{READING_LIST}?output=json&n=1000"),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let mut reconciled = 0usize;
    {
        let state = app.state::<AppState>();
        let conn = state.db.lock().await;
        // Articles with still-unsent local edits keep their local state; we
        // only assign their remote id so the next sync can push them.
        let pending: std::collections::HashSet<i64> =
            db::pending_sync_article_ids(&conn)?.into_iter().collect();
        for item in contents.items {
            let url = item
                .canonical
                .first()
                .or_else(|| item.alternate.first())
                .map(|h| h.href.clone());
            let Some(url) = url else { continue };
            if let Some(aid) = db::article_id_by_url(&conn, &url)? {
                db::set_remote_id(&conn, aid, &item.id)?;
                if !pending.contains(&aid) {
                    let read = item.categories.iter().any(|c| c == READ_TAG);
                    let starred = item.categories.iter().any(|c| c == STARRED_TAG);
                    db::set_sync_state(&conn, aid, read, starred)?;
                }
                reconciled += 1;
            }
        }
    }
    Ok(reconciled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name of the folder a feed currently sits in, or `None` if ungrouped.
    fn folder_of(conn: &rusqlite::Connection, feed_url: &str) -> Option<String> {
        let feed = db::list_feeds(conn)
            .unwrap()
            .into_iter()
            .find(|f| f.feed_url == feed_url)
            .expect("feed should exist");
        feed.folder_id.map(|id| {
            db::list_folders(conn)
                .unwrap()
                .into_iter()
                .find(|fo| fo.id == id)
                .expect("folder should exist")
                .name
        })
    }

    #[test]
    fn subscribes_a_new_feed_into_its_freshrss_folder() {
        let conn = db::test_conn();
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), Some("Tech")).unwrap();
        assert_eq!(folder_of(&conn, "https://a.example/feed").as_deref(), Some("Tech"));
    }

    #[test]
    fn subscribes_an_uncategorised_feed_without_a_folder() {
        let conn = db::test_conn();
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), None).unwrap();
        assert_eq!(folder_of(&conn, "https://a.example/feed"), None);
    }

    #[test]
    fn files_an_existing_ungrouped_feed_into_its_folder() {
        let conn = db::test_conn();
        // First sync: the feed arrives without a category, so it is ungrouped —
        // exactly the state of feeds synced before folders were handled.
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), None).unwrap();
        assert_eq!(folder_of(&conn, "https://a.example/feed"), None);
        // A later sync reports the category: the feed is filed into the folder.
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), Some("News")).unwrap();
        assert_eq!(folder_of(&conn, "https://a.example/feed").as_deref(), Some("News"));
    }

    #[test]
    fn keeps_a_feeds_local_folder_over_the_server_category() {
        let conn = db::test_conn();
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), Some("Tech")).unwrap();
        // The server later reports a different category; the local placement
        // must win, and no duplicate feed may be created.
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), Some("News")).unwrap();
        assert_eq!(folder_of(&conn, "https://a.example/feed").as_deref(), Some("Tech"));
        let count = db::list_feeds(&conn)
            .unwrap()
            .into_iter()
            .filter(|f| f.feed_url == "https://a.example/feed")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn reuses_an_existing_folder_by_name_case_insensitively() {
        let conn = db::test_conn();
        reconcile_subscription(&conn, "https://a.example/feed", Some("A"), Some("Tech")).unwrap();
        reconcile_subscription(&conn, "https://b.example/feed", Some("B"), Some("tech")).unwrap();
        assert_eq!(db::list_folders(&conn).unwrap().len(), 1);
    }

    /// End-to-end of the subscription pull *minus the network*: a verbatim
    /// FreshRSS `subscription/list?output=json` body is deserialised with the
    /// real serde structs, then fed through the same `feed_url()` / `folder()`
    /// extraction and `reconcile_subscription` calls the sync loop uses. This
    /// guards the wiring the direct-arg tests above skip: that FreshRSS's actual
    /// `categories` shape parses, and that the folder label is pulled from it.
    #[test]
    fn parses_a_real_freshrss_payload_and_files_each_feed() {
        // Shape taken from FreshRSS's GReader `subscription/list` output, extra
        // fields (id, htmlUrl, iconUrl, sortid…) included to prove serde skips
        // the ones we don't model.
        let body = r#"{
          "subscriptions": [
            {
              "id": "feed/1",
              "title": "Rust Blog",
              "categories": [{ "id": "user/-/label/Tech", "label": "Tech" }],
              "url": "https://blog.rust-lang.org/feed.xml",
              "htmlUrl": "https://blog.rust-lang.org",
              "iconUrl": "https://blog.rust-lang.org/favicon.ico",
              "sortid": "ABCD0001"
            },
            {
              "id": "feed/2",
              "title": "Hacker News",
              "categories": [{ "id": "user/-/label/Tech", "label": "Tech" }],
              "url": "https://news.ycombinator.com/rss"
            },
            {
              "id": "feed/3",
              "title": "Unfiled Feed",
              "categories": [],
              "url": "https://example.com/unfiled.xml"
            },
            {
              "id": "feed/4",
              "title": "Blank Category",
              "categories": [{ "id": "user/-/label/", "label": "   " }],
              "url": "https://example.com/blank.xml"
            },
            {
              "id": "feed/5",
              "title": "No URL",
              "categories": [{ "id": "user/-/label/Tech", "label": "Tech" }],
              "url": ""
            }
          ]
        }"#;

        let subs: SubList = serde_json::from_str(body).expect("real payload must deserialize");
        let conn = db::test_conn();
        for sub in &subs.subscriptions {
            let Some(feed_url) = sub.feed_url() else { continue };
            reconcile_subscription(&conn, feed_url, sub.title.as_deref(), sub.folder()).unwrap();
        }

        // Two feeds carry the same "Tech" label → exactly one folder, both in it.
        assert_eq!(db::list_folders(&conn).unwrap().len(), 1);
        assert_eq!(
            folder_of(&conn, "https://blog.rust-lang.org/feed.xml").as_deref(),
            Some("Tech")
        );
        assert_eq!(
            folder_of(&conn, "https://news.ycombinator.com/rss").as_deref(),
            Some("Tech")
        );
        // No category, and a whitespace-only label, both resolve to ungrouped.
        assert_eq!(folder_of(&conn, "https://example.com/unfiled.xml"), None);
        assert_eq!(folder_of(&conn, "https://example.com/blank.xml"), None);
        // The empty-URL subscription is skipped, not inserted as an empty feed.
        assert!(db::find_feed_by_url(&conn, "").unwrap().is_none());
        assert_eq!(db::list_feeds(&conn).unwrap().len(), 4);
    }
}
