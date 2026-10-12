//! Errors from feed acquisition.
//!
//! [`FeedError::Http`] carries the status code because the refresh pipeline branches on it:
//! HTTP 410 mutes the feed (matching FreshRSS's `feedController` behaviour), while 429/503 feed
//! the `Retry-After` gate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FeedError {
    /// A non-2xx HTTP response. `status` is 0 when the request never completed.
    #[error("http {status}: {message}")]
    Http { status: u16, message: String },

    /// HTTP 410 Gone — the feed is permanently retired, so it gets muted.
    #[error("feed is gone (http 410)")]
    Gone,

    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),

    #[error("timed out after {secs}s")]
    Timeout { secs: u64 },

    #[error("response body exceeded the {max} byte limit")]
    TooLarge { max: usize },

    #[error("the response could not be parsed as a feed: {0}")]
    Parse(String),

    #[error("no feed could be discovered at {0}")]
    NotDiscovered(String),

    #[error("feed kind {0} is not supported by this build")]
    UnsupportedKind(i16),

    #[error("the feed contained no items")]
    Empty,

    #[error("refusing to fetch {0}: it does not resolve to a remote address")]
    NonRemote(String),

    #[error("retry-after is active for this host until {until}")]
    RetryAfter { until: i64 },

    #[error("invalid url {url:?}: {reason}")]
    BadUrl { url: String, reason: String },

    #[error("invalid feed configuration: {0}")]
    BadConfig(String),

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

impl FeedError {
    /// The HTTP status behind this error, when there is one.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            FeedError::Http { status, .. } if *status > 0 => Some(*status),
            FeedError::Gone => Some(410),
            FeedError::Timeout { .. } => Some(408),
            _ => None,
        }
    }

    /// True for failures worth retrying later rather than treating as permanent.
    pub fn is_transient(&self) -> bool {
        match self {
            FeedError::Transport(_) | FeedError::Timeout { .. } | FeedError::RetryAfter { .. } => {
                true
            }
            FeedError::Http { status, .. } => {
                *status == 0 || *status == 408 || *status == 429 || *status >= 500
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gone_reports_410() {
        assert_eq!(FeedError::Gone.http_status(), Some(410));
        assert!(!FeedError::Gone.is_transient(), "410 is permanent");
    }

    #[test]
    fn statuses_map_through() {
        let e = FeedError::Http {
            status: 404,
            message: "not found".into(),
        };
        assert_eq!(e.http_status(), Some(404));
        assert!(!e.is_transient());

        let e = FeedError::Http {
            status: 503,
            message: "unavailable".into(),
        };
        assert!(e.is_transient());

        let e = FeedError::Http {
            status: 429,
            message: "rate limited".into(),
        };
        assert!(e.is_transient());
    }

    #[test]
    fn transport_errors_are_transient() {
        assert!(FeedError::Timeout { secs: 20 }.is_transient());
        assert_eq!(FeedError::Timeout { secs: 20 }.http_status(), Some(408));
        assert!(!FeedError::Empty.is_transient());
        assert!(!FeedError::Parse("bad xml".into()).is_transient());
    }
}
