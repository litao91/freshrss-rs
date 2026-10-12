//! The OPML document model.
//!
//! Deliberately a flat, lossy-free picture of an `<outline>` tree: everything the import service cares
//! about has a field, and everything it does not is dropped at parse time rather than carried in a
//! catch-all map, because an OPML file from the wild is untrusted input and a catch-all is how
//! attacker-controlled strings reach a database.

use std::collections::BTreeMap;

use crate::kind::FeedKind;

pub use crate::kind::{JSON_KEYS, XPATH_KEYS};

/// The `frss:priority` words.
///
/// FreshRSS exports the default (main stream) as *no attribute at all*, so `None` in [`Outline`]
/// means "main stream" and `Some(Main)` only appears when a file spelled it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpmlPriority {
    Important,
    Main,
    Category,
    Feed,
    Hidden,
}

impl OpmlPriority {
    /// The exported spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            OpmlPriority::Important => "important",
            OpmlPriority::Main => "main",
            OpmlPriority::Category => "category",
            OpmlPriority::Feed => "feed",
            OpmlPriority::Hidden => "hidden",
        }
    }

    /// Parses case-insensitively; anything else is the default, matching the import service's
    /// `match (…) { … default: PRIORITY_MAIN_STREAM }`.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.to_ascii_lowercase().as_str() {
            "important" => OpmlPriority::Important,
            "main" => OpmlPriority::Main,
            "category" => OpmlPriority::Category,
            "feed" => OpmlPriority::Feed,
            "hidden" => OpmlPriority::Hidden,
            _ => return None,
        })
    }

    /// The numeric ladder the server stores. Kept here so the word-to-number mapping lives next to
    /// the words.
    pub fn as_i8(self) -> i8 {
        match self {
            OpmlPriority::Important => 20,
            OpmlPriority::Main => 10,
            OpmlPriority::Category => 0,
            OpmlPriority::Feed => -5,
            OpmlPriority::Hidden => -10,
        }
    }

    /// The word for a stored ladder value, or `None` when the value is the export default and should
    /// therefore be omitted.
    pub fn from_i8(value: i8) -> Option<Self> {
        Some(match value {
            20 => OpmlPriority::Important,
            10 => return None,
            0 => OpmlPriority::Category,
            -5 => OpmlPriority::Feed,
            -10 => OpmlPriority::Hidden,
            _ => return None,
        })
    }
}

/// The feed kind an OPML `type` word names.
///
/// Anything unrecognized means plain RSS, which is the import service's `default:` arm: a file from
/// a reader that writes `type="link"`, or no type at all, still imports.
pub fn kind_of_type(word: &str) -> FeedKind {
    match word.to_ascii_lowercase().as_str() {
        "html+xpath" => FeedKind::HtmlXpath,
        "xml+xpath" => FeedKind::XmlXpath,
        // `JSON+DotPath` is the 1.24.0-dev spelling of the same kind, still accepted on import.
        "json+dotnotation" | "json+dotpath" => FeedKind::JsonDotNotation,
        "jsonfeed" => FeedKind::JsonFeed,
        "html+xpath+json+dotnotation" => FeedKind::HtmlXpathJsonDotNotation,
        _ => FeedKind::Rss,
    }
}

/// The `type` word for a feed kind.
///
/// Two kinds have no word of their own and leave as `rss`, exactly as they do from PHP FreshRSS:
/// [`FeedKind::RssForced`] records that a past parse had to be lenient, which is not a property of
/// the subscription, and the export switch has no arm for [`FeedKind::JsonXpath`].
pub fn type_of_kind(kind: FeedKind) -> &'static str {
    match kind {
        FeedKind::HtmlXpath => "HTML+XPath",
        FeedKind::XmlXpath => "XML+XPath",
        FeedKind::JsonFeed => "JSONFeed",
        FeedKind::JsonDotNotation => "JSON+DotNotation",
        FeedKind::HtmlXpathJsonDotNotation => "HTML+XPath+JSON+DotNotation",
        FeedKind::Rss | FeedKind::RssForced | FeedKind::JsonXpath => "rss",
    }
}

/// One `<outline>`.
///
/// A folder is an outline with children and no `xml_url`; a feed is an outline with an `xml_url`.
/// The two are not mutually exclusive in the wild — a broken exporter can produce both — and the
/// model records what the file says rather than resolving the conflict, because which of the two an
/// importer should honour is a policy decision that belongs to the importer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outline {
    /// `text`, falling back to `title` on import. The display name.
    pub text: String,
    /// Import-only. The exporter writes the name as `text`, so this is never emitted; it exists
    /// because OPML 2.0 allows `title` as an alternative and other readers use it.
    pub title: Option<String>,
    /// Present exactly when this outline is a feed.
    pub xml_url: Option<String>,
    pub html_url: String,
    pub description: String,
    /// The standard OPML `category` attribute, comma-separated in the file.
    ///
    /// Import-only, and only meaningful on a top-level outline: readers that do not nest folders
    /// (Feedly, Inoreader) instead tag each feed with its folder names, and FreshRSS's importer
    /// uses the first outline carrying one as that feed's category. Nesting wins when both are
    /// present, which is why this is not written on export.
    pub category: Vec<String>,
    /// Only meaningful for feeds; folders carry the default.
    pub kind: FeedKind,
    /// `None` means the attribute was absent, i.e. the main-stream default.
    pub priority: Option<OpmlPriority>,
    /// Signed refresh interval in seconds; negative means muted. `None` is "not written", i.e. the
    /// reader's own default.
    pub ttl: Option<i32>,
    /// The GUID unicity policy, when it is not the default `id`.
    pub unicity_criteria: Option<String>,
    pub unicity_criteria_forced: bool,
    /// The `frss:xPathItem*` map, keyed without the prefix.
    pub xpath: BTreeMap<String, String>,
    /// The `frss:jsonItem*` map, keyed without the prefix.
    pub json_dotnotation: BTreeMap<String, String>,
    /// `frss:xPathToJson`, for the HTML-that-wraps-JSON kind.
    pub xpath_to_json: Option<String>,
    /// `frss:cssFullContent`: the CSS selectors for full-article extraction.
    pub path_entries: Option<String>,
    /// `frss:cssFullContentConditions`, one selector per line in the attribute.
    pub path_entries_conditions: Vec<String>,
    /// `frss:cssContentFilter`: selectors to strip from the extracted article.
    pub path_entries_filter: Option<String>,
    /// `frss:filtersActionRead`, one search per line in the attribute.
    pub filters_action_read: Vec<String>,
    pub children: Vec<Outline>,
}

impl Outline {
    /// True when this outline names a feed rather than a folder.
    pub fn is_feed(&self) -> bool {
        self.xml_url.is_some()
    }

    /// The display name, applying the import service's `text ?? title` fallback.
    pub fn name(&self) -> &str {
        if self.text.trim().is_empty() {
            self.title.as_deref().unwrap_or_default()
        } else {
            &self.text
        }
    }

    /// A folder outline.
    pub fn folder(text: impl Into<String>, children: Vec<Outline>) -> Self {
        Self {
            text: text.into(),
            children,
            ..Self::default()
        }
    }

    /// A feed outline with the fields every exporter writes.
    pub fn feed(text: impl Into<String>, xml_url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            xml_url: Some(xml_url.into()),
            ..Self::default()
        }
    }
}

/// A whole document.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Opml {
    pub title: String,
    /// `dateCreated`, kept as written: OPML dates are RFC 822 and nobody benefits from this crate
    /// owning a calendar.
    pub date_created: Option<String>,
    pub outlines: Vec<Outline>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_words_round_trip() {
        for p in [
            OpmlPriority::Important,
            OpmlPriority::Main,
            OpmlPriority::Category,
            OpmlPriority::Feed,
            OpmlPriority::Hidden,
        ] {
            assert_eq!(OpmlPriority::parse(p.as_str()), Some(p));
            assert_eq!(OpmlPriority::parse(&p.as_str().to_uppercase()), Some(p));
        }
        assert_eq!(OpmlPriority::parse("sideways"), None);
        assert_eq!(OpmlPriority::parse(""), None);
    }

    #[test]
    fn the_main_stream_priority_is_the_one_that_is_omitted() {
        // The export writes no attribute for the default, so the ladder value for it maps to None and
        // an absent attribute imports as the default.
        assert_eq!(OpmlPriority::from_i8(10), None);
        assert_eq!(OpmlPriority::from_i8(20), Some(OpmlPriority::Important));
        assert_eq!(OpmlPriority::from_i8(0), Some(OpmlPriority::Category));
        assert_eq!(OpmlPriority::from_i8(-5), Some(OpmlPriority::Feed));
        assert_eq!(OpmlPriority::from_i8(-10), Some(OpmlPriority::Hidden));
        // A value outside the ladder is not exported at all rather than as a wrong word.
        assert_eq!(OpmlPriority::from_i8(7), None);
    }

    #[test]
    fn the_words_match_the_ladder() {
        for (word, value) in [
            ("important", 20),
            ("main", 10),
            ("category", 0),
            ("feed", -5),
            ("hidden", -10),
        ] {
            assert_eq!(OpmlPriority::parse(word).map(|p| p.as_i8()), Some(value));
        }
    }

    #[test]
    fn an_outline_is_a_feed_exactly_when_it_has_a_url() {
        assert!(Outline::feed("a", "https://a.example/f.xml").is_feed());
        assert!(!Outline::folder("a", Vec::new()).is_feed());
    }

    #[test]
    fn the_name_falls_back_to_title() {
        let mut o = Outline::default();
        assert_eq!(o.name(), "");
        o.title = Some("From title".into());
        assert_eq!(o.name(), "From title");
        o.text = "From text".into();
        assert_eq!(o.name(), "From text", "text wins when both are present");
        o.text = "   ".into();
        assert_eq!(o.name(), "From title", "blank text is as absent as none");
    }
}
