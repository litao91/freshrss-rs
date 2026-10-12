//! The `entry` row — one article.
//!
//! Two serialization formats here are load-bearing and must stay in sync with
//! `freshrss_feed::hash`, because the change-detection hash is computed over exactly these
//! strings:
//!
//! * `tags` is stored as `"#tag1 #tag2"`. The `#tag` search operator relies on that shape
//!   (`TRIM(tags) || ' #' LIKE '%foo #%'` matches whole tags rather than substrings).
//! * `author` is stored as `"name1; name2"`.
//!
//! FreshRSS prefixes the author string with a stray `;` (`';' . implode('; ', …)`). We do not:
//! the leading separator exists only to make its `explode` round-trip unambiguous, and dropping it
//! makes the column readable in a `sqlite3` session. Since clean-text storage already breaks
//! byte-compatibility with a PHP FreshRSS database, the hash is recomputed on import anyway.

use serde::{Deserialize, Serialize};

use crate::domain::attributes::{keys, Attributes};
use crate::domain::ids::EntryId;

/// When an article has no usable title, FreshRSS falls back to the first N characters of its
/// stripped content (`MAX_CHARS_EMPTY_FEED_TITLE`).
pub const TITLE_FALLBACK_CHARS: usize = 75;

/// Wraps scraped full text so it can be removed again before hashing.
pub use freshrss_feed::hash::{FULLCONTENT_END, FULLCONTENT_START};

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: EntryId,
    pub id_user: i64,
    pub id_feed: i64,
    /// Feed-level identity. Unique per feed, not globally: the same article syndicated to two
    /// feeds is two entries.
    pub guid: String,
    pub title: String,
    pub authors: Vec<String>,
    /// Sanitized HTML.
    pub content: String,
    pub link: String,
    /// Publication time as reported by the feed, in unix seconds. Falls back to the ingestion time
    /// when a feed supplies none, so this is always usable for sorting and display.
    pub date: i64,
    /// Ingestion time of the most recent refresh that still saw this article upstream. Retention
    /// works off this column, not `date`.
    pub last_seen: i64,
    /// Set when a refresh found the article's content had changed.
    pub last_modified: Option<i64>,
    /// Set when the user marked it read or starred it.
    pub last_user_modified: Option<i64>,
    /// MD5 over the content-bearing fields, used to detect upstream edits. Excludes `date`
    /// deliberately — a feed that republishes with a new timestamp but identical text must not
    /// look like an edit.
    pub hash: Option<[u8; 16]>,
    pub is_read: bool,
    pub is_favorite: bool,
    /// Feed-provided keywords. Distinct from user labels, which live in `entrytag`.
    pub tags: Vec<String>,
    pub attributes: Attributes,
}

/// An article's attachments, from the `enclosures` attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Enclosure {
    pub url: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
}

/// A feed-provided thumbnail, from the `thumbnail` attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

const IMAGE_EXTENSIONS: [&str; 7] = [".avif", ".gif", ".jpeg", ".jpg", ".png", ".svg", ".webp"];

impl Enclosure {
    /// Image if the feed says so, or if it says nothing and the URL looks like one. Mirrors
    /// `FreshRSS_Entry::enclosureIsImage`, including the extension check on the path before any
    /// `?` or `#`.
    pub fn is_image(&self) -> bool {
        if self.medium.as_deref() == Some("image") {
            return true;
        }
        if let Some(mime) = self.mime.as_deref() {
            if mime.to_ascii_lowercase().starts_with("image/") {
                return true;
            }
        }
        // Extension sniffing only applies when the feed told us nothing at all.
        if self.mime.is_some() || self.length.is_some() {
            return false;
        }
        let path = self
            .url
            .split(['?', '#'])
            .next()
            .unwrap_or(&self.url)
            .to_ascii_lowercase();
        IMAGE_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
    }

    pub fn is_audio(&self) -> bool {
        self.medium.as_deref() == Some("audio")
            || self
                .mime
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().starts_with("audio/"))
    }

    pub fn is_video(&self) -> bool {
        self.medium.as_deref() == Some("video")
            || self
                .mime
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().starts_with("video/"))
    }
}

impl Entry {
    pub fn new(id_user: i64, id_feed: i64, guid: impl Into<String>) -> Self {
        Self {
            id: EntryId::default(),
            id_user,
            id_feed,
            guid: guid.into(),
            title: String::new(),
            authors: Vec::new(),
            content: String::new(),
            link: String::new(),
            date: 0,
            last_seen: 0,
            last_modified: None,
            last_user_modified: None,
            hash: None,
            is_read: false,
            is_favorite: false,
            tags: Vec::new(),
            attributes: Attributes::new(),
        }
    }

    // --- column serialization ----------------------------------------------

    /// The `author` column value.
    ///
    /// Delegated to the feed crate because this exact string is also the hash input; a second
    /// definition here would let the column and the digest drift apart.
    pub fn author_to_db(&self) -> String {
        freshrss_feed::hash::authors_field(&self.authors)
    }

    /// Parses the `author` column. FreshRSS splits on `;` when present and on `,` otherwise,
    /// because feeds variously supply one or the other.
    pub fn authors_from_db(raw: &str) -> Vec<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let separator: &[char] = if trimmed.contains(';') {
            &[';']
        } else {
            &[',']
        };
        trimmed
            .split(separator)
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The `tags` column value: `"#tag1 #tag2"`, or empty when there are none.
    pub fn tags_to_db(&self) -> String {
        freshrss_feed::hash::tags_field(&self.tags)
    }

    /// Parses the `tags` column. Accepts `#` and `,` as separators, matching
    /// `FreshRSS_Entry::_tags` — some feeds put every keyword in one comma-separated category.
    pub fn tags_from_db(raw: &str) -> Vec<String> {
        raw.split(['#', ','])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    }

    // --- hashing inputs ----------------------------------------------------

    /// The content used when computing `hash`: the pre-scrape original when full-text extraction
    /// replaced the body, otherwise the content with any scrape wrapper removed.
    ///
    /// Without this, turning on `pathEntries` would make every article look edited on the next
    /// refresh and clear the user's read state.
    pub fn content_for_hash(&self) -> String {
        freshrss_feed::hash::original_content(
            &self.content,
            self.attributes.get_str(keys::ORIGINAL_CONTENT),
        )
        .into_owned()
    }

    // --- attribute-backed accessors ----------------------------------------

    pub fn thumbnail(&self) -> Option<Thumbnail> {
        self.attributes.get_typed::<Thumbnail>(keys::THUMBNAIL)
    }

    pub fn set_thumbnail(&mut self, thumbnail: Option<Thumbnail>) {
        match thumbnail {
            Some(t) => {
                let _ = self.attributes.set_typed(keys::THUMBNAIL, &t);
            }
            None => {
                self.attributes.remove(keys::THUMBNAIL);
            }
        }
    }

    pub fn enclosures(&self) -> Vec<Enclosure> {
        self.attributes
            .get_typed::<Vec<Enclosure>>(keys::ENCLOSURES)
            .unwrap_or_default()
    }

    /// The first playable attachment, for `section.rd-player`.
    pub fn playable_enclosure(&self) -> Option<Enclosure> {
        self.enclosures()
            .into_iter()
            .find(|e| e.is_audio() || e.is_video())
    }

    pub fn has_enclosures(&self) -> bool {
        self.attributes
            .get(keys::ENCLOSURES)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    }

    // --- display helpers ---------------------------------------------------

    /// The title to show, applying FreshRSS's fallbacks:
    ///
    /// 1. a real title wins;
    /// 2. no title at all → the GUID, which at least identifies the article;
    /// 3. no title *and* no GUID → the first [`TITLE_FALLBACK_CHARS`] characters of the stripped
    ///    content, with an ellipsis when truncated.
    pub fn display_title(&self) -> String {
        let trimmed = self.title.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
        if !self.guid.trim().is_empty() {
            return self.guid.clone();
        }
        let text: String = strip_html_tags(&self.content)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            return String::new();
        }
        let truncated: String = text.chars().take(TITLE_FALLBACK_CHARS).collect();
        if truncated.chars().count() < text.chars().count() {
            format!("{truncated}…")
        } else {
            truncated
        }
    }

    /// Plaintext for the `.rd-item-summary` line: HTML stripped and whitespace collapsed.
    pub fn summary(&self, max_chars: usize) -> String {
        let collapsed: String = strip_html_tags(&self.content)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if collapsed.chars().count() <= max_chars {
            return collapsed;
        }
        // Cut on a character boundary, then back off to the last space so the summary does not end
        // mid-word — but only when that does not throw away most of the budget.
        let cut: String = collapsed.chars().take(max_chars).collect();
        match cut.rfind(' ') {
            Some(idx) if idx > max_chars / 2 => format!("{}…", &cut[..idx]),
            _ => format!("{cut}…"),
        }
    }
}

/// Strips tags and decodes entities, for previews and title fallbacks.
///
/// Not a sanitizer — the output is plain text and callers must not treat it as HTML.
///
/// The full entity decode (rather than [`freshrss_feed::html_only_entity_decode`], which preserves
/// `&amp;` and friends) is correct *here* because this is display text: an article body holding
/// `Tom &amp; Jerry` must render as `Tom & Jerry`. On ingest the opposite holds, which is why the
/// two functions exist.
pub fn strip_html_tags(html: &str) -> String {
    let stripped = freshrss_feed::text::strip_tags(html);
    html_escape::decode_html_entities(&stripped).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        let mut e = Entry::new(1, 2, "guid-1");
        e.title = "A title".into();
        e.link = "https://example.com/a".into();
        e.content = "<p>Hello world</p>".into();
        e
    }

    fn base_enclosure() -> Enclosure {
        Enclosure {
            url: String::new(),
            mime: None,
            medium: None,
            length: None,
            title: None,
            description: None,
            height: None,
            width: None,
        }
    }

    #[test]
    fn tags_serialize_with_the_hash_prefix_the_search_operator_expects() {
        let mut e = entry();
        assert_eq!(
            e.tags_to_db(),
            "",
            "no tags stores an empty string, not '#'"
        );
        e.tags = vec!["rust".into(), "rss".into()];
        assert_eq!(e.tags_to_db(), "#rust #rss");
    }

    #[test]
    fn tags_parse_from_hashes_and_commas() {
        assert_eq!(Entry::tags_from_db("#rust #rss"), vec!["rust", "rss"]);
        assert_eq!(Entry::tags_from_db("rust,rss"), vec!["rust", "rss"]);
        assert_eq!(Entry::tags_from_db("#a, #b ,c"), vec!["a", "b", "c"]);
        assert!(Entry::tags_from_db("").is_empty());
        assert!(Entry::tags_from_db("#").is_empty());
        assert!(Entry::tags_from_db("  ").is_empty());
    }

    #[test]
    fn tags_round_trip() {
        let mut e = entry();
        e.tags = vec!["alpha".into(), "beta gamma".into()];
        let stored = e.tags_to_db();
        assert_eq!(Entry::tags_from_db(&stored), e.tags);
    }

    #[test]
    fn authors_serialize_without_freshrss_stray_leading_separator() {
        let mut e = entry();
        assert_eq!(e.author_to_db(), "");
        e.authors = vec!["Ada".into()];
        assert_eq!(e.author_to_db(), "Ada");
        e.authors = vec!["Ada".into(), "Grace".into()];
        assert_eq!(e.author_to_db(), "Ada; Grace");
    }

    #[test]
    fn authors_parse_from_semicolons_or_commas() {
        assert_eq!(Entry::authors_from_db("Ada; Grace"), vec!["Ada", "Grace"]);
        assert_eq!(Entry::authors_from_db("Ada, Grace"), vec!["Ada", "Grace"]);
        // A single comma-containing author is split — the same wart FreshRSS has.
        assert_eq!(Entry::authors_from_db("Smith, Jr."), vec!["Smith", "Jr."]);
        assert!(Entry::authors_from_db("").is_empty());
        assert!(Entry::authors_from_db(";").is_empty());
    }

    #[test]
    fn authors_round_trip() {
        let mut e = entry();
        e.authors = vec!["Ada Lovelace".into(), "Grace Hopper".into()];
        let stored = e.author_to_db();
        assert_eq!(Entry::authors_from_db(&stored), e.authors);
    }

    #[test]
    fn content_for_hash_prefers_the_stored_original() {
        let mut e = entry();
        e.content = format!("{FULLCONTENT_START}<p>scraped</p>{FULLCONTENT_END}");
        // With no original_content attribute, the wrapper is stripped.
        assert_eq!(e.content_for_hash(), "");

        e.attributes
            .set(keys::ORIGINAL_CONTENT, "<p>from the feed</p>");
        assert_eq!(e.content_for_hash(), "<p>from the feed</p>");
    }

    #[test]
    fn fullcontent_markers_are_removed_but_surrounding_text_is_kept() {
        let mut e = entry();
        e.content = format!("teaser{FULLCONTENT_START}scraped body{FULLCONTENT_END}tail");
        assert_eq!(e.content_for_hash(), "teasertail");
    }

    #[test]
    fn an_unterminated_marker_leaves_the_content_alone() {
        let mut e = entry();
        e.content = format!("teaser{FULLCONTENT_START}never closed");
        // PHP's `preg_replace` finds no match without a closing marker, so nothing is removed.
        assert_eq!(e.content_for_hash(), e.content);
    }

    #[test]
    fn content_without_markers_is_untouched() {
        assert_eq!(entry().content_for_hash(), "<p>Hello world</p>");
    }

    #[test]
    fn display_title_prefers_the_real_title() {
        assert_eq!(entry().display_title(), "A title");
        let mut e = entry();
        e.title = "  padded  ".into();
        assert_eq!(e.display_title(), "padded");
    }

    #[test]
    fn display_title_falls_back_to_the_guid() {
        let mut e = entry();
        e.title = String::new();
        assert_eq!(e.display_title(), "guid-1");
    }

    #[test]
    fn display_title_synthesizes_from_content_when_there_is_no_guid() {
        let mut e = entry();
        e.title = String::new();
        e.guid = String::new();
        e.content = "<p>Some body text</p>".into();
        assert_eq!(e.display_title(), "Some body text");
    }

    #[test]
    fn display_title_truncates_the_content_fallback_with_an_ellipsis() {
        let mut e = entry();
        e.title = String::new();
        e.guid = String::new();
        e.content = format!("<p>{}</p>", "x".repeat(200));
        let title = e.display_title();
        assert!(title.ends_with('…'), "{title}");
        // 75 content characters plus the ellipsis.
        assert_eq!(title.chars().count(), TITLE_FALLBACK_CHARS + 1);
    }

    #[test]
    fn display_title_is_empty_when_there_is_nothing_to_show() {
        let mut e = entry();
        e.title = String::new();
        e.guid = String::new();
        e.content = String::new();
        assert_eq!(e.display_title(), "");
    }

    #[test]
    fn strip_html_tags_decodes_entities() {
        assert_eq!(strip_html_tags("<p>Hello <b>world</b></p>"), "Hello world");
        assert_eq!(strip_html_tags("Caf&eacute; &amp; bar"), "Café & bar");
        assert_eq!(strip_html_tags("no tags"), "no tags");
        // A script body is not article text, so it goes with the tags.
        assert_eq!(strip_html_tags("<script>evil()</script>text"), "text");
        assert_eq!(strip_html_tags("a < b"), "a < b");
        assert_eq!(strip_html_tags(r#"<a title=">">link</a>"#), "link");
    }

    #[test]
    fn summary_collapses_whitespace_and_truncates_at_a_word_boundary() {
        let mut e = entry();
        e.content = "<p>one   two\n\nthree   four five six</p>".into();
        assert_eq!(e.summary(1000), "one two three four five six");

        e.content = format!("<p>{}</p>", "word ".repeat(50));
        let s = e.summary(30);
        assert!(s.ends_with('…'), "{s}");
        assert!(s.chars().count() <= 32, "{s} is too long");
        assert!(
            !s[..s.len() - '…'.len_utf8()].ends_with(' '),
            "must not end in a gap"
        );
    }

    #[test]
    fn summary_of_empty_content_is_empty() {
        let mut e = entry();
        e.content = String::new();
        assert_eq!(e.summary(200), "");
    }

    #[test]
    fn enclosure_image_detection_uses_medium_then_mime_then_extension() {
        let by_medium = Enclosure {
            url: "https://e.com/x".into(),
            medium: Some("image".into()),
            ..base_enclosure()
        };
        assert!(by_medium.is_image());

        let by_mime = Enclosure {
            url: "https://e.com/x".into(),
            mime: Some("IMAGE/PNG".into()),
            ..base_enclosure()
        };
        assert!(by_mime.is_image(), "mime comparison is case-insensitive");

        let by_ext = Enclosure {
            url: "https://e.com/photo.jpeg?size=large#frag".into(),
            ..base_enclosure()
        };
        assert!(
            by_ext.is_image(),
            "extension check ignores query and fragment"
        );

        let not_image = Enclosure {
            url: "https://e.com/episode.mp3".into(),
            mime: Some("audio/mpeg".into()),
            ..base_enclosure()
        };
        assert!(!not_image.is_image());
    }

    #[test]
    fn enclosure_extension_inference_needs_both_mime_and_length_absent() {
        // With a length present, a bare `.png` URL is treated as a download, not an inline image,
        // matching FreshRSS's conservative rule.
        let with_length = Enclosure {
            url: "https://e.com/photo.png".into(),
            length: Some(1234),
            ..base_enclosure()
        };
        assert!(!with_length.is_image());

        let webp = Enclosure {
            url: "https://e.com/photo.webp".into(),
            ..base_enclosure()
        };
        assert!(webp.is_image());
    }

    #[test]
    fn audio_and_video_enclosures_are_classified() {
        let audio = Enclosure {
            url: "https://e.com/e.mp3".into(),
            mime: Some("audio/mpeg".into()),
            ..base_enclosure()
        };
        assert!(audio.is_audio());
        assert!(!audio.is_video());

        let video = Enclosure {
            url: "https://e.com/e.mp4".into(),
            medium: Some("video".into()),
            ..base_enclosure()
        };
        assert!(video.is_video());
        assert!(!video.is_audio());
    }

    #[test]
    fn playable_enclosure_picks_the_first_media_attachment() {
        let mut e = entry();
        assert!(e.playable_enclosure().is_none());
        assert!(!e.has_enclosures());

        e.attributes
            .set_typed(
                keys::ENCLOSURES,
                &vec![
                    Enclosure {
                        url: "https://e.com/photo.png".into(),
                        mime: Some("image/png".into()),
                        ..base_enclosure()
                    },
                    Enclosure {
                        url: "https://e.com/ep.mp3".into(),
                        mime: Some("audio/mpeg".into()),
                        ..base_enclosure()
                    },
                ],
            )
            .expect("serializable");

        assert_eq!(
            e.playable_enclosure().map(|x| x.url).as_deref(),
            Some("https://e.com/ep.mp3")
        );
        assert!(e.has_enclosures());
    }

    #[test]
    fn thumbnail_round_trips_through_attributes() {
        let mut e = entry();
        assert_eq!(e.thumbnail(), None);
        e.set_thumbnail(Some(Thumbnail {
            url: "https://e.com/t.jpg".into(),
            width: Some(640),
            height: None,
            time: None,
        }));
        let back = e.thumbnail().expect("stored");
        assert_eq!(back.url, "https://e.com/t.jpg");
        assert_eq!(back.width, Some(640));
        assert_eq!(back.height, None);

        e.set_thumbnail(None);
        assert_eq!(e.thumbnail(), None);
        assert!(!e.attributes.contains(keys::THUMBNAIL));
    }
}
