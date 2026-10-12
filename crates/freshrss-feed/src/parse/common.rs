//! Pieces the XML readers share: URL sanity, tag splitting and Media RSS attribute mapping.

use crate::raw::{RawEnclosure, RawThumbnail};

/// True for a URL that can actually be fetched.
///
/// Port of SimplePie's `Misc::is_remote_uri`. FreshRSS applies it before storing a thumbnail or an
/// enclosure, because feeds do carry `data:` images and `file:` paths — and an enclosure URL ends up
/// in an `<audio src>` the reader will load.
///
/// The scheme comparison is case-insensitive: RFC 3986 says schemes are, and feeds do contain
/// `HTTP://`.
pub fn is_remote_uri(url: &str) -> bool {
    let trimmed = url.trim();
    let Some(colon) = trimmed.find(':') else {
        return false;
    };
    if !trimmed[..colon].eq_ignore_ascii_case("http")
        && !trimmed[..colon].eq_ignore_ascii_case("https")
    {
        return false;
    }
    let rest = &trimmed[colon + 1..];
    // Require `//` and then a host: this rejects `http:/x`, `http://`, `http://?q` and `http://#f`.
    let Some(after_slashes) = rest.strip_prefix("//") else {
        return false;
    };
    let host_end = after_slashes
        .find(['/', '?', '#'])
        .unwrap_or(after_slashes.len());
    !after_slashes[..host_end].is_empty()
}

/// Removes duplicates while preserving first-occurrence order.
///
/// `Vec::dedup` only collapses *adjacent* repeats, which is not enough: a feed that lists
/// `<category>rust</category>` then `<category>rss</category>` then `<category>rust</category>`
/// would keep both `rust` entries and show the tag twice in the sidebar.
pub fn unique_in_place(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    values.retain(|v| seen.insert(v.clone()));
}

/// Resolves an href against the document's `xml:base`, leaving anything unresolvable as it was.
///
/// Atom explicitly allows relative hrefs, and feeds that use them are common — the `<link>` of an
/// entry is usually the site's own path. Without resolving, `is_remote_uri` rejects the value and
/// the article ends up with no link at all, which is the most visible failure a reader can have:
/// there is nothing to open.
///
/// A `base` of `None` or `""` returns the href unchanged, so feeds without `xml:base` behave exactly
/// as before and a relative href is still rejected by [`is_remote_uri`].
pub fn resolve_url(href: &str, base: Option<&str>) -> String {
    let href = href.trim();
    let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) else {
        return href.to_owned();
    };
    // Already absolute: nothing to do, and re-resolving would normalize away a form the publisher
    // chose (a trailing slash, for instance), which feeds the change-detection hash.
    if is_remote_uri(href) {
        return href.to_owned();
    }
    crate::sanitize::uri::absolutize(href, base)
}

/// Resolves an href against `base` and returns it only if it is then fetchable.
pub fn resolve_remote(href: &str, base: Option<&str>) -> Option<String> {
    let resolved = resolve_url(href, base);
    is_remote_uri(&resolved).then_some(resolved)
}

/// Splits a `<category>` label into individual tags.
///
/// Some feeds put every keyword in one comma-separated category, which is why FreshRSS explodes on
/// `,` rather than taking the label whole. Duplicates are dropped, first occurrence winning, so a
/// feed that repeats a keyword does not inflate the tag list.
pub fn split_tags(label: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in label.split(',') {
        let tag = part.trim();
        if !tag.is_empty() && !out.iter().any(|existing: &String| existing == tag) {
            out.push(tag.to_owned());
        }
    }
    out
}

/// Parses a possibly-absent integer attribute.
pub fn int_attr(attrs: &super::xml::Attrs, name: &str) -> Option<i64> {
    super::xml::attr(attrs, name)
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
}

/// Builds an enclosure from Media RSS or `<enclosure>` attributes, or `None` if the URL is unusable.
///
/// Both `url` (RSS `<enclosure>`, Media RSS) and `href` (`<atom:link rel="enclosure">`) are
/// accepted, since the three spellings mean the same thing and a feed may use any of them.
///
/// `mime` and `medium` are lowercased, matching FreshRSS, so `AUDIO/MPEG` and `audio/mpeg` produce
/// the same stored value and therefore the same change-detection hash.
pub fn enclosure_from_attrs(attrs: &super::xml::Attrs) -> Option<RawEnclosure> {
    let url = super::xml::attr(attrs, "url")
        .or_else(|| super::xml::attr(attrs, "href"))?
        .trim();
    if !is_remote_uri(url) {
        return None;
    }
    let mime = super::xml::attr(attrs, "type")
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.to_ascii_lowercase());
    Some(RawEnclosure {
        medium: super::xml::attr(attrs, "medium")
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| m.to_ascii_lowercase())
            .or_else(|| mime_top_level(mime.as_deref())),
        url: url.to_owned(),
        mime,
        length: int_attr(attrs, "length")
            .or_else(|| int_attr(attrs, "size_in_bytes"))
            .or_else(|| int_attr(attrs, "fileSize")),
        title: nonempty(super::xml::attr(attrs, "title")),
        description: None,
        height: int_attr(attrs, "height"),
        width: int_attr(attrs, "width"),
    })
}

/// [`enclosure_from_attrs`] with the URL resolved against a document base first.
///
/// Atom permits a relative `href`, and an attachment that is dropped for being relative means a
/// podcast episode with no player — so resolution has to happen before the remote check.
pub fn enclosure_from_attrs_with_base(
    attrs: &super::xml::Attrs,
    base: Option<&str>,
) -> Option<RawEnclosure> {
    let raw = super::xml::attr(attrs, "url").or_else(|| super::xml::attr(attrs, "href"))?;
    let resolved = resolve_remote(raw, base)?;
    let mime = super::xml::attr(attrs, "type")
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.to_ascii_lowercase());
    Some(RawEnclosure {
        medium: super::xml::attr(attrs, "medium")
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| m.to_ascii_lowercase())
            .or_else(|| mime_top_level(mime.as_deref())),
        url: resolved,
        mime,
        length: int_attr(attrs, "length")
            .or_else(|| int_attr(attrs, "size_in_bytes"))
            .or_else(|| int_attr(attrs, "fileSize")),
        title: nonempty(super::xml::attr(attrs, "title")),
        description: None,
        height: int_attr(attrs, "height"),
        width: int_attr(attrs, "width"),
    })
}

/// [`thumbnail_from_attrs`] with the URL resolved against a document base first.
pub fn thumbnail_from_attrs_with_base(
    attrs: &super::xml::Attrs,
    base: Option<&str>,
) -> Option<RawThumbnail> {
    let raw = super::xml::attr(attrs, "url").or_else(|| super::xml::attr(attrs, "href"))?;
    let resolved = resolve_remote(raw, base)?;
    Some(RawThumbnail {
        url: resolved,
        height: int_attr(attrs, "height"),
        width: int_attr(attrs, "width"),
        time: nonempty(super::xml::attr(attrs, "time")),
    })
}

/// The top-level MIME type, when it is one of the five media kinds the reader can do something with.
///
/// Deriving `medium` from the MIME type is what lets an `<atom:link rel="enclosure" type="audio/mpeg">`
/// drive the audio player; Media RSS feeds say it explicitly, Atom feeds do not.
pub fn mime_top_level(mime: Option<&str>) -> Option<String> {
    mime.and_then(|m| m.split('/').next())
        .filter(|t| matches!(*t, "image" | "audio" | "video" | "text" | "application"))
        .map(str::to_owned)
}

/// Builds a thumbnail from `media:thumbnail` attributes, or `None` if the URL is unusable.
pub fn thumbnail_from_attrs(attrs: &super::xml::Attrs) -> Option<RawThumbnail> {
    let url = super::xml::attr(attrs, "url")?.trim();
    if !is_remote_uri(url) {
        return None;
    }
    Some(RawThumbnail {
        url: url.to_owned(),
        height: int_attr(attrs, "height"),
        width: int_attr(attrs, "width"),
        time: nonempty(super::xml::attr(attrs, "time")),
    })
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> super::super::xml::Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn only_http_and_https_are_remote() {
        for ok in [
            "http://example.com/a.mp3",
            "https://example.com/a.mp3",
            "  https://example.com/a.mp3  ",
            "HTTPS://example.com",
        ] {
            assert!(is_remote_uri(ok), "{ok}");
        }
        for bad in [
            "",
            "data:image/png;base64,AAAA",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "//example.com/a",
            "example.com/a",
            "ftp://example.com/a",
            "http://",
            "http:///path",
        ] {
            assert!(!is_remote_uri(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn a_single_category_is_split_on_commas() {
        assert_eq!(
            split_tags("rust, rss ,reader"),
            vec!["rust", "rss", "reader"]
        );
        assert_eq!(split_tags("solo"), vec!["solo"]);
        assert!(split_tags("").is_empty());
        assert!(split_tags(" , , ").is_empty());
    }

    #[test]
    fn duplicate_tags_are_dropped_keeping_the_first() {
        assert_eq!(split_tags("a, b, a"), vec!["a", "b"]);
        assert_eq!(split_tags("a,a,a"), vec!["a"]);
    }

    #[test]
    fn enclosures_reject_non_remote_urls() {
        assert!(enclosure_from_attrs(&attrs(&[("url", "data:image/png;base64,x")])).is_none());
        assert!(enclosure_from_attrs(&attrs(&[("length", "1024")])).is_none());
        assert!(enclosure_from_attrs(&attrs(&[("url", "https://x/a.mp3")])).is_some());
    }

    #[test]
    fn enclosure_mime_and_medium_are_lowercased() {
        let e = enclosure_from_attrs(&attrs(&[
            ("url", "https://x/a.mp3"),
            ("type", "AUDIO/MPEG"),
            ("medium", "Audio"),
            ("length", "1024"),
            ("height", "0"),
            ("width", "640"),
            ("title", "  "),
        ]))
        .unwrap();
        assert_eq!(e.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(e.medium.as_deref(), Some("audio"));
        assert_eq!(e.length, Some(1024));
        // Zero is not a real dimension, and a blank title is not a title.
        assert_eq!(e.height, None);
        assert_eq!(e.width, Some(640));
        assert_eq!(e.title, None);
    }

    #[test]
    fn a_non_numeric_length_is_dropped_not_zeroed() {
        let e = enclosure_from_attrs(&attrs(&[("url", "https://x/a.mp3"), ("length", "unknown")]))
            .unwrap();
        assert_eq!(e.length, None);
    }

    #[test]
    fn thumbnails_reject_non_remote_urls() {
        assert!(thumbnail_from_attrs(&attrs(&[("url", "file:///tmp/x.png")])).is_none());
        let t = thumbnail_from_attrs(&attrs(&[
            ("url", "https://x/t.png"),
            ("height", "90"),
            ("width", "160"),
            ("time", "00:01:30"),
        ]))
        .unwrap();
        assert_eq!(t.url, "https://x/t.png");
        assert_eq!((t.height, t.width), (Some(90), Some(160)));
        assert_eq!(t.time.as_deref(), Some("00:01:30"));
    }
}
