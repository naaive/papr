//! Feed refresh: a reusable `refresh_all` (driven by both the manual command
//! and the periodic timer) plus the background scheduler loop.

use crate::db;
use crate::error::AppResult;
use crate::ingestion::newsletter;
use crate::ingestion::{fetch, parse, rsshub};
use crate::models::RefreshProgress;
use crate::state::AppState;
use crate::{notify, sync, tray};
use std::sync::Arc;
use std::time::Duration;
use tauri::{ipc::Channel, AppHandle, Emitter, Manager};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Wall-clock cap for polling one newsletter mailbox over IMAP. Generous
/// enough for a slow mailbox with large messages, short enough that a wedged
/// server never stalls the refresh cycle. Shared with the interactive
/// `add_newsletter_source` probe so a hung server can't hang the Add dialog
/// either.
pub const NEWSLETTER_POLL_TIMEOUT_SECS: u64 = 90;

/// Result of polling one newsletter mailbox: the feed id paired with either
/// the raw RFC822 message bytes or an error string describing the failure.
type MailboxPoll = (i64, Result<Vec<Vec<u8>>, String>);

/// Insert a batch of articles for one feed in bounded chunks, releasing the
/// shared DB lock between each so concurrent UI queries aren't starved while a
/// large feed (hundreds of items) is being ingested. Returns the count newly
/// inserted; `label` only distinguishes the warning text (`rss`/`newsletter`).
async fn upsert_articles(
    db: &tokio::sync::Mutex<rusqlite::Connection>,
    feed_id: i64,
    articles: &[crate::db::NewArticle],
    dedup: bool,
    rules: &[crate::models::Rule],
    label: &str,
) -> usize {
    let mut new_count = 0usize;
    for chunk in articles.chunks(64) {
        let conn = db.lock().await;
        for article in chunk {
            match db::upsert_article(&conn, feed_id, article, dedup, rules) {
                Ok(true) => new_count += 1,
                Ok(false) => {}
                Err(e) => log::warn!("{label} upsert failed (feed {feed_id}): {e}"),
            }
        }
    }
    new_count
}

/// Outcome of fetching one feed.
enum Outcome {
    NotModified,
    Updated {
        parsed: parse::ParsedFeed,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    Failed(String),
}

async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
    etag: Option<String>,
    last_modified: Option<String>,
) -> Outcome {
    match fetch::conditional_get(client, url, etag.as_deref(), last_modified.as_deref()).await {
        Ok(fetch::Fetched::NotModified) => Outcome::NotModified,
        Ok(fetch::Fetched::Body {
            bytes,
            etag,
            last_modified,
        }) => match parse::parse_feed(&bytes, url) {
            Ok(parsed) => Outcome::Updated {
                parsed,
                etag,
                last_modified,
            },
            Err(e) => Outcome::Failed(e.to_string()),
        },
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Refresh every feed (bounded concurrency). Streams per-feed progress over
/// `progress` when provided, emits `feeds-updated`, fires a notification,
/// runs retention cleanup, syncs to FreshRSS, and returns the new-article count.
pub async fn refresh_all(
    app: &AppHandle,
    progress: Option<Channel<RefreshProgress>>,
    wait_if_busy: bool,
) -> AppResult<usize> {
    let state = app.state::<AppState>();

    // Only one refresh at a time: the manual command and the periodic
    // scheduler would otherwise duplicate every fetch. `wait_if_busy` callers
    // (OPML import) queue behind an in-flight run so their freshly added
    // feeds still get fetched; everyone else bows out cleanly.
    let _refresh_guard = if wait_if_busy {
        state.refresh_lock.lock().await
    } else {
        match state.refresh_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                log::debug!("refresh already in progress; skipping this run");
                if let Some(p) = &progress {
                    let _ = p.send(RefreshProgress::Started { total: 0 });
                    let _ = p.send(RefreshProgress::Finished { new_articles: 0 });
                }
                return Ok(0);
            }
        }
    };

    let (feeds, concurrency, dedup, rules, rsshub_instance) = {
        let conn = state.db.lock().await;
        let feeds = db::feeds_to_refresh(&conn)?;
        let concurrency =
            db::setting_parsed::<i64>(&conn, "net_concurrency", 6).clamp(1, 16) as usize;
        let dedup = db::setting_flag(&conn, "dedup_enabled", false);
        let rules = db::active_rules(&conn).unwrap_or_default();
        // RSSHub feeds are stored as instance-independent `rsshub://` URLs and
        // expanded against this instance at fetch time, so changing it re-routes
        // every RSSHub feed at once.
        let rsshub_instance = db::get_setting(&conn, rsshub::INSTANCE_SETTING)
            .ok()
            .flatten()
            .unwrap_or_else(|| rsshub::DEFAULT_INSTANCE.to_string());
        (feeds, concurrency, dedup, rules, rsshub_instance)
    };
    if let Some(p) = &progress {
        let _ = p.send(RefreshProgress::Started { total: feeds.len() });
    }

    let sem = Arc::new(Semaphore::new(concurrency));
    // The feed URL travels back out alongside the outcome — `refine_source_type`
    // needs it for the Mastodon `/@user.rss` pattern check below.
    let mut set: JoinSet<(i64, String, Outcome)> = JoinSet::new();
    for (id, url, etag, last_modified) in feeds {
        let client = state.http();
        let sem = sem.clone();
        // Expand `rsshub://` URLs against the configured instance for the fetch;
        // a plain URL passes through unchanged. The original `url` travels back
        // out for `refine_source_type`.
        let fetch_url = rsshub::expand(&url, &rsshub_instance);
        set.spawn(async move {
            let _permit = sem.acquire().await;
            let outcome = fetch_one(&client, &fetch_url, etag, last_modified).await;
            (id, url, outcome)
        });
    }

    let mut total_new = 0usize;
    while let Some(joined) = set.join_next().await {
        let Ok((feed_id, feed_url, outcome)) = joined else {
            continue;
        };
        let mut new_here = 0usize;
        let mut error: Option<String> = None;

        match outcome {
            Outcome::NotModified => {
                let conn = state.db.lock().await;
                let _ = db::touch_feed(&conn, feed_id);
            }
            Outcome::Failed(e) => {
                let conn = state.db.lock().await;
                let _ = db::set_feed_error(&conn, feed_id, &e);
                error = Some(e);
            }
            Outcome::Updated {
                parsed,
                etag,
                last_modified,
            } => {
                new_here += upsert_articles(
                    &state.db,
                    feed_id,
                    &parsed.articles,
                    dedup,
                    &rules,
                    "rss",
                )
                .await;
                let conn = state.db.lock().await;
                let _ = db::update_feed_meta(
                    &conn,
                    feed_id,
                    parsed.title.as_deref(),
                    parsed.site_url.as_deref(),
                    parsed.description.as_deref(),
                    parsed.icon.as_deref(),
                );
                let _ = db::set_feed_fetch_state(
                    &conn,
                    feed_id,
                    etag.as_deref(),
                    last_modified.as_deref(),
                    None,
                );
                // Promote a still-generic `'rss'` feed to its real kind now
                // that the parsed document reveals it (audio enclosures →
                // podcast, `/@user.rss` → mastodon). `add_feed` already does
                // this at subscribe time; `import_opml` cannot — it only sees
                // the URL — so an OPML-imported podcast would otherwise stay
                // mislabelled forever. The DB call is a no-op for an already
                // classified feed.
                let refined = parse::refine_source_type(
                    crate::models::SourceType::Rss,
                    &parsed,
                    &feed_url,
                );
                let _ = db::refine_feed_source_type(&conn, feed_id, refined);
            }
        }

        total_new += new_here;
        if let Some(p) = &progress {
            let _ = p.send(RefreshProgress::FeedDone {
                feed_id,
                new_articles: new_here,
                error,
            });
        }
    }

    // Newsletter sources: poll each configured IMAP mailbox and ingest any
    // new messages as articles, alongside the RSS refresh above.
    total_new += poll_newsletters(app, dedup, &rules).await;

    // Retention: drop old read articles when a finite window is configured.
    // The DELETE scans the whole table, so throttle it to once per day rather
    // than running on every refresh cycle.
    {
        let conn = state.db.lock().await;
        let retention = db::get_setting(&conn, "retention_days").ok().flatten();
        if let Some(days) = retention.and_then(|v| v.parse::<i64>().ok()) {
            let now = chrono::Utc::now().timestamp();
            let last_run = db::setting_parsed::<i64>(&conn, "retention_last_run", 0);
            if now - last_run >= 86_400 {
                match db::cleanup_old_articles(&conn, days) {
                    Ok(removed) => {
                        if removed > 0 {
                            log::info!("retention: removed {removed} old articles");
                        }
                        let _ =
                            db::set_setting(&conn, "retention_last_run", &now.to_string());
                    }
                    Err(e) => log::warn!("retention cleanup failed: {e}"),
                }
            }
        }
    }

    if let Some(p) = &progress {
        let _ = p.send(RefreshProgress::Finished {
            new_articles: total_new,
        });
    }
    let _ = app.emit("feeds-updated", total_new);

    notify::notify_new_articles(app, total_new).await;

    // Reconcile read/starred state with the sync server, if one is connected.
    // A sync mutates article state and may add feeds, so emit `feeds-updated`
    // again afterwards — the first emit fired before the sync touched the DB.
    match sync::run_if_connected(app).await {
        Ok(true) => {
            let _ = app.emit("feeds-updated", 0);
        }
        Ok(false) => {}
        Err(e) => log::warn!("sync failed: {e}"),
    }

    notify::update_badge(app).await;
    tray::refresh(app).await;
    Ok(total_new)
}

/// Poll every configured email-newsletter source over IMAP and ingest any new
/// messages as articles. Runs as part of `refresh_all` so newsletters refresh
/// on the same cadence as RSS feeds. Returns the total number of new articles.
///
/// A failure for one mailbox (bad credentials, server down) is recorded as the
/// feed's `fetch_error` and does not abort the others — the same resilience
/// the RSS path has.
async fn poll_newsletters(
    app: &AppHandle,
    dedup: bool,
    rules: &[crate::models::Rule],
) -> usize {
    let state = app.state::<AppState>();
    let sources = {
        let conn = state.db.lock().await;
        db::newsletter_sources_to_poll(&conn).unwrap_or_default()
    };
    if sources.is_empty() {
        return 0;
    }

    // Poll the mailboxes concurrently — each IMAP fetch is slow and fully
    // independent, so serialising them made a refresh stall for the sum of
    // every mailbox's fetch time. Bounded by a semaphore (mirroring the RSS
    // path) so a user with many newsletters does not open dozens of TLS
    // connections at once.
    let sem = Arc::new(Semaphore::new(4));
    let mut set: JoinSet<MailboxPoll> = JoinSet::new();
    for (feed_id, cfg) in sources {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await;
            // `imap` is a blocking crate with no per-operation timeout: a
            // server that completes the TCP/TLS handshake but then stalls
            // mid-command would block this task forever, holding its permit
            // and never letting `join_next` complete. Bound the whole fetch
            // with a wall-clock timeout so a hung mailbox degrades to a
            // per-feed error instead of wedging the refresh.
            let fetched = match tokio::time::timeout(
                Duration::from_secs(NEWSLETTER_POLL_TIMEOUT_SECS),
                tokio::task::spawn_blocking(move || newsletter::fetch_recent(&cfg, 50)),
            )
            .await
            {
                Ok(joined) => joined
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string())),
                Err(_) => Err(format!(
                    "IMAP poll timed out after {NEWSLETTER_POLL_TIMEOUT_SECS}s"
                )),
            };
            (feed_id, fetched)
        });
    }

    let mut total_new = 0usize;
    while let Some(joined) = set.join_next().await {
        let Ok((feed_id, fetched)) = joined else {
            continue;
        };
        match fetched {
            Ok(messages) => {
                // Parse the RFC822 bytes into articles *before* taking the DB
                // lock — `email_to_article` sanitizes HTML and is CPU-bound, so
                // doing it inside the locked scope would starve concurrent UI
                // queries (the same hazard the RSS path avoids with chunking).
                let articles: Vec<_> = messages
                    .iter()
                    .filter_map(|raw| newsletter::email_to_article(raw))
                    .map(|p| p.article)
                    .collect();
                total_new +=
                    upsert_articles(&state.db, feed_id, &articles, dedup, rules, "newsletter")
                        .await;
                let conn = state.db.lock().await;
                let _ = db::touch_feed(&conn, feed_id);
            }
            Err(e) => {
                log::warn!("newsletter poll failed (feed {feed_id}): {e}");
                let conn = state.db.lock().await;
                let _ = db::set_feed_error(&conn, feed_id, &e);
            }
        }
    }
    total_new
}

/// Read the refresh interval (minutes) from settings, defaulting to 30.
///
/// Clamped to `[5, 525_600]` (5 minutes … one year — the "auto-refresh off"
/// sentinel the Settings panel writes): the lower bound matches the slider
/// minimum, the upper one keeps `mins * 60` well clear of `u64` overflow even
/// if a corrupt out-of-range value is stored.
async fn refresh_interval_minutes(app: &AppHandle) -> u64 {
    let state = app.state::<AppState>();
    let conn = state.db.lock().await;
    db::get_setting(&conn, "refresh_interval_min")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|m| *m >= 5)
        .map(|m| m.min(525_600))
        .unwrap_or(30)
}

/// Longest single `sleep` the scheduler will take between two wake-ups. The
/// configured interval can be hours (or the ~1-year "off" sentinel), but
/// sleeping that whole span in one call freezes the *next* refresh on the
/// interval that was current when the sleep began: shortening the interval —
/// or toggling auto-refresh back on — would not take effect until the old,
/// possibly year-long, sleep finally elapsed. So the wait is sliced: the loop
/// re-reads the interval at most this often and refreshes as soon as enough
/// total time has accumulated, making an interval change land within one slice.
const SCHEDULER_SLICE: Duration = Duration::from_secs(60);

/// Spawn the background refresh loop. The app must stay resident (tray) for
/// this to run — macOS does not execute the process after the app is quit.
pub fn spawn_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            if let Err(e) = refresh_all(&app, None, false).await {
                log::warn!("scheduled refresh failed: {e}");
            }
            // Wait for the configured interval, but in short slices so a
            // changed interval is picked up promptly instead of only after
            // the previous (stale) interval's full sleep would have ended.
            let mut waited = Duration::ZERO;
            loop {
                let target = Duration::from_secs(refresh_interval_minutes(&app).await * 60);
                if waited >= target {
                    break;
                }
                let slice = (target - waited).min(SCHEDULER_SLICE);
                tokio::time::sleep(slice).await;
                waited += slice;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::SCHEDULER_SLICE;
    use std::time::Duration;

    /// Mirror of the sliced-wait loop in `spawn_scheduler`: returns how long
    /// the scheduler actually sleeps when the interval reported on each slice
    /// boundary is given by `interval_at`. Pure, so the wait policy is
    /// testable without the Tauri runtime or a real clock.
    fn waited_until_due(interval_at: impl Fn(Duration) -> Duration) -> Duration {
        let mut waited = Duration::ZERO;
        loop {
            let target = interval_at(waited);
            if waited >= target {
                return waited;
            }
            let slice = (target - waited).min(SCHEDULER_SLICE);
            waited += slice;
        }
    }

    #[test]
    fn waits_the_full_interval_when_it_is_stable() {
        // A steady 30-minute interval is honoured exactly.
        let thirty_min = Duration::from_secs(30 * 60);
        assert_eq!(waited_until_due(|_| thirty_min), thirty_min);
    }

    #[test]
    fn a_shortened_interval_is_picked_up_within_one_slice() {
        // The loop starts waiting on a 2-hour interval, then the interval is
        // cut to 5 minutes. The refresh must fire shortly after 5 minutes —
        // not hours later — so the wait lands within one slice of the new
        // interval, never on the stale 2-hour figure.
        let five_min = Duration::from_secs(5 * 60);
        let two_hours = Duration::from_secs(2 * 60 * 60);
        let waited = waited_until_due(|elapsed| {
            if elapsed >= five_min {
                five_min
            } else {
                two_hours
            }
        });
        assert!(waited >= five_min, "fired before the new interval: {waited:?}");
        assert!(
            waited < five_min + SCHEDULER_SLICE,
            "stale interval delayed the refresh: {waited:?}",
        );
    }

    #[test]
    fn re_enabling_after_the_off_sentinel_does_not_strand_for_a_year() {
        // Auto-refresh is off (the ~1-year sentinel), then re-enabled to 15
        // minutes mid-wait. The refresh must still fire near 15 minutes.
        let fifteen_min = Duration::from_secs(15 * 60);
        let one_year = Duration::from_secs(525_600 * 60);
        let waited = waited_until_due(|elapsed| {
            if elapsed >= fifteen_min {
                fifteen_min
            } else {
                one_year
            }
        });
        assert!(waited >= fifteen_min);
        assert!(waited < fifteen_min + SCHEDULER_SLICE);
    }
}
