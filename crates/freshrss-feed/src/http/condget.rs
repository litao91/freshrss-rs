//! Conditional GET: the headers that let a server answer "unchanged" without sending the body.

/// The `ETag` and `Last-Modified` values to replay on the next fetch.
///
/// Stored per feed as attributes, because they are properties of the remote document rather than of
/// the subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalHeaders {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl ConditionalHeaders {
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    /// Header values to send, as `(name, value)` pairs.
    ///
    /// A weak ETag (`W/"…"`) is sent back unchanged: the spec allows it for `If-None-Match`, and
    /// rewriting it would turn a match into a miss.
    pub fn as_headers(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::with_capacity(2);
        if let Some(etag) = &self.etag {
            out.push(("if-none-match", etag.as_str()));
        }
        if let Some(last_modified) = &self.last_modified {
            out.push(("if-modified-since", last_modified.as_str()));
        }
        out
    }

    /// Builds the pair from a response, keeping only values worth replaying.
    ///
    /// An `ETag` of `*` and an empty `Last-Modified` are dropped: replaying either can make a server
    /// answer 304 forever, which silently freezes a feed.
    pub fn from_response(etag: Option<String>, last_modified: Option<String>) -> Self {
        Self {
            etag: etag
                .map(|e| e.trim().to_owned())
                .filter(|e| !e.is_empty() && e != "*"),
            last_modified: last_modified
                .map(|m| m.trim().to_owned())
                .filter(|m| !m.is_empty()),
        }
    }
}

/// The 304 answer.
///
/// A 304 may still carry a refreshed `ETag`, and ignoring it means the next request replays a stale
/// validator that the server has already rotated — turning a cheap 304 into a full body transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotModified {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let c = ConditionalHeaders::default();
        assert!(c.is_empty());
        assert!(c.as_headers().is_empty());
    }

    #[test]
    fn both_headers_are_sent_when_both_are_known() {
        let c = ConditionalHeaders {
            etag: Some("\"abc\"".to_owned()),
            last_modified: Some("Mon, 02 Jan 2006 15:04:05 GMT".to_owned()),
        };
        assert_eq!(
            c.as_headers(),
            vec![
                ("if-none-match", "\"abc\""),
                ("if-modified-since", "Mon, 02 Jan 2006 15:04:05 GMT")
            ]
        );
    }

    #[test]
    fn a_weak_etag_is_replayed_unchanged() {
        let c = ConditionalHeaders::from_response(Some("W/\"abc\"".to_owned()), None);
        assert_eq!(c.etag.as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn a_wildcard_or_blank_etag_is_dropped() {
        // Replaying `*` matches every representation, so the server would answer 304 forever and the
        // feed would silently stop updating.
        assert!(ConditionalHeaders::from_response(Some("*".to_owned()), None).is_empty());
        assert!(ConditionalHeaders::from_response(Some("   ".to_owned()), None).is_empty());
        assert!(ConditionalHeaders::from_response(Some(String::new()), None).is_empty());
    }

    #[test]
    fn a_blank_last_modified_is_dropped() {
        assert!(ConditionalHeaders::from_response(None, Some("  ".to_owned())).is_empty());
        assert!(ConditionalHeaders::from_response(None, Some(String::new())).is_empty());
    }

    #[test]
    fn values_are_trimmed() {
        let c = ConditionalHeaders::from_response(
            Some("  \"abc\"  ".to_owned()),
            Some("  Mon, 02 Jan 2006 15:04:05 GMT ".to_owned()),
        );
        assert_eq!(c.etag.as_deref(), Some("\"abc\""));
        assert_eq!(c.last_modified.as_deref(), Some("Mon, 02 Jan 2006 15:04:05 GMT"));
    }

    #[test]
    fn a_304_can_carry_a_refreshed_validator() {
        let n = NotModified {
            etag: Some("\"new\"".to_owned()),
            last_modified: None,
        };
        assert_eq!(n.etag.as_deref(), Some("\"new\""));
    }
}
