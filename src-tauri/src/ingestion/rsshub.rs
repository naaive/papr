//! RSSHub support: the `rsshub://` URL namespace.
//!
//! [RSSHub](https://docs.rsshub.app) generates feeds for sites that ship none.
//! Following the convention other readers use, a subscription can be written
//! `rsshub://<route>` — e.g. `rsshub://github/issue/DIYgod/RSSHub` — and stored
//! in that instance-independent form. It is expanded to a concrete URL against
//! the user's configured instance only at *fetch* time, so changing the
//! instance re-routes every RSSHub feed at once (and a self-hoster can point
//! the whole app at their own instance without re-subscribing).
//!
//! Everything here is a pure, network-free string transform so the routing
//! rules can be unit-tested without a DB or the network. Reading the configured
//! instance from settings lives in the callers (`add_feed`, the scheduler).

/// The setting key holding the user's chosen instance base URL.
pub const INSTANCE_SETTING: &str = "rsshub_instance";

/// The public instance, used when the user has not configured their own.
pub const DEFAULT_INSTANCE: &str = "https://rsshub.app";

/// The namespace scheme, including the `://` separator.
const SCHEME: &str = "rsshub://";

/// True when `url` is an `rsshub://…` namespace URL. The scheme match is
/// case-insensitive and tolerant of leading whitespace; `get(..)` keeps it
/// panic-free even when the first bytes are a multi-byte char boundary.
pub fn is_rsshub_url(url: &str) -> bool {
    let trimmed = url.trim_start();
    trimmed
        .get(..SCHEME.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SCHEME))
}

/// Normalize a user-entered instance base URL: trim it, fall back to the public
/// instance when blank, add `https://` when no scheme was typed, and drop any
/// trailing slash so [`expand`] can join a route with a single separator.
pub fn normalize_instance(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_INSTANCE.to_string();
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// The portion of an `rsshub://` URL after the scheme, with surrounding
/// whitespace and leading slashes stripped. Caller must have checked
/// [`is_rsshub_url`]; the prefix check there guarantees the byte slice is valid.
fn route_of(url: &str) -> &str {
    url.trim_start()[SCHEME.len()..]
        .trim()
        .trim_start_matches('/')
}

/// Canonical stored form of an `rsshub://` URL: lowercase scheme, no leading
/// slashes on the route, surrounding whitespace trimmed. Keeps the DB free of
/// trivially-different duplicates of the same route. A non-RSSHub URL is only
/// trimmed.
pub fn canonical(url: &str) -> String {
    if !is_rsshub_url(url) {
        return url.trim().to_string();
    }
    format!("{SCHEME}{}", route_of(url))
}

/// Expand an `rsshub://<route>` URL into a concrete fetchable URL against
/// `instance` (which is normalized first, so the raw stored setting may be
/// passed). A non-RSSHub URL is returned unchanged, so this is safe to apply to
/// every feed URL on the fetch path.
pub fn expand(url: &str, instance: &str) -> String {
    if !is_rsshub_url(url) {
        return url.to_string();
    }
    format!("{}/{}", normalize_instance(instance), route_of(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_scheme() {
        assert!(is_rsshub_url("rsshub://github/issue/DIYgod/RSSHub"));
        assert!(is_rsshub_url("RSSHUB://github/repos/DIYgod"));
        assert!(is_rsshub_url("  rsshub://twitter/user/x"));
        assert!(!is_rsshub_url("https://rsshub.app/github/issue/DIYgod/RSSHub"));
        assert!(!is_rsshub_url("rsshub:/github")); // missing the second slash
        assert!(!is_rsshub_url(""));
        assert!(!is_rsshub_url("rss"));
    }

    #[test]
    fn normalizes_instances() {
        assert_eq!(normalize_instance(""), DEFAULT_INSTANCE);
        assert_eq!(normalize_instance("   "), DEFAULT_INSTANCE);
        assert_eq!(normalize_instance("https://rsshub.app"), "https://rsshub.app");
        // Trailing slash dropped.
        assert_eq!(normalize_instance("https://rsshub.app/"), "https://rsshub.app");
        // Bare host gains a scheme.
        assert_eq!(normalize_instance("my.rsshub.io"), "https://my.rsshub.io");
        // A sub-path instance is preserved (minus the trailing slash).
        assert_eq!(
            normalize_instance("https://host.example/rsshub/"),
            "https://host.example/rsshub"
        );
    }

    #[test]
    fn expands_against_the_default_instance() {
        assert_eq!(
            expand("rsshub://github/issue/DIYgod/RSSHub", DEFAULT_INSTANCE),
            "https://rsshub.app/github/issue/DIYgod/RSSHub"
        );
    }

    #[test]
    fn expands_against_a_custom_instance() {
        assert_eq!(
            expand("rsshub://twitter/user/durov", "https://my.host/"),
            "https://my.host/twitter/user/durov"
        );
        // A bare-host instance is given a scheme first.
        assert_eq!(
            expand("rsshub://twitter/user/durov", "my.host"),
            "https://my.host/twitter/user/durov"
        );
    }

    #[test]
    fn expand_preserves_query_strings() {
        assert_eq!(
            expand("rsshub://twitter/user/durov?mode=fulltext", DEFAULT_INSTANCE),
            "https://rsshub.app/twitter/user/durov?mode=fulltext"
        );
    }

    #[test]
    fn expand_tolerates_extra_leading_slashes_and_whitespace() {
        assert_eq!(
            expand("  rsshub:///github/repos/DIYgod  ", DEFAULT_INSTANCE),
            "https://rsshub.app/github/repos/DIYgod"
        );
    }

    #[test]
    fn expand_leaves_plain_urls_untouched() {
        let url = "https://example.com/feed.xml";
        assert_eq!(expand(url, DEFAULT_INSTANCE), url);
    }

    #[test]
    fn canonicalizes_the_stored_form() {
        // Scheme lowercased, leading slashes and whitespace stripped.
        assert_eq!(
            canonical("RSSHUB:///github/issue/DIYgod/RSSHub  "),
            "rsshub://github/issue/DIYgod/RSSHub"
        );
        // Already canonical — unchanged.
        assert_eq!(
            canonical("rsshub://twitter/user/x"),
            "rsshub://twitter/user/x"
        );
        // Non-RSSHub URL is only trimmed.
        assert_eq!(
            canonical("  https://example.com/feed  "),
            "https://example.com/feed"
        );
    }

    #[test]
    fn canonical_then_expand_round_trips() {
        let raw = "  RSSHUB:///github/issue/DIYgod/RSSHub  ";
        let stored = canonical(raw);
        assert_eq!(stored, "rsshub://github/issue/DIYgod/RSSHub");
        assert_eq!(
            expand(&stored, DEFAULT_INSTANCE),
            "https://rsshub.app/github/issue/DIYgod/RSSHub"
        );
    }
}
