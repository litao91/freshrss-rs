//! Feed kinds: how the bytes at a feed URL should be interpreted.
//!
//! The numeric codes are FreshRSS's and are stored verbatim in the `feed.kind` column, so an
//! OPML export from either project can be read by the other. `0` and `2` both mean "an XML
//! syndication document"; `2` records that the document was malformed enough that the parser had
//! to be forced, which only changes error reporting.

use serde::{Deserialize, Serialize};

/// Interpretation strategy for a feed URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    /// A well-formed RSS, Atom or JSON Feed document, detected from its content.
    #[default]
    Rss = 0,
    /// An RSS/Atom document that failed strict parsing and was re-parsed leniently.
    RssForced = 2,
    /// HTML page scraped with CSS-style XPath expressions.
    HtmlXpath = 10,
    /// XML (non-syndication) document scraped with XPath.
    XmlXpath = 15,
    /// JSON document scraped with XPath after being converted to an XML tree.
    JsonXpath = 20,
    /// JSON Feed 1.x, read through the standard dot-notation map.
    JsonFeed = 25,
    /// JSON document scraped with `a.b[0].c` dot-notation paths.
    JsonDotNotation = 30,
    /// JSON embedded inside an HTML page: XPath finds the JSON, dot-notation reads it.
    HtmlXpathJsonDotNotation = 35,
}

impl FeedKind {
    /// Every code the database may legitimately hold.
    pub const ALL: [FeedKind; 8] = [
        FeedKind::Rss,
        FeedKind::RssForced,
        FeedKind::HtmlXpath,
        FeedKind::XmlXpath,
        FeedKind::JsonXpath,
        FeedKind::JsonFeed,
        FeedKind::JsonDotNotation,
        FeedKind::HtmlXpathJsonDotNotation,
    ];

    pub const fn as_i16(self) -> i16 {
        self as i16
    }

    /// Unknown codes fall back to [`FeedKind::Rss`] rather than failing: a feed row written by a
    /// newer version must still render, and the parser sniffs the real format anyway.
    pub fn from_i16(code: i16) -> Self {
        match code {
            2 => FeedKind::RssForced,
            10 => FeedKind::HtmlXpath,
            15 => FeedKind::XmlXpath,
            20 => FeedKind::JsonXpath,
            25 => FeedKind::JsonFeed,
            30 => FeedKind::JsonDotNotation,
            35 => FeedKind::HtmlXpathJsonDotNotation,
            _ => FeedKind::Rss,
        }
    }

    /// True when the response body must be parsed as HTML rather than as a feed document.
    pub const fn is_html(self) -> bool {
        matches!(
            self,
            FeedKind::HtmlXpath | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the document is JSON at some level, including JSON buried in an HTML page.
    pub const fn is_json(self) -> bool {
        matches!(
            self,
            FeedKind::JsonFeed
                | FeedKind::JsonXpath
                | FeedKind::JsonDotNotation
                | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the kind requires a user-supplied XPath map to yield anything.
    pub const fn uses_xpath(self) -> bool {
        matches!(
            self,
            FeedKind::HtmlXpath
                | FeedKind::XmlXpath
                | FeedKind::JsonXpath
                | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the kind reads items through dot-notation paths.
    pub const fn uses_dot_notation(self) -> bool {
        matches!(
            self,
            FeedKind::JsonFeed | FeedKind::JsonDotNotation | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// The `Accept` header to send, mirroring FreshRSS's per-kind negotiation.
    ///
    /// HTML scrapers must not be offered XML: many servers answer `Accept: application/xml` with
    /// a redirect to a syndication endpoint, which silently changes what the XPath matches.
    pub const fn http_accept(self) -> &'static str {
        match self {
            FeedKind::Rss | FeedKind::RssForced => {
                "application/rss+xml, application/rdf+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.1"
            }
            FeedKind::XmlXpath => {
                "application/xml, text/xml, application/rss+xml;q=0.9, application/atom+xml;q=0.9, */*;q=0.1"
            }
            FeedKind::JsonFeed | FeedKind::JsonDotNotation | FeedKind::JsonXpath => {
                "application/json, */*;q=0.1"
            }
            FeedKind::HtmlXpath | FeedKind::HtmlXpathJsonDotNotation => {
                "text/html, application/xhtml+xml, */*;q=0.1"
            }
        }
    }
}

/// The keys of the XPath selector map, in the order they are written.
///
/// These are the names the subscription form uses and the names RSS-Bridge uses, which is why a
/// bridge's expressions can be pasted straight into a feed's configuration.
///
/// `feedTitle` is the one key PHP FreshRSS's OPML exporter omits: it writes ten and reads ten, so a
/// feed-title expression does not survive a round trip there. It is included here because losing it
/// silently renames a feed, and a PHP FreshRSS reading our file ignores an attribute it does not
/// know.
pub const XPATH_KEYS: &[&str] = &[
    "feedTitle",
    "item",
    "itemTitle",
    "itemContent",
    "itemUri",
    "itemAuthor",
    "itemTimestamp",
    "itemTimeFormat",
    "itemThumbnail",
    "itemCategories",
    "itemUid",
];

/// The keys of the JSON dot-notation map.
///
/// A superset of [`XPATH_KEYS`]: the JSON scrapers also read a feed image with a fallback, an HTML
/// body alongside the text one, and attachments. [`json_feed_dot_notation`] is this vocabulary
/// filled in for a standard JSON Feed document.
pub const JSON_KEYS: &[&str] = &[
    "feedTitle",
    "feedImage",
    "feedImageFallback",
    "item",
    "itemTitle",
    "itemContent",
    "itemContentHTML",
    "itemUri",
    "itemTimestamp",
    "itemTimeFormat",
    "itemThumbnail",
    "itemCategories",
    "itemUid",
    "itemAttachment",
    "itemAttachmentUrl",
    "itemAttachmentType",
    "itemAttachmentLength",
];

/// The dot-notation map that lets a standard JSON Feed be read by the generic JSON scraper.
///
/// FreshRSS expresses JSON Feed support this way rather than with a dedicated parser, which buys
/// one code path for every JSON shape. Keeping the same map means the same expressions work here.
pub fn json_feed_dot_notation() -> Vec<(&'static str, &'static str)> {
    vec![
        ("feedTitle", "title"),
        ("feedImage", "icon"),
        ("feedImageFallback", "favicon"),
        ("item", "items"),
        ("itemTitle", "title"),
        ("itemContent", "content_text"),
        ("itemContentHTML", "content_html"),
        ("itemUri", "url"),
        ("itemTimestamp", "date_published"),
        ("itemTimeFormat", "Y-m-d\\TH:i:s.vP"),
        ("itemThumbnail", "image"),
        ("itemCategories", "tags"),
        ("itemUid", "id"),
        ("itemAttachment", "attachments"),
        ("itemAttachmentUrl", "url"),
        ("itemAttachmentType", "mime_type"),
        ("itemAttachmentLength", "size_in_bytes"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for kind in FeedKind::ALL {
            assert_eq!(FeedKind::from_i16(kind.as_i16()), kind, "{kind:?}");
        }
    }

    #[test]
    fn unknown_codes_degrade_to_rss() {
        assert_eq!(FeedKind::from_i16(99), FeedKind::Rss);
        assert_eq!(FeedKind::from_i16(-1), FeedKind::Rss);
    }

    #[test]
    fn only_html_kinds_offer_html() {
        for kind in FeedKind::ALL {
            let accept = kind.http_accept();
            assert_eq!(
                accept.contains("text/html"),
                kind.is_html(),
                "{kind:?} accept={accept}"
            );
        }
    }

    #[test]
    fn json_feed_is_both_json_and_dot_notation() {
        assert!(FeedKind::JsonFeed.is_json());
        assert!(FeedKind::JsonFeed.uses_dot_notation());
        assert!(!FeedKind::JsonFeed.uses_xpath());
        assert!(!FeedKind::JsonFeed.is_html());
    }

    #[test]
    fn kind_35_needs_every_scraper() {
        let kind = FeedKind::HtmlXpathJsonDotNotation;
        assert!(kind.is_html() && kind.is_json() && kind.uses_xpath() && kind.uses_dot_notation());
    }

    #[test]
    fn default_is_rss() {
        assert_eq!(FeedKind::default(), FeedKind::Rss);
        assert_eq!(FeedKind::default().as_i16(), 0);
    }

    #[test]
    fn serializes_as_a_name_but_stores_as_a_code() {
        assert_eq!(
            serde_json::to_string(&FeedKind::HtmlXpath).unwrap(),
            "\"html_xpath\""
        );
    }

    #[test]
    fn the_two_vocabularies_share_every_field_but_the_author() {
        // A configuration can be moved between the two scrapers without losing a field, except the
        // per-item author: JSON Feed puts authors in an `authors` array rather than a scalar, and
        // FreshRSS's standard dot-notation map has no key for it, so this port does not invent one.
        for key in XPATH_KEYS {
            if *key == "itemAuthor" {
                assert!(!JSON_KEYS.contains(key));
            } else {
                assert!(
                    JSON_KEYS.contains(key),
                    "{key} is missing from the JSON map"
                );
            }
        }
        assert_eq!(XPATH_KEYS.first(), Some(&"feedTitle"));
        assert_eq!(XPATH_KEYS[1], "item");
    }

    #[test]
    fn the_dot_notation_map_only_uses_known_keys() {
        for (key, _) in json_feed_dot_notation() {
            assert!(
                JSON_KEYS.contains(&key),
                "{key} is not part of the vocabulary"
            );
        }
    }

    #[test]
    fn json_feed_map_covers_the_documented_keys() {
        let map: std::collections::BTreeMap<_, _> = json_feed_dot_notation().into_iter().collect();
        assert_eq!(map["item"], "items");
        assert_eq!(map["itemUid"], "id");
        assert_eq!(map["itemContentHTML"], "content_html");
        assert_eq!(map["itemAttachmentLength"], "size_in_bytes");
    }
}
