//! The intermediate representation every parser produces and every consumer reads.
//!
//! Having one IR is what lets RSS 2.0, Atom 1.0, RDF, JSON Feed and the XPath/dot-notation
//! scraping kinds all share a single GUID policy, sanitizer and reconcile loop instead of five
//! near-copies.
//!
//! The important detail is that fields the *feed* may or may not supply are `Option`, not
//! defaulted. `RawItem::guid == None` means "this feed gave no id", which is the input
//! [`crate::guid::decide_guid`] needs in order to run its fallback chain. Collapsing that to
//! `String::new()` at parse time would silently change which articles are treated as new on every
//! refresh — the most visible bug a feed reader can have.

use serde::{Deserialize, Serialize};

/// One article, as the feed described it. Nothing here is sanitized or resolved yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawItem {
    /// The feed's own identifier, if it supplied one.
    ///
    /// `None` means absent — distinct from `Some("")`. RSS `<guid>`, Atom `<id>`, JSON Feed `id`,
    /// or a scraping kind's `itemUid` expression all land here.
    pub guid: Option<String>,
    /// Whether the RSS `<guid>` carried `isPermaLink="true"` (or omitted it, which means true).
    /// A permalink guid doubles as the article's URL; a non-permalink one must not.
    pub guid_is_permalink: bool,
    pub title: Option<String>,
    /// The article's own URL, before resolution against [`RawFeed::base_url`].
    pub link: Option<String>,
    /// Publication time in unix seconds. RSS `pubDate`, Atom `published` (then `updated`),
    /// JSON Feed `date_published`.
    pub date: Option<i64>,
    /// Last-modified time in unix seconds. Atom `updated`, JSON Feed `date_modified`.
    pub updated: Option<i64>,
    /// The full body when the feed provides one (`content:encoded`, Atom `content`,
    /// `content_html`).
    pub content_html: Option<String>,
    /// A summary or plain-text body (`description`, Atom `summary`, `content_text`, `summary`).
    /// Used when there is no full body, and as the source of the list pane's preview.
    pub content_text: Option<String>,
    pub authors: Vec<String>,
    pub enclosures: Vec<RawEnclosure>,
    /// Feed-provided keywords. Distinct from user labels, which do not exist yet at this stage.
    pub tags: Vec<String>,
    pub thumbnail: Option<RawThumbnail>,
}

impl RawItem {
    /// True when there is nothing worth storing: no title, no body and no link.
    ///
    /// Feeds sometimes emit an empty trailing `<item>`; keeping it produces a blank article in the
    /// list with no way to open it.
    pub fn is_empty(&self) -> bool {
        self.title.as_deref().unwrap_or("").trim().is_empty()
            && self.content_html.as_deref().unwrap_or("").trim().is_empty()
            && self.content_text.as_deref().unwrap_or("").trim().is_empty()
            && self.link.as_deref().unwrap_or("").trim().is_empty()
    }

    /// The body to store, preferring the full content over the summary.
    ///
    /// Mirrors SimplePie's `get_content()` precedence: `content:encoded` / Atom `content` /
    /// JSON Feed `content_html` win, and only then does the description or summary get used.
    pub fn effective_content(&self) -> Option<&str> {
        // Each candidate is filtered on its own: a whitespace-only `<content:encoded>` must fall
        // through to `<description>` rather than blanking the article.
        fn nonblank(s: &Option<String>) -> Option<&str> {
            s.as_deref().filter(|c| !c.trim().is_empty())
        }
        nonblank(&self.content_html).or_else(|| nonblank(&self.content_text))
    }
}

/// An attachment. Field names follow RSS 2.0's `<enclosure>` plus the Media RSS attributes that
/// most podcast and video feeds actually use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawEnclosure {
    pub url: String,
    /// MIME type, lowercased.
    pub mime: Option<String>,
    /// Media RSS `medium`: `image`, `audio`, `video`, `document`, `executable`.
    pub medium: Option<String>,
    pub length: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub height: Option<i64>,
    pub width: Option<i64>,
}

/// A `media:thumbnail`, kept separate from enclosures because feeds use it for the list preview
/// rather than as a downloadable attachment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawThumbnail {
    pub url: String,
    pub height: Option<i64>,
    pub width: Option<i64>,
    /// Media RSS `time`, for thumbnails attached at a point in a video.
    pub time: Option<String>,
}

/// A whole feed, as parsed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RawFeed {
    pub title: Option<String>,
    /// The URL to resolve relative links against: `<base href>`, `xml:base`, or the request URL.
    pub base_url: Option<String>,
    /// The channel's own link — the human-facing site, not the feed URL.
    pub website: Option<String>,
    pub description: Option<String>,
    /// The feed's icon (`<image><url>`, Atom `icon`/`logo`, JSON Feed `icon`/`favicon`).
    pub icon: Option<String>,
    /// Atom `<link rel="self">`, used for WebSub and for detecting a moved feed.
    pub self_url: Option<String>,
    /// Atom `<link rel="hub">`, the WebSub hub.
    pub hub_url: Option<String>,
    /// The URL the feed was ultimately fetched from, after redirects. HTTP 301 makes this differ
    /// from the subscribed URL, and the subscription is then updated to match.
    pub effective_url: Option<String>,
    /// Items in **document order**, not chronological.
    ///
    /// Feeds are conventionally newest-first, so the refresh pipeline reverses this when it needs
    /// chronological order for id allocation.
    pub items: Vec<RawItem>,
    /// The feed format that produced this, for diagnostics.
    pub format: FeedFormat,
}

/// Which parser produced a [`RawFeed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedFormat {
    #[default]
    Unknown,
    Rss2,
    Rss1,
    Atom,
    JsonFeed,
    /// Produced by an XPath or dot-notation scraping kind.
    Scraped,
}

impl FeedFormat {
    /// Items in chronological (oldest-first) order, which is what id allocation needs.
    ///
    /// Document order is newest-first by convention, but not by guarantee, so this reverses rather
    /// than sorting by date: a feed with missing or bogus dates would otherwise be reordered in a
    /// way that makes article ids non-monotonic.
    pub fn chronological(items: &[RawItem]) -> Vec<&RawItem> {
        items.iter().rev().collect()
    }
}

impl RawFeed {
    /// Items in chronological order.
    pub fn chronological(&self) -> Vec<&RawItem> {
        FeedFormat::chronological(&self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> RawItem {
        RawItem {
            guid: Some("g1".into()),
            title: Some("Title".into()),
            link: Some("https://example.com/a".into()),
            date: Some(1_700_000_000),
            ..RawItem::default()
        }
    }

    #[test]
    fn a_missing_guid_is_distinguishable_from_an_empty_one() {
        // The whole reason `guid` is an Option: the fallback chain must only run when the feed
        // genuinely supplied nothing.
        assert_eq!(RawItem::default().guid, None);
        assert_eq!(
            RawItem {
                guid: Some(String::new()),
                ..RawItem::default()
            }
            .guid,
            Some(String::new())
        );
    }

    #[test]
    fn content_precedence_prefers_the_full_body() {
        let mut it = item();
        it.content_html = Some("<p>full</p>".into());
        it.content_text = Some("summary".into());
        assert_eq!(it.effective_content(), Some("<p>full</p>"));

        it.content_html = None;
        assert_eq!(it.effective_content(), Some("summary"));
    }

    #[test]
    fn whitespace_only_content_falls_through_to_the_summary() {
        let mut it = item();
        it.content_html = Some("   \n  ".into());
        it.content_text = Some("summary".into());
        assert_eq!(
            it.effective_content(),
            Some("summary"),
            "a body of only whitespace is not a body"
        );
    }

    #[test]
    fn content_is_none_when_there_is_nothing() {
        assert_eq!(item().effective_content(), None);
    }

    #[test]
    fn an_item_with_no_title_body_or_link_is_empty() {
        assert!(RawItem::default().is_empty());
        assert!(RawItem {
            title: Some("   ".into()),
            ..RawItem::default()
        }
        .is_empty());

        assert!(!item().is_empty(), "has a title and a link");
        assert!(
            !RawItem {
                content_text: Some("body".into()),
                ..RawItem::default()
            }
            .is_empty(),
            "a body alone is enough"
        );
        assert!(
            !RawItem {
                link: Some("https://e.com".into()),
                ..RawItem::default()
            }
            .is_empty(),
            "a link alone is enough"
        );
    }

    #[test]
    fn chronological_reverses_document_order() {
        let mut feed = RawFeed::default();
        for i in 0..3 {
            feed.items.push(RawItem {
                guid: Some(format!("g{i}")),
                // Newest first, the convention feeds actually follow.
                date: Some(1_700_000_000 - i),
                ..RawItem::default()
            });
        }
        let dates: Vec<i64> = feed
            .chronological()
            .into_iter()
            .map(|i| i.date.unwrap())
            .collect();
        assert_eq!(dates, vec![1_699_999_998, 1_699_999_999, 1_700_000_000]);
    }

    #[test]
    fn chronological_does_not_depend_on_dates_being_sane() {
        // Reversing rather than sorting means a feed with missing or duplicated dates still gets a
        // stable order, so article ids stay monotonic within a batch.
        let mut feed = RawFeed::default();
        for guid in ["a", "b", "c"] {
            feed.items.push(RawItem {
                guid: Some(guid.into()),
                date: None,
                ..RawItem::default()
            });
        }
        let guids: Vec<&str> = feed
            .chronological()
            .into_iter()
            .map(|i| i.guid.as_deref().unwrap())
            .collect();
        assert_eq!(guids, vec!["c", "b", "a"]);
    }

    #[test]
    fn an_empty_feed_chronologizes_to_nothing() {
        assert!(RawFeed::default().chronological().is_empty());
    }

    #[test]
    fn raw_items_survive_a_json_round_trip() {
        // The `parse --json` CLI path and the fixture corpus both depend on this.
        let it = item();
        let json = serde_json::to_string(&it).expect("serialize");
        assert_eq!(
            serde_json::from_str::<RawItem>(&json).expect("deserialize"),
            it
        );
    }
}
