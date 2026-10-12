//! Feed kinds — FreshRSS's `feed.kind` column.
//!
//! The numeric values are part of the on-disk format and of OPML round-tripping
//! (`frss:kind`), so they must not be renumbered.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "i16", into = "i16")]
pub enum FeedKind {
    /// RSS/Atom, autodiscovery off at refresh time.
    Rss,
    /// "Invalid RSS or Atom feed": parsed as a feed regardless of content type. Corresponds to
    /// FreshRSS's `#force_feed` URL suffix.
    RssForced,
    /// An HTML page scraped with a user-supplied XPath map.
    HtmlXpath,
    /// An XML document scraped with a user-supplied XPath map.
    XmlXpath,
    /// Legacy JSON-with-XPath. FreshRSS no longer dispatches this separately; it falls through to
    /// the RSS path. Kept so existing rows and OPML files still load.
    JsonXpath,
    /// Standard JSON Feed 1.x.
    JsonFeed,
    /// Arbitrary JSON addressed by a dot-notation map.
    JsonDotNotation,
    /// JSON embedded in an HTML page: an XPath expression extracts it, then dot-notation maps it.
    HtmlXpathJsonDotNotation,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("{0} is not a valid feed kind")]
pub struct UnknownFeedKind(pub i16);

impl FeedKind {
    pub const fn as_i16(self) -> i16 {
        match self {
            FeedKind::Rss => 0,
            FeedKind::RssForced => 2,
            FeedKind::HtmlXpath => 10,
            FeedKind::XmlXpath => 15,
            FeedKind::JsonXpath => 20,
            FeedKind::JsonFeed => 25,
            FeedKind::JsonDotNotation => 30,
            FeedKind::HtmlXpathJsonDotNotation => 35,
        }
    }

    pub const fn from_i16(value: i16) -> Result<Self, UnknownFeedKind> {
        Ok(match value {
            0 => FeedKind::Rss,
            2 => FeedKind::RssForced,
            10 => FeedKind::HtmlXpath,
            15 => FeedKind::XmlXpath,
            20 => FeedKind::JsonXpath,
            25 => FeedKind::JsonFeed,
            30 => FeedKind::JsonDotNotation,
            35 => FeedKind::HtmlXpathJsonDotNotation,
            other => return Err(UnknownFeedKind(other)),
        })
    }

    /// True when the feed is scraped from a page or API rather than parsed as a syndication feed.
    pub const fn is_scraped(self) -> bool {
        matches!(
            self,
            FeedKind::HtmlXpath
                | FeedKind::XmlXpath
                | FeedKind::JsonXpath
                | FeedKind::JsonDotNotation
                | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the feed needs the `xpath` attribute map.
    pub const fn uses_xpath(self) -> bool {
        matches!(
            self,
            FeedKind::HtmlXpath | FeedKind::XmlXpath | FeedKind::JsonXpath | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the feed needs a dot-notation map (`json_dotnotation`, or the built-in JSON Feed
    /// mapping).
    pub const fn uses_dot_notation(self) -> bool {
        matches!(
            self,
            FeedKind::JsonFeed | FeedKind::JsonDotNotation | FeedKind::HtmlXpathJsonDotNotation
        )
    }

    /// True when the response is HTML rather than a syndication format.
    pub const fn is_html(self) -> bool {
        matches!(self, FeedKind::HtmlXpath | FeedKind::HtmlXpathJsonDotNotation)
    }

    /// The `Accept` header FreshRSS sends for this kind, mirroring `FreshRSS_http_Util::httpGet`.
    pub const fn accept_header(self) -> &'static str {
        match self {
            FeedKind::Rss | FeedKind::RssForced => {
                "application/atom+xml, application/rss+xml, application/rdf+xml;q=0.9, \
                 application/xml;q=0.8, text/xml;q=0.8, text/html;q=0.7, \
                 unknown/unknown;q=0.1, application/unknown;q=0.1, */*;q=0.1"
            }
            FeedKind::HtmlXpath | FeedKind::HtmlXpathJsonDotNotation => {
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
            }
            FeedKind::XmlXpath | FeedKind::JsonXpath => {
                "application/xml,application/xhtml+xml,text/xml;q=0.9,*/*;q=0.8"
            }
            FeedKind::JsonFeed | FeedKind::JsonDotNotation => {
                "application/json,application/feed+json,application/javascript;q=0.9,\
                 text/javascript;q=0.8,*/*;q=0.7"
            }
        }
    }

    /// Whether the content type should be trusted when deciding if a response is a feed.
    /// False for `RssForced`, which is the whole point of that kind.
    pub const fn trust_content_type(self) -> bool {
        !matches!(self, FeedKind::RssForced)
    }
}

impl Default for FeedKind {
    fn default() -> Self {
        FeedKind::Rss
    }
}

impl TryFrom<i16> for FeedKind {
    type Error = UnknownFeedKind;

    fn try_from(value: i16) -> Result<Self, Self::Error> {
        Self::from_i16(value)
    }
}

impl From<FeedKind> for i16 {
    fn from(kind: FeedKind) -> Self {
        kind.as_i16()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [FeedKind; 8] = [
        FeedKind::Rss,
        FeedKind::RssForced,
        FeedKind::HtmlXpath,
        FeedKind::XmlXpath,
        FeedKind::JsonXpath,
        FeedKind::JsonFeed,
        FeedKind::JsonDotNotation,
        FeedKind::HtmlXpathJsonDotNotation,
    ];

    #[test]
    fn numeric_values_match_freshrss_and_round_trip() {
        let expected = [0i16, 2, 10, 15, 20, 25, 30, 35];
        for (kind, value) in ALL.iter().zip(expected) {
            assert_eq!(kind.as_i16(), value, "{kind:?} must keep its on-disk value");
            assert_eq!(FeedKind::from_i16(value).unwrap(), *kind);
        }
    }

    #[test]
    fn unknown_values_are_rejected() {
        assert_eq!(FeedKind::from_i16(1), Err(UnknownFeedKind(1)));
        assert_eq!(FeedKind::from_i16(99), Err(UnknownFeedKind(99)));
        assert_eq!(FeedKind::from_i16(-1), Err(UnknownFeedKind(-1)));
    }

    #[test]
    fn default_is_plain_rss_matching_the_schema() {
        assert_eq!(FeedKind::default(), FeedKind::Rss);
        assert_eq!(FeedKind::default().as_i16(), 0);
    }

    #[test]
    fn scraped_kinds_are_identified() {
        assert!(!FeedKind::Rss.is_scraped());
        assert!(!FeedKind::RssForced.is_scraped());
        assert!(!FeedKind::JsonFeed.is_scraped());
        for kind in [
            FeedKind::HtmlXpath,
            FeedKind::XmlXpath,
            FeedKind::JsonXpath,
            FeedKind::JsonDotNotation,
            FeedKind::HtmlXpathJsonDotNotation,
        ] {
            assert!(kind.is_scraped(), "{kind:?} should be scraped");
        }
    }

    #[test]
    fn xpath_and_dot_notation_requirements() {
        assert!(FeedKind::HtmlXpath.uses_xpath());
        assert!(FeedKind::XmlXpath.uses_xpath());
        assert!(FeedKind::HtmlXpathJsonDotNotation.uses_xpath());
        assert!(!FeedKind::JsonDotNotation.uses_xpath());

        assert!(FeedKind::JsonFeed.uses_dot_notation());
        assert!(FeedKind::JsonDotNotation.uses_dot_notation());
        assert!(FeedKind::HtmlXpathJsonDotNotation.uses_dot_notation());
        assert!(!FeedKind::HtmlXpath.uses_dot_notation());
    }

    #[test]
    fn json_in_html_is_both_html_and_xpath_and_dot_notation() {
        let kind = FeedKind::HtmlXpathJsonDotNotation;
        assert!(kind.is_html());
        assert!(kind.uses_xpath());
        assert!(kind.uses_dot_notation());
    }

    #[test]
    fn only_the_html_kinds_are_html() {
        assert!(FeedKind::HtmlXpath.is_html());
        assert!(FeedKind::HtmlXpathJsonDotNotation.is_html());
        assert!(!FeedKind::XmlXpath.is_html());
        assert!(!FeedKind::JsonFeed.is_html());
    }

    #[test]
    fn every_kind_has_an_accept_header() {
        for kind in ALL {
            assert!(!kind.accept_header().is_empty(), "{kind:?} needs an Accept header");
        }
        // The JSON kinds must prefer JSON.
        assert!(FeedKind::JsonFeed.accept_header().starts_with("application/json"));
        assert!(FeedKind::Rss.accept_header().contains("application/rss+xml"));
    }

    #[test]
    fn only_rss_forced_ignores_the_content_type() {
        assert!(!FeedKind::RssForced.trust_content_type());
        for kind in ALL.iter().filter(|k| **k != FeedKind::RssForced) {
            assert!(kind.trust_content_type(), "{kind:?} should trust the content type");
        }
    }

    #[test]
    fn serializes_as_the_numeric_value_for_api_and_opml_stability() {
        assert_eq!(serde_json::to_string(&FeedKind::HtmlXpath).unwrap(), "10");
        let back: FeedKind = serde_json::from_str("25").unwrap();
        assert_eq!(back, FeedKind::JsonFeed);
        assert!(serde_json::from_str::<FeedKind>("7").is_err());
    }
}
