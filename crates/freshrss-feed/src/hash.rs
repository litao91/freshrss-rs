//! Change detection: the MD5 that decides whether an article was *updated* or merely *seen again*.
//!
//! Port of `FreshRSS_Entry::hash()` (`app/Models/Entry.php`). Two properties are load-bearing and
//! both are asserted below:
//!
//! - **The publication date is excluded.** FreshRSS synthesizes a date when the feed omits one, so
//!   including it would make every article on such a feed look modified on every refresh.
//! - **The digest is over the article's substance, not its markup wrapper.** Scrape markers added by
//!   full-text extraction are stripped first (see [`original_content`]), so turning `pathEntries` on
//!   does not mark a whole backlog as updated.
//!
//! A hash that flips spuriously is worse than one that misses a change: with
//! `mark_updated_article_unread` enabled it re-unreads everything the user has already read.

use md5::{Digest as _, Md5};
use once_cell::sync::Lazy;
use serde::Serialize;

use crate::raw::RawEnclosure;

/// The `pathEntries` scrape wrapper. FreshRSS surrounds extracted full text with these markers so
/// the reader can tell "this is the whole article" from "this is what the feed gave us", and strips
/// them again before hashing.
pub const FULLCONTENT_START: &str = "<!-- FULLCONTENT start //-->";
pub const FULLCONTENT_END: &str = "<!-- FULLCONTENT end //-->";

/// `#<!-- FULLCONTENT start //-->.*<!-- FULLCONTENT end //-->#s` — greedy, dot-matches-newline.
static FULLCONTENT_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(&format!(
        r"(?s){}.*{}",
        regex::escape(FULLCONTENT_START),
        regex::escape(FULLCONTENT_END)
    ))
    .expect("static pattern")
});

/// Joins author names for both the `author` column and the hash.
///
/// FreshRSS uses one function for both (`Entry::authors(true)`), which is why this lives here rather
/// than next to the DAO: if the column format and the hash input ever drift apart, every article
/// changes hash on the next refresh. The separator is `"; "`, and — unlike FreshRSS — there is no
/// stray leading `';'`. That leading separator is an artifact of `';' . implode('; ', …)` that then
/// has to be parsed back out; dropping it makes the column readable and the split unambiguous.
#[must_use]
pub fn authors_field(authors: &[String]) -> String {
    authors.join("; ")
}

/// Joins feed keywords for both the `tags` column and the hash.
///
/// The `#`-prefixed, space-separated form is what makes `#tag` search syntax a substring match
/// rather than needing a join, which is why FreshRSS chose it and why it must not change.
#[must_use]
pub fn tags_field(tags: &[String]) -> String {
    if tags.is_empty() {
        String::new()
    } else {
        format!("#{}", tags.join(" #"))
    }
}

/// The body as the feed supplied it, undoing any full-text scrape wrapper.
///
/// `stored_original` is the entry's `original_content` attribute: when scraping replaced the body,
/// the pre-scrape text was saved there and is the correct hash input, because that is what the feed
/// itself will be compared against next time.
#[must_use]
pub fn original_content<'a>(
    content: &'a str,
    stored_original: Option<&'a str>,
) -> std::borrow::Cow<'a, str> {
    if let Some(original) = stored_original {
        return std::borrow::Cow::Borrowed(original);
    }
    if !content.contains(FULLCONTENT_START) {
        return std::borrow::Cow::Borrowed(content);
    }
    std::borrow::Cow::Owned(FULLCONTENT_RE.replace_all(content, "").into_owned())
}

/// The attachment identity that goes into the hash.
///
/// Deliberately a separate, frozen shape rather than serializing [`RawEnclosure`] directly. Adding a
/// field to `RawEnclosure` — a display hint, a scraped dimension — must not change every article's
/// hash and mark a whole backlog as updated. Only what *identifies* an attachment belongs here.
#[derive(Serialize)]
struct EnclosureHash<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    length: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    medium: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
}

/// Canonical JSON for the attachments, or `""` when there are none.
///
/// `serde_json` matches PHP's `JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE` by default, so URLs
/// and non-ASCII titles digest the same way in both projects. An empty list yields `""` rather than
/// `"[]"`, matching FreshRSS's `empty($enclosures) ? '' : …` and keeping the common case
/// byte-identical to an article that never had attachments.
#[must_use]
pub fn enclosures_field(enclosures: &[RawEnclosure]) -> String {
    if enclosures.is_empty() {
        return String::new();
    }
    let view: Vec<EnclosureHash<'_>> = enclosures
        .iter()
        .map(|e| EnclosureHash {
            url: &e.url,
            mime: e.mime.as_deref(),
            length: e.length,
            medium: e.medium.as_deref(),
            title: e.title.as_deref(),
        })
        .collect();
    serde_json::to_string(&view).unwrap_or_default()
}

/// Everything the digest reads. All fields are already-serialized strings so the caller controls the
/// exact column format; see [`authors_field`] and [`tags_field`] for the canonical ones.
#[derive(Debug, Clone, Default)]
pub struct HashInput<'a> {
    pub link: &'a str,
    pub title: &'a str,
    pub author: &'a str,
    /// [`original_content`]'s output, not the stored body.
    pub content: &'a str,
    pub tags: &'a str,
    /// [`enclosures_field`]'s output.
    pub enclosures: &'a str,
}

/// The raw 16-byte MD5, as stored in the `entry.hash BLOB(16)` column.
#[must_use]
pub fn content_hash(input: &HashInput<'_>) -> [u8; 16] {
    let mut hasher = Md5::new();
    // Order matters and is FreshRSS's: link, title, authors, original content, tags, attributes.
    hasher.update(input.link.as_bytes());
    hasher.update(input.title.as_bytes());
    hasher.update(input.author.as_bytes());
    hasher.update(input.content.as_bytes());
    hasher.update(input.tags.as_bytes());
    hasher.update(input.enclosures.as_bytes());
    hasher.finalize().into()
}

/// The lowercase hex form, for logs, the API and comparisons against a PHP-produced database.
#[must_use]
pub fn content_hash_hex(input: &HashInput<'_>) -> String {
    hex::encode(content_hash(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawItem;

    fn input() -> HashInput<'static> {
        HashInput {
            link: "https://example.com/a",
            title: "A title",
            author: "Ada; Grace",
            content: "<p>Body</p>",
            tags: "#rust #rss",
            enclosures: "",
        }
    }

    fn enclosure(url: &str) -> RawEnclosure {
        RawEnclosure {
            url: url.into(),
            ..RawEnclosure::default()
        }
    }

    #[test]
    fn the_digest_is_md5_of_the_fields_in_order() {
        let mut expected = Md5::new();
        expected.update(b"https://example.com/aA titleAda; Grace<p>Body</p>#rust #rss");
        assert_eq!(
            content_hash(&input()),
            <[u8; 16]>::from(expected.finalize())
        );
    }

    #[test]
    fn the_hash_is_16_bytes_and_hex_is_32_lowercase_chars() {
        let digest = content_hash(&input());
        assert_eq!(digest.len(), 16);
        let hexed = content_hash_hex(&input());
        assert_eq!(hexed.len(), 32);
        assert!(hexed
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(hexed, hex::encode(digest));
    }

    #[test]
    fn every_field_participates() {
        let base = content_hash_hex(&input());
        let enclosures = enclosures_field(&[enclosure("https://e/x.mp3")]);
        let variants = [
            HashInput {
                link: "https://example.com/b",
                ..input()
            },
            HashInput {
                title: "Another title",
                ..input()
            },
            HashInput {
                author: "Ada",
                ..input()
            },
            HashInput {
                content: "<p>Different</p>",
                ..input()
            },
            HashInput {
                tags: "#rust",
                ..input()
            },
            HashInput {
                enclosures: &enclosures,
                ..input()
            },
        ];
        for v in variants {
            assert_ne!(content_hash_hex(&v), base, "{v:?} must change the digest");
        }
    }

    #[test]
    fn the_digest_is_pinned_so_a_serializer_change_is_a_reviewed_event() {
        // Hardcoded on purpose. If field order, a separator or the enclosure JSON ever changes,
        // every article in every deployment silently looks updated on the next refresh — and with
        // `mark_updated_article_unread` on, everything the user has read becomes unread again.
        assert_eq!(
            content_hash_hex(&input()),
            "d6bd7148c6c5eb8fcd4ee93544605fba"
        );
    }

    #[test]
    fn the_hash_is_stable_across_repeated_calls() {
        let first = content_hash_hex(&input());
        for _ in 0..10 {
            assert_eq!(content_hash_hex(&input()), first);
        }
    }

    #[test]
    fn an_all_empty_input_is_not_the_md5_of_nothing() {
        let empty = HashInput::default();
        assert_eq!(content_hash_hex(&empty), hex::encode(Md5::digest(b"")));
        assert_ne!(content_hash_hex(&empty), content_hash_hex(&input()));
    }

    // --- date independence -------------------------------------------------

    #[test]
    fn the_publication_date_does_not_affect_the_hash() {
        // `HashInput` has no date field at all — that is the guarantee. This test exists so adding
        // one is a conscious, reviewed act.
        let a = RawItem {
            link: Some("https://example.com/a".into()),
            title: Some("Same".into()),
            content_html: Some("<p>x</p>".into()),
            date: Some(1_000_000_000),
            ..RawItem::default()
        };
        let b = RawItem {
            date: Some(1_999_999_999),
            ..a.clone()
        };

        let hash_of = |it: &RawItem| {
            content_hash_hex(&HashInput {
                link: it.link.as_deref().unwrap_or(""),
                title: it.title.as_deref().unwrap_or(""),
                content: it.effective_content().unwrap_or(""),
                ..HashInput::default()
            })
        };
        assert_eq!(
            hash_of(&a),
            hash_of(&b),
            "a feed that starts sending dates must not re-mark every article"
        );
    }

    // --- field serializers -------------------------------------------------

    #[test]
    fn authors_field_joins_without_a_leading_separator() {
        assert_eq!(authors_field(&[]), "");
        assert_eq!(authors_field(&["Ada".into()]), "Ada");
        assert_eq!(authors_field(&["Ada".into(), "Grace".into()]), "Ada; Grace");
    }

    #[test]
    fn tags_field_is_the_hash_prefixed_form() {
        assert_eq!(
            tags_field(&[]),
            "",
            "no tags stores an empty string, not '#'"
        );
        assert_eq!(tags_field(&["rust".into()]), "#rust");
        assert_eq!(tags_field(&["rust".into(), "rss".into()]), "#rust #rss");
    }

    #[test]
    fn enclosures_field_is_empty_when_there_are_none() {
        assert_eq!(enclosures_field(&[]), "");
    }

    #[test]
    fn enclosures_field_omits_absent_fields_and_keeps_slashes() {
        let json = enclosures_field(&[enclosure("https://example.com/a.mp3")]);
        assert_eq!(json, r#"[{"url":"https://example.com/a.mp3"}]"#);
    }

    #[test]
    fn enclosures_field_does_not_escape_unicode() {
        let mut e = enclosure("https://example.com/x.mp3");
        e.title = Some("Épisode 1".into());
        let json = enclosures_field(&[e]);
        assert!(json.contains("Épisode 1"), "{json}");
        assert!(!json.contains("\\u"), "{json}");
    }

    #[test]
    fn enclosure_order_is_significant() {
        let a = enclosures_field(&[enclosure("https://x/1"), enclosure("https://x/2")]);
        let b = enclosures_field(&[enclosure("https://x/2"), enclosure("https://x/1")]);
        assert_ne!(
            a, b,
            "reordered attachments are a real change to the article"
        );
    }

    #[test]
    fn a_display_only_enclosure_field_does_not_change_the_hash() {
        // `description`, `height` and `width` are not part of the identity, so scraping a thumbnail
        // dimension must not mark every podcast episode as updated.
        let a = enclosure("https://x/e.mp3");
        let mut b = a.clone();
        b.description = Some("shownotes".into());
        b.height = Some(100);
        assert_eq!(
            enclosures_field(std::slice::from_ref(&a)),
            enclosures_field(&[b])
        );

        // But a changed URL or length is.
        let mut c = a.clone();
        c.length = Some(1024);
        assert_ne!(enclosures_field(&[a]), enclosures_field(&[c]));
    }

    // --- original content --------------------------------------------------

    #[test]
    fn content_without_markers_is_returned_unchanged() {
        let body = "<p>plain</p>";
        assert_eq!(original_content(body, None), body);
        assert!(matches!(
            original_content(body, None),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn the_scrape_wrapper_is_stripped_before_hashing() {
        // `content_action: append` — the feed's own summary followed by the scraped full text.
        // FreshRSS clears the `original_content` attribute in this mode, so the strip *is* the
        // recovery path.
        let body = format!("<p>teaser</p>{FULLCONTENT_START}<p>full article</p>{FULLCONTENT_END}");
        assert_eq!(original_content(&body, None), "<p>teaser</p>");

        // `content_action: prepend` — the same, mirrored.
        let body = format!("{FULLCONTENT_START}<p>full article</p>{FULLCONTENT_END}<p>teaser</p>");
        assert_eq!(original_content(&body, None), "<p>teaser</p>");
    }

    #[test]
    fn the_strip_is_greedy_across_a_whole_document() {
        // `.*` with the `s` flag runs from the first start marker to the last end marker, so an
        // article that embeds its own markers loses everything between them. That is PHP's
        // behaviour and it is why the markers are comments nobody writes by hand.
        let body = format!(
            "a{FULLCONTENT_START}b{FULLCONTENT_END}c{FULLCONTENT_START}d{FULLCONTENT_END}e"
        );
        assert_eq!(original_content(&body, None), "ae");
    }

    #[test]
    fn the_wrapper_strip_spans_newlines() {
        let body = format!("{FULLCONTENT_START}\nline1\nline2\n{FULLCONTENT_END}tail");
        assert_eq!(original_content(&body, None), "tail");
    }

    #[test]
    fn a_stored_original_wins_over_the_marker_strip() {
        let body = format!("{FULLCONTENT_START}scraped{FULLCONTENT_END}");
        assert_eq!(
            original_content(&body, Some("from the feed")),
            "from the feed"
        );
        assert_eq!(
            original_content("whatever", Some("")),
            "",
            "an explicitly empty original is honoured"
        );
    }

    #[test]
    fn an_unterminated_marker_is_left_alone() {
        let body = format!("<p>x</p>{FULLCONTENT_START}never closed");
        assert_eq!(original_content(&body, None), body);
    }

    #[test]
    fn turning_scraping_on_does_not_change_the_hash() {
        let feed_body = "<p>teaser</p>";
        let scraped = "<p>the whole article, fetched from the website</p>";
        let before = content_hash_hex(&HashInput {
            content: &original_content(feed_body, None),
            ..HashInput::default()
        });

        // `content_action: append` recovers the original by stripping the wrapper…
        let appended = format!("{feed_body}{FULLCONTENT_START}{scraped}{FULLCONTENT_END}");
        assert_eq!(
            content_hash_hex(&HashInput {
                content: &original_content(&appended, None),
                ..HashInput::default()
            }),
            before
        );

        // …and `replace` recovers it from the stored attribute.
        let replaced = format!("{FULLCONTENT_START}{scraped}{FULLCONTENT_END}");
        assert_eq!(
            content_hash_hex(&HashInput {
                content: &original_content(&replaced, Some(feed_body)),
                ..HashInput::default()
            }),
            before,
            "enabling pathEntries must not mark the whole backlog as updated"
        );
    }
}
