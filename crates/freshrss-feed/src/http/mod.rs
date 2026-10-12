//! Fetching feed bodies over HTTP.
//!
//! The [`Fetcher`] trait is the seam that keeps the rest of the crate testable offline: the refresh
//! pipeline, discovery and the CLI all go through it, so a fixture-backed implementation can drive
//! the whole path without a network. `cargo test` never opens a socket.

pub mod bodyhash;
pub mod condget;
pub mod reqwest;

use std::time::Duration;

use crate::error::FeedError;

pub use bodyhash::body_hash;
pub use condget::{ConditionalHeaders, NotModified};

/// The default `User-Agent`.
///
/// Identifying the client is not politeness, it is function: a surprising number of publishers block
/// or serve degraded content to a blank UA, and a support request that starts "which reader are you
/// using" needs an answer in the server log.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "freshrss-rs/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/freshrss-rs; feed reader)"
);

/// A cap on how much of a response body is read.
///
/// Applied while streaming, not after: a hostile or misconfigured URL that returns an endless body
/// must not be able to exhaust memory before the limit is noticed.
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// What to fetch, and how.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    /// The `Accept` header, taken from [`crate::FeedKind::http_accept`].
    pub accept: String,
    pub user_agent: String,
    pub timeout: Duration,
    pub max_bytes: u64,
    /// Credentials for HTTP Basic auth, from the feed's `httpAuth` column.
    pub basic_auth: Option<(String, String)>,
    /// Per-feed opt-out of certificate verification, for a self-signed internal feed.
    pub ssl_verify: bool,
    pub proxy: Option<String>,
    /// Replayed conditional-GET headers.
    pub conditional: ConditionalHeaders,
}

impl FetchRequest {
    /// A request with the defaults filled in, for a plain unauthenticated fetch.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            accept: crate::FeedKind::Rss.http_accept().to_owned(),
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(30),
            max_bytes: DEFAULT_MAX_BYTES,
            basic_auth: None,
            ssl_verify: true,
            proxy: None,
            conditional: ConditionalHeaders::default(),
        }
    }

    /// Sets the `Accept` header from a feed kind.
    pub fn with_kind(mut self, kind: crate::FeedKind) -> Self {
        self.accept = kind.http_accept().to_owned();
        self
    }
}

impl Default for FetchRequest {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBody {
    pub body: Vec<u8>,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// The URL after redirects.
    ///
    /// Compared against the subscribed URL to detect a permanent move: an HTTP 301 means the
    /// subscription should be updated, or every future fetch pays for a redirect that will
    /// eventually stop being served.
    pub effective_url: String,
    /// [`body_hash`] of the body — the volatile-stripped fingerprint.
    pub hash: String,
    /// True when the server reported a permanent redirect.
    pub permanently_moved: bool,
}

impl FetchedBody {
    /// True when the body differs from the fingerprint stored on the feed.
    ///
    /// An empty stored hash means "never fetched", which is a change: the first refresh of a feed
    /// must always parse.
    pub fn changed_since(&self, stored_hash: Option<&str>) -> bool {
        stored_hash.is_none_or(|stored| stored.is_empty() || stored != self.hash)
    }
}

/// The outcome of a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// HTTP 304: the server says nothing has changed.
    ///
    /// Distinct from "changed but identical body", because a 304 means the article `lastSeen`
    /// timestamps should slide without touching any content.
    NotModified,
    Found(FetchedBody),
}

/// Fetches a feed body.
///
/// Implementations must map transport and HTTP failures onto [`FeedError`] rather than returning
/// their own error type, because the caller's response depends on the distinction: a 410 mutes the
/// feed, a timeout reschedules it, and a 500 leaves it alone.
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch(&self, request: &FetchRequest) -> Result<FetchOutcome, FeedError>;
}

/// Reads a `Retry-After` header value into a delay.
///
/// Both forms are legal: a number of seconds, or an HTTP date. The date form matters because rate
/// limiters on shared hosting commonly send it, and ignoring it means hammering a server that
/// explicitly told us when to come back.
pub fn parse_retry_after(value: &str, now_unix: i64) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(24 * 3600)));
    }
    let when = crate::parse::date::parse_date(trimmed)?;
    let delta = when.saturating_sub(now_unix);
    Some(Duration::from_secs((delta.max(0) as u64).min(24 * 3600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched(hash: &str) -> FetchedBody {
        FetchedBody {
            body: Vec::new(),
            status: 200,
            etag: None,
            last_modified: None,
            content_type: None,
            effective_url: "https://example.com/feed.xml".to_owned(),
            hash: hash.to_owned(),
            permanently_moved: false,
        }
    }

    #[test]
    fn a_never_fetched_feed_counts_as_changed() {
        let f = fetched("abc");
        assert!(f.changed_since(None));
        assert!(f.changed_since(Some("")), "an empty stored hash means never fetched");
    }

    #[test]
    fn an_identical_fingerprint_is_not_a_change() {
        assert!(!fetched("abc").changed_since(Some("abc")));
    }

    #[test]
    fn a_different_fingerprint_is_a_change() {
        assert!(fetched("abc").changed_since(Some("def")));
    }

    #[test]
    fn the_user_agent_names_the_client() {
        assert!(DEFAULT_USER_AGENT.contains("freshrss-rs/"));
        assert!(!DEFAULT_USER_AGENT.is_empty());
    }

    #[test]
    fn defaults_are_safe() {
        let r = FetchRequest::new("https://example.com/feed.xml");
        assert!(r.ssl_verify, "certificate verification is on unless a feed opts out");
        assert_eq!(r.max_bytes, DEFAULT_MAX_BYTES);
        assert!(r.basic_auth.is_none());
        assert!(r.conditional.is_empty());
        assert_eq!(r.accept, crate::FeedKind::Rss.http_accept());
    }

    #[test]
    fn the_accept_header_follows_the_feed_kind() {
        let r = FetchRequest::new("https://example.com/page").with_kind(crate::FeedKind::HtmlXpath);
        assert!(r.accept.starts_with("text/html"), "{}", r.accept);

        let r = FetchRequest::new("https://example.com/api").with_kind(crate::FeedKind::JsonFeed);
        assert!(r.accept.starts_with("application/json"), "{}", r.accept);
    }

    #[test]
    fn retry_after_accepts_seconds() {
        assert_eq!(parse_retry_after("120", 0), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  0  ", 0), Some(Duration::from_secs(0)));
    }

    #[test]
    fn retry_after_accepts_an_http_date() {
        let now = 1_136_214_245;
        let later = crate::parse::date::to_rfc822(now + 300);
        assert_eq!(parse_retry_after(&later, now), Some(Duration::from_secs(300)));
    }

    #[test]
    fn a_retry_after_in_the_past_is_zero_not_negative() {
        let now = 1_136_214_245;
        let earlier = crate::parse::date::to_rfc822(now - 600);
        assert_eq!(parse_retry_after(&earlier, now), Some(Duration::from_secs(0)));
    }

    #[test]
    fn retry_after_is_capped_so_a_hostile_header_cannot_stall_the_scheduler() {
        assert_eq!(
            parse_retry_after("999999999999", 0),
            Some(Duration::from_secs(24 * 3600))
        );
    }

    #[test]
    fn an_unparseable_retry_after_is_none() {
        assert_eq!(parse_retry_after("soon", 0), None);
        assert_eq!(parse_retry_after("", 0), None);
    }
}
