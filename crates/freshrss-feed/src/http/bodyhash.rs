//! The whole-document fingerprint that decides "this feed has not changed".
//!
//! Port of SimplePie's `clean_hash()`, which FreshRSS stores as the `SimplePieHash` feed attribute.
//! A conditional GET only helps when the server sends `ETag`/`Last-Modified` and honours them; many
//! do not, and a feed that regenerates its XML on every request sends a fresh `Last-Modified` while
//! its articles are identical. Comparing a hash of the body is what catches that.
//!
//! The hash is *not* over the raw bytes. Feeds embed values that change on every fetch without any
//! article changing — `<lastBuildDate>`, comment counters, view statistics — and hashing those would
//! make every refresh look like an update, re-running sanitization and re-hashing every article for
//! nothing. Stripping them first is the whole point of this module.

use once_cell::sync::Lazy;
use sha1::{Digest as _, Sha1};

/// Elements whose contents change without any article changing.
///
/// `slash:comments` is a comment *count*; `media:starRating` and `media:statistics` are view and
/// rating counters that a video platform updates continuously.
const VOLATILE_ELEMENTS: &[&str] = &[
    "lastBuildDate",
    "pubDate",
    "updated",
    "feedDate",
    "dc:date",
    "slash:comments",
];

/// Self-closing elements carrying volatile statistics.
const VOLATILE_EMPTY_ELEMENTS: &[&str] = &["media:starRating", "media:statistics"];

/// A comment longer than this is left alone.
///
/// SimplePie's cap. Without it, a body containing `<!--` and no terminator would make the scanner
/// walk to the end of a multi-megabyte document on every refresh, for a fingerprint that is about to
/// differ anyway.
const MAX_COMMENT_SPAN: usize = 8192;

static VOLATILE_PAIR_RE: Lazy<regex::Regex> = Lazy::new(|| {
    // `regex` has no backreferences, so `<tag>…</tag>` is enumerated per name. Enumerating is also
    // stricter than SimplePie's `\1`, which matters: a loose match would strip a real change.
    let pairs = VOLATILE_ELEMENTS
        .iter()
        .map(|t| format!("<{t}>[^<]+</{t}>"))
        .collect::<Vec<_>>()
        .join("|");
    regex::Regex::new(&pairs).expect("generated from a static list")
});

static VOLATILE_EMPTY_RE: Lazy<regex::Regex> = Lazy::new(|| {
    let names = VOLATILE_EMPTY_ELEMENTS.join("|");
    // `[^/<>]+` is SimplePie's: attributes only, so a stray `/` or `<` ends the match rather than
    // letting it run across elements.
    regex::Regex::new(&format!("<(?:{names}) [^/<>]+/>")).expect("generated from a static list")
});

/// The volatile-stripped SHA-1 of a feed body, as a lowercase hex string.
///
/// Returns `""` for an empty body, matching SimplePie: an empty response is "no fingerprint" rather
/// than "the fingerprint of nothing", and the caller must not treat that as unchanged.
pub fn body_hash(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    let Ok(text) = std::str::from_utf8(body) else {
        // A non-UTF-8 body cannot be scanned for text patterns. Hashing the raw bytes is still
        // correct for change detection — it just cannot ignore the volatile parts.
        return hex::encode(Sha1::digest(body));
    };

    let without_elements = VOLATILE_PAIR_RE.replace_all(text, "");
    let without_empty = VOLATILE_EMPTY_RE.replace_all(&without_elements, "");
    let stripped = strip_comments(&without_empty);

    hex::encode(Sha1::digest(stripped.as_bytes()))
}

/// Removes XML comments, bounded by [`MAX_COMMENT_SPAN`].
///
/// A comment ends at `-->` or, following SimplePie, at an apparent `]]>` — a generator that emitted
/// `<!--` inside a CDATA section would otherwise swallow the rest of the document.
fn strip_comments(text: &str) -> String {
    if !text.contains("<!--") {
        return text.to_owned();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(open) = rest.find("<!--") {
        let body_start = open + "<!--".len();
        let window = &rest[body_start..];
        // The first terminator within the cap, whichever comes earlier.
        let terminator = ["-->", "]]>"]
            .iter()
            .filter_map(|t| window.find(t).map(|at| (at, t.len())))
            .filter(|(at, _)| *at <= MAX_COMMENT_SPAN)
            .min_by_key(|(at, _)| *at);

        out.push_str(&rest[..open]);
        match terminator {
            Some((at, len)) => rest = &window[at + len..],
            // No terminator inside the cap: this is not a comment, so keep the `<!--` and move on.
            // Advancing past it is what stops an unterminated `<!--` from looping.
            None => {
                out.push_str("<!--");
                rest = window;
            }
        }
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = r#"<item><title>Hello</title><guid>1</guid></item>"#;

    fn rss(channel_extra: &str, items: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title>{channel_extra}{items}</channel></rss>"#
        )
    }

    #[test]
    fn two_bodies_differing_only_in_lastbuilddate_hash_equal() {
        let a = rss(
            "<lastBuildDate>Mon, 02 Jan 2006 15:04:05 +0000</lastBuildDate>",
            ITEM,
        );
        let b = rss(
            "<lastBuildDate>Tue, 03 Jan 2006 09:00:00 +0000</lastBuildDate>",
            ITEM,
        );
        assert_eq!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn every_volatile_element_is_ignored() {
        for tag in VOLATILE_ELEMENTS {
            let a = rss(&format!("<{tag}>one</{tag}>"), ITEM);
            let b = rss(&format!("<{tag}>two</{tag}>"), ITEM);
            assert_eq!(
                body_hash(a.as_bytes()),
                body_hash(b.as_bytes()),
                "<{tag}> should not affect the hash"
            );
        }
    }

    #[test]
    fn volatile_statistics_attributes_are_ignored() {
        for tag in VOLATILE_EMPTY_ELEMENTS {
            let a = rss(&format!(r#"<{tag} views="1" rating="3.0"/>"#), ITEM);
            let b = rss(&format!(r#"<{tag} views="999" rating="4.9"/>"#), ITEM);
            assert_eq!(body_hash(a.as_bytes()), body_hash(b.as_bytes()), "<{tag}/>");
        }
    }

    #[test]
    fn a_changed_article_does_change_the_hash() {
        let a = rss("", ITEM);
        let b = rss("", r#"<item><title>Hello!</title><guid>1</guid></item>"#);
        assert_ne!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn a_new_article_does_change_the_hash() {
        let a = rss("", ITEM);
        let b = rss("", &format!("{ITEM}{ITEM}"));
        assert_ne!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn an_item_level_pubdate_is_also_volatile() {
        // `<pubDate>` inside an item is stripped by the same rule. That is intentional and matches
        // SimplePie: an article's *content* is what identifies a change, and the GUID already covers
        // identity.
        let a = rss(
            "",
            "<item><title>T</title><pubDate>Mon, 02 Jan 2006 15:04:05 +0000</pubDate></item>",
        );
        let b = rss(
            "",
            "<item><title>T</title><pubDate>Tue, 03 Jan 2006 15:04:05 +0000</pubDate></item>",
        );
        assert_eq!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn an_added_element_is_not_stripped() {
        let a = rss("", ITEM);
        let b = rss("<language>en</language>", ITEM);
        assert_ne!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn comments_are_ignored() {
        let a = rss("<!-- generated at 12:00 -->", ITEM);
        let b = rss("<!-- generated at 13:00 -->", ITEM);
        let c = rss("", ITEM);
        assert_eq!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
        assert_eq!(body_hash(a.as_bytes()), body_hash(c.as_bytes()));
    }

    #[test]
    fn a_comment_terminated_by_cdata_end_stops_there() {
        // SimplePie stops at an apparent `]]>` so a `<!--` inside CDATA cannot eat the document.
        let a = "<!-- open ]]>rest of the document";
        let b = "<!-- other ]]>rest of the document";
        assert_eq!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
        assert!(strip_comments(a).contains("rest of the document"));
    }

    #[test]
    fn an_unterminated_comment_is_left_as_text() {
        let body = "<rss><!-- never closed";
        assert_eq!(strip_comments(body), body);
        assert_eq!(
            body_hash(body.as_bytes()),
            hex::encode(Sha1::digest(body.as_bytes()))
        );
    }

    #[test]
    fn a_comment_beyond_the_cap_is_not_stripped() {
        let long = format!("<!--{}-->", "x".repeat(MAX_COMMENT_SPAN + 10));
        assert_eq!(
            strip_comments(&long),
            long,
            "too long to be treated as a comment"
        );
        let short = format!("<!--{}-->", "x".repeat(10));
        assert_eq!(strip_comments(&short), "");
    }

    #[test]
    fn nested_looking_comment_markers_do_not_loop() {
        let body = "<!--<!--<!---->tail";
        let out = strip_comments(body);
        assert!(out.contains("tail"), "{out}");
    }

    #[test]
    fn an_empty_body_has_no_hash() {
        assert_eq!(body_hash(b""), "");
    }

    #[test]
    fn a_non_utf8_body_is_still_fingerprinted() {
        let body = b"<rss>\xFF\xFE</rss>";
        assert_eq!(body_hash(body), hex::encode(Sha1::digest(body)));
        assert_ne!(body_hash(body), body_hash(b"<rss>other</rss>"));
    }

    #[test]
    fn the_hash_is_40_lowercase_hex_characters() {
        let h = body_hash(rss("", ITEM).as_bytes());
        assert_eq!(h.len(), 40);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn the_hash_is_stable_across_calls() {
        let body = rss("<lastBuildDate>x</lastBuildDate>", ITEM);
        let first = body_hash(body.as_bytes());
        for _ in 0..5 {
            assert_eq!(body_hash(body.as_bytes()), first);
        }
    }

    #[test]
    fn whitespace_outside_volatile_elements_is_significant() {
        // Only the listed elements are stripped; incidental reformatting by a generator does change
        // the hash. That is correct — it is a real difference in the document — and it is why the
        // volatile list exists rather than a "normalize the XML" pass.
        let a = rss("", ITEM);
        let b = rss("", &format!("  {ITEM}  "));
        assert_ne!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }

    #[test]
    fn an_empty_volatile_element_is_not_stripped() {
        // `<pubDate></pubDate>` has no `[^<]+` content, so the pattern does not match and the
        // difference is visible. SimplePie behaves the same way.
        let a = rss("<pubDate></pubDate>", ITEM);
        let b = rss("", ITEM);
        assert_ne!(body_hash(a.as_bytes()), body_hash(b.as_bytes()));
    }
}
