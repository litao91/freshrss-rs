//! Reading a JSON document through a dot-notation map.
//!
//! Feed kinds 30 (JSON+DotNotation) and 35 (JSON inside HTML, found by XPath) do not use XPath at
//! all: a JSON document has no axes, and `items.0.title` says everything `//item[1]/title` would. The
//! path language is Laravel's `Arr::get` with three extensions FreshRSS added, all of which a
//! real-world configuration eventually needs:
//!
//! - a quoted segment is a literal, so `'"By " & author.name'` builds a byline;
//! - `&` concatenates, which is how a title and a subtitle are joined into one;
//! - `[0]` is accepted as well as `.0`, because that is what people type.
//!
//! The literal form also means a path can produce text that is nowhere in the document, which is why
//! `itemTimeFormat` — a format string, not a path — is kept out of the path reader.
//!
//! [`json_feed_dot_notation`](crate::kind::json_feed_dot_notation) is this vocabulary filled in for a
//! standard JSON Feed, but that format has its own parser: it has fields a generic map cannot
//! express, and reading it through the map would lose them.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use sha1::{Digest as _, Sha1};

use super::common::{is_remote_uri, mime_top_level};
use super::date::scraped_timestamp;
use crate::error::FeedError;
use crate::raw::{FeedFormat, RawEnclosure, RawFeed, RawItem, RawThumbnail};

/// The character a literal `&` is swapped for while a path is being split, so that an ampersand
/// inside quotes survives the split. PHP uses the same fullwidth character.
const ESCAPED_AMP: char = '\u{ff06}';

/// Reads a scalar out of `value` at `path`.
///
/// `None` means the path led nowhere, or led to an object or an array — a field that is supposed to
/// be a title is not one if it is a subtree.
///
/// # The path language
///
/// An empty path, `.` or `$` is the whole document. A quoted path is a literal. Otherwise the path is
/// split on `&` and the scalar parts are concatenated; with no `&` it is walked segment by segment
/// through objects and arrays, where `[3]` and `.3` mean the same thing. A key that literally
/// contains a dot is found before the path is split, so `{"a.b": 1}` is reachable as `a.b`.
pub fn get_string(value: &Json, path: &str) -> Option<String> {
    match read(value, path)? {
        Found::Node(node) => scalar(node),
        Found::Text(text) => Some(text),
    }
}

/// Reads a node out of `value` at `path`, without reducing it to text.
///
/// This is the form the item list, the categories and the attachments need: their result is a subtree
/// to iterate, not a string.
pub fn get_node<'a>(value: &'a Json, path: &str) -> Option<&'a Json> {
    match read(value, path)? {
        Found::Node(node) => Some(node),
        // A literal path names no node, so there is nothing to iterate.
        Found::Text(_) => None,
    }
}

/// What a path resolved to.
enum Found<'a> {
    Node(&'a Json),
    Text(String),
}

fn read<'a>(value: &'a Json, path: &str) -> Option<Found<'a>> {
    let path = path.trim();
    if matches!(path, "" | "." | "$") {
        return Some(Found::Node(value));
    }
    if let Some(text) = literal(path) {
        return Some(Found::Text(text.replace(ESCAPED_AMP, "&")));
    }

    let protected = protect_quoted(path);
    let parts: Vec<&str> = protected.split('&').collect();
    if parts.len() > 1 {
        let mut text = String::new();
        for part in parts {
            // A part that is not scalar contributes nothing rather than failing the whole path: a
            // byline built from an optional subtitle still has to work when the subtitle is missing.
            match read(value, part) {
                Some(Found::Node(node)) => text.push_str(&scalar(node).unwrap_or_default()),
                Some(Found::Text(part)) => text.push_str(&part),
                None => {}
            }
        }
        return Some(Found::Text(text));
    }

    walk(value, &normalize_brackets(&protected)).map(Found::Node)
}

/// A path that is entirely one quoted literal.
///
/// The text between the quotes may not contain a raw `&`, which is PHP's rule and the reason a
/// literal ampersand has to be written fullwidth: without it, `"a & b"` would be indistinguishable
/// from a concatenation of three paths.
fn literal(path: &str) -> Option<String> {
    let mut chars = path.chars();
    let first = chars.next()?;
    let last = chars.next_back()?;
    if !matches!(first, '\'' | '"') || first != last {
        return None;
    }
    let inner = &path[first.len_utf8()..path.len() - last.len_utf8()];
    if inner.contains('&') {
        return None;
    }
    Some(inner.to_owned())
}

/// Replaces every `&` inside a quoted run with [`ESCAPED_AMP`].
fn protect_quoted(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path.chars();
    while let Some(c) = rest.next() {
        out.push(c);
        if !matches!(c, '\'' | '"') {
            continue;
        }
        for inner in rest.by_ref() {
            out.push(if inner == '&' { ESCAPED_AMP } else { inner });
            if inner == c {
                break;
            }
        }
    }
    out
}

/// Rewrites `[3]` as `.3`, so one traversal handles both spellings.
///
/// A bracket whose contents are not a plain index is left alone: `items[?]` is not a path this
/// language supports, and silently turning it into something else would find the wrong field.
fn normalize_brackets(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with(']') {
            out.push('.');
            out.push_str(&digits);
            rest = &after[digits.len() + 1..];
        } else {
            out.push('[');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Walks a dotted path through objects and arrays.
fn walk<'a>(value: &'a Json, path: &str) -> Option<&'a Json> {
    // A key containing a literal dot wins, before the path is split into segments.
    if let Json::Object(map) = value {
        if let Some(found) = map.get(path) {
            return Some(found);
        }
    }
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            Json::Object(map) => map.get(segment)?,
            Json::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// PHP's `(string)` cast, restricted to the scalars it is applied to.
fn scalar(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Number(number) => Some(number.to_string()),
        // `(string)true` is `"1"` and `(string)false` is `""` in PHP, and a configuration that
        // concatenates a boolean field means it.
        Json::Bool(flag) => Some(if *flag { "1".to_owned() } else { String::new() }),
        _ => None,
    }
}

/// Reads a whole feed out of a JSON document.
///
/// `url` is where the document came from and becomes the feed's website; `fallback_title` is the
/// subscription's configured name, used when the map has no `feedTitle` path or that path finds
/// nothing.
///
/// # Errors
///
/// [`FeedError::BadConfig`] when the map has no `item` path, and [`FeedError::Empty`] when that path
/// finds no items.
pub fn scrape(
    document: &Json,
    map: &BTreeMap<String, String>,
    url: &str,
    fallback_title: &str,
) -> Result<RawFeed, FeedError> {
    let Some(item_path) = expression(map, "item") else {
        return Err(FeedError::BadConfig(
            "the dot-notation map has no `item` path, so there is nothing to scrape".to_owned(),
        ));
    };
    // Only an array, not an object: `serde_json`'s map is key-ordered, so iterating an object would
    // quietly reorder the articles, and a scrambled feed is worse than a rejected one.
    let Some(items) = get_node(document, item_path) else {
        return Err(FeedError::Empty);
    };
    let Json::Array(items) = items else {
        return Err(FeedError::Empty);
    };
    if items.is_empty() {
        return Err(FeedError::Empty);
    }

    let title = map_string(document, map, "feedTitle")
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| fallback_title.to_owned());
    let icon = map_string(document, map, "feedImage")
        .or_else(|| map_string(document, map, "feedImageFallback"))
        .filter(|icon| !icon.is_empty());

    let mut raw_items = Vec::with_capacity(items.len());
    for item in items {
        if let Some(item) = scrape_item(item, map)? {
            raw_items.push(item);
        }
    }

    Ok(RawFeed {
        title: (!title.is_empty()).then_some(title),
        base_url: Some(url.to_owned()),
        website: Some(url.to_owned()),
        description: None,
        icon,
        self_url: None,
        hub_url: None,
        effective_url: Some(url.to_owned()),
        items: raw_items,
        format: FeedFormat::Scraped,
    })
}

/// Reads one item.
///
/// Returns `None` for an item with no link, which is what PHP does and the only reason an item is
/// dropped: an article that cannot be opened is worse than no article, because the reader's whole
/// model of an entry is "a thing you can go to". An item with a link and nothing else is kept, and
/// [`RawItem::is_empty`] is what tells the caller there was nothing to read.
fn scrape_item(item: &Json, map: &BTreeMap<String, String>) -> Result<Option<RawItem>, FeedError> {
    let Some(link) = map_string(item, map, "itemUri") else {
        return Ok(None);
    };

    // `itemContentHTML` wins outright when the map has the key at all, even if that key finds
    // nothing: a configuration that asks for an HTML body and does not get one must not silently fall
    // back to a plain-text summary of a different field.
    let content = match expression(map, "itemContentHTML") {
        Some(_) => map_string(item, map, "itemContentHTML"),
        None => map_string(item, map, "itemContent"),
    };
    let title = map_string(item, map, "itemTitle");
    let timestamp = map_string(item, map, "itemTimestamp");
    let time_format = expression(map, "itemTimeFormat").unwrap_or_default();
    let uid = map_string(item, map, "itemUid").filter(|uid| !uid.is_empty());

    Ok(Some(RawItem {
        guid: Some(uid.unwrap_or_else(|| {
            synthetic_guid(
                title.as_deref().unwrap_or_default(),
                content.as_deref().unwrap_or_default(),
                &link,
            )
        })),
        guid_is_permalink: false,
        title,
        link: Some(link),
        date: timestamp.and_then(|value| scraped_timestamp(&value, time_format)),
        updated: None,
        content_html: content,
        content_text: None,
        authors: map_string(item, map, "itemAuthor").into_iter().collect(),
        enclosures: attachments(item, map),
        tags: categories(item, map),
        thumbnail: map_string(item, map, "itemThumbnail")
            .filter(|url| !url.is_empty())
            .map(|url| RawThumbnail {
                url,
                height: None,
                width: None,
                time: None,
            }),
    }))
}

/// The categories path, which yields either one string or an array of them.
fn categories(item: &Json, map: &BTreeMap<String, String>) -> Vec<String> {
    let Some(path) = expression(map, "itemCategories") else {
        return Vec::new();
    };
    match get_node(item, path) {
        Some(Json::Array(values)) => values
            .iter()
            .filter_map(scalar)
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
            .collect(),
        Some(other) => scalar(other)
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
            .into_iter()
            .collect(),
        None => Vec::new(),
    }
}

/// The attachments, each read through its own three paths.
fn attachments(item: &Json, map: &BTreeMap<String, String>) -> Vec<RawEnclosure> {
    let Some(path) = expression(map, "itemAttachment") else {
        return Vec::new();
    };
    let Some(Json::Array(values)) = get_node(item, path) else {
        return Vec::new();
    };
    values
        .iter()
        .filter_map(|attachment| {
            // A non-remote URL is dropped rather than stored: an enclosure is fetched by the media
            // player, and `file:` or `data:` there would be a way for a feed to point the browser at
            // the reader's own origin.
            let url = map_string(attachment, map, "itemAttachmentUrl")?;
            if !is_remote_uri(&url) {
                return None;
            }
            let mime = map_string(attachment, map, "itemAttachmentType")
                .map(|mime| mime.to_ascii_lowercase());
            Some(RawEnclosure {
                length: map_number(attachment, map, "itemAttachmentLength"),
                medium: mime_top_level(mime.as_deref()),
                url,
                mime,
                title: None,
                description: None,
                height: None,
                width: None,
            })
        })
        .collect()
}

/// The identity PHP synthesizes when the map has no `itemUid`, or it finds nothing.
///
/// Without it every article would share one empty guid and collapse into a single row.
fn synthetic_guid(title: &str, content: &str, link: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(title.as_bytes());
    digest.update(content.as_bytes());
    digest.update(link.as_bytes());
    format!("urn:sha1:{}", hex::encode(digest.finalize()))
}

/// A map entry, trimmed, or `None` when absent or blank.
fn expression<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Reads one scalar field through the map.
fn map_string(value: &Json, map: &BTreeMap<String, String>, key: &str) -> Option<String> {
    let path = expression(map, key)?;
    get_string(value, path).filter(|found| !found.is_empty())
}

/// Reads one numeric field through the map, accepting the string spelling JSON APIs often emit.
fn map_number(value: &Json, map: &BTreeMap<String, String>, key: &str) -> Option<i64> {
    let path = expression(map, key)?;
    match get_node(value, path)? {
        Json::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Json::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|length| *length > 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::kind::{json_feed_dot_notation, FeedKind};

    /// The document from `tests/app/Utils/dotNotationUtilTest.php`, whose cases are translated below.
    fn oracle() -> Json {
        json!({
            "hello": "world",
            "deeper": { "hello": "again" },
            "items": [
                { "meta": { "title": "first" } },
                { "meta": { "title": "second" } }
            ]
        })
    }

    fn standard_map() -> BTreeMap<String, String> {
        json_feed_dot_notation()
            .into_iter()
            .map(|(key, path)| (key.to_owned(), path.to_owned()))
            .collect()
    }

    fn entry(
        body: &str,
        kind: FeedKind,
        map: &BTreeMap<String, String>,
        x_path_to_json: &str,
    ) -> Result<RawFeed, FeedError> {
        crate::parse::scrape(
            body.as_bytes(),
            &crate::parse::Scrape {
                kind,
                map,
                x_path_to_json,
                url: "https://e.example/feed.json",
                fallback_title: "Configured name",
            },
        )
    }

    // --- the path language, and the PHP oracle for it -----------------------

    #[test]
    fn the_php_oracle_cases() {
        let doc = oracle();
        for (path, expected) in [
            ("hello", "world"),
            ("deeper.hello", "again"),
            ("items.0.meta.title", "first"),
            ("items[0].meta.title", "first"),
            ("items.1.meta.title", "second"),
            ("items[1].meta.title", "second"),
            (r#""Hello " & hello & '!'"#, "Hello world!"),
            (
                r#""Hello & goodbye " & hello & '!'"#,
                "Hello & goodbye world!",
            ),
            (
                r#""Hello " & hello & deeper.hello & "!""#,
                "Hello worldagain!",
            ),
        ] {
            assert_eq!(get_string(&doc, path).as_deref(), Some(expected), "{path}");
        }
    }

    #[test]
    fn the_whole_document_is_reachable_three_ways() {
        let doc = json!({ "a": 1 });
        for path in ["", "  ", ".", "$"] {
            assert_eq!(get_node(&doc, path), Some(&doc), "{path:?}");
            // …but it is not a scalar, so `get_string` finds nothing.
            assert_eq!(get_string(&doc, path), None, "{path:?}");
        }
    }

    #[test]
    fn a_path_that_leads_nowhere_is_absent() {
        let doc = oracle();
        assert_eq!(get_string(&doc, "nope"), None);
        assert_eq!(get_string(&doc, "hello.deeper"), None, "into a scalar");
        assert_eq!(get_string(&doc, "items.9.meta.title"), None, "out of range");
        assert_eq!(get_string(&doc, "items.-1"), None);
        assert_eq!(get_string(&doc, "items.meta"), None, "through an array");
        assert_eq!(get_node(&doc, "'literal'"), None, "a literal names no node");
    }

    #[test]
    fn a_key_containing_a_dot_is_found_before_the_path_is_split() {
        let doc = json!({ "a.b": "whole", "a": { "b": "split" } });
        assert_eq!(get_string(&doc, "a.b").as_deref(), Some("whole"));
        let only_split = json!({ "a": { "b": "split" } });
        assert_eq!(get_string(&only_split, "a.b").as_deref(), Some("split"));
    }

    #[test]
    fn scalars_become_their_php_string_cast() {
        let doc = json!({ "n": 42, "f": 1.5, "t": true, "f2": false, "s": "x" });
        assert_eq!(get_string(&doc, "n").as_deref(), Some("42"));
        assert_eq!(get_string(&doc, "f").as_deref(), Some("1.5"));
        assert_eq!(get_string(&doc, "t").as_deref(), Some("1"));
        assert_eq!(get_string(&doc, "f2").as_deref(), Some(""));
        assert_eq!(get_string(&doc, "s").as_deref(), Some("x"));
    }

    #[test]
    fn a_concatenation_skips_the_parts_that_are_not_there() {
        let doc = json!({ "a": "x" });
        assert_eq!(
            get_string(&doc, r#""[" & a & "] " & missing & b.c"#).as_deref(),
            Some("[x] "),
            "a byline built from an optional field still works without it"
        );
    }

    #[test]
    fn a_literal_ampersand_is_written_fullwidth() {
        // A raw `&` inside quotes would be indistinguishable from the concatenation operator, so the
        // fullwidth form is the escape — PHP's, and the reason a literal cannot contain a raw one.
        let doc = json!({});
        assert_eq!(
            get_string(&doc, "'Tom ＆ Jerry'").as_deref(),
            Some("Tom & Jerry")
        );
        assert_eq!(
            get_string(&doc, "'a & b'"),
            None,
            "a raw & is not a literal"
        );
    }

    #[test]
    fn brackets_that_are_not_an_index_are_left_alone() {
        assert_eq!(normalize_brackets("items[0].a"), "items.0.a");
        assert_eq!(normalize_brackets("items[12]"), "items.12");
        assert_eq!(normalize_brackets("items[?].a"), "items[?].a");
        assert_eq!(normalize_brackets("items[].a"), "items[].a");
        assert_eq!(normalize_brackets("a[0]b[1]"), "a.0b.1");
    }

    // --- reading a whole feed ----------------------------------------------

    const DOCUMENT: &str = r#"{
        "title": "Café & News",
        "icon": "https://e.example/icon.png",
        "items": [
            {
                "id": "a1",
                "url": "https://e.example/1",
                "title": "First",
                "content_html": "<p>One</p>",
                "content_text": "One",
                "author": "Alice",
                "date_published": "2026-10-01T08:00:00.000Z",
                "image": "https://e.example/1.png",
                "tags": ["News", " Café ", ""],
                "attachments": [
                    { "url": "https://e.example/a.mp3", "mime_type": "AUDIO/MPEG", "size_in_bytes": 1024 },
                    { "url": "file:///etc/passwd", "mime_type": "text/plain" }
                ]
            },
            { "url": "https://e.example/2", "title": "Second" },
            { "title": "No link, so dropped" }
        ]
    }"#;

    #[test]
    fn reads_a_whole_document_through_the_standard_map() {
        let feed = entry(DOCUMENT, FeedKind::JsonDotNotation, &standard_map(), "").unwrap();
        assert_eq!(feed.format, FeedFormat::Scraped);
        assert_eq!(feed.title.as_deref(), Some("Café & News"));
        assert_eq!(feed.icon.as_deref(), Some("https://e.example/icon.png"));
        assert_eq!(feed.website.as_deref(), Some("https://e.example/feed.json"));
        assert_eq!(feed.items.len(), 2, "the item with no link is dropped");

        let first = &feed.items[0];
        assert_eq!(first.title.as_deref(), Some("First"));
        assert_eq!(first.link.as_deref(), Some("https://e.example/1"));
        assert_eq!(first.guid.as_deref(), Some("a1"));
        assert_eq!(
            first.authors,
            [] as [&str; 0],
            "the standard map has no author key"
        );
        assert_eq!(
            first.content_html.as_deref(),
            Some("<p>One</p>"),
            "itemContentHTML wins over itemContent"
        );
        assert_eq!(first.date, Some(1_790_841_600), "2026-10-01T08:00:00Z");
        assert_eq!(
            first.thumbnail.as_ref().map(|t| t.url.as_str()),
            Some("https://e.example/1.png")
        );
        assert_eq!(
            first.tags,
            ["News", "Café"],
            "trimmed, and the blank dropped"
        );
        assert!(!first.guid_is_permalink);

        // The non-remote attachment is dropped: an enclosure is fetched by the media player.
        assert_eq!(first.enclosures.len(), 1);
        let enclosure = &first.enclosures[0];
        assert_eq!(enclosure.url, "https://e.example/a.mp3");
        assert_eq!(enclosure.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(enclosure.medium.as_deref(), Some("audio"));
        assert_eq!(enclosure.length, Some(1024));
    }

    #[test]
    fn an_item_with_only_a_link_is_kept() {
        let second = &entry(DOCUMENT, FeedKind::JsonDotNotation, &standard_map(), "")
            .unwrap()
            .items[1];
        assert_eq!(second.title.as_deref(), Some("Second"));
        assert!(second.content_html.is_none());
        assert!(second.guid.as_deref().unwrap().starts_with("urn:sha1:"));
        assert!(second.enclosures.is_empty());
        assert!(second.tags.is_empty());
        assert_eq!(second.date, None);
    }

    #[test]
    fn the_title_and_icon_fall_back() {
        let doc = json!({ "items": [{ "url": "https://e.example/1", "title": "t" }] });
        let map = BTreeMap::from([
            ("item".to_owned(), "items".to_owned()),
            ("itemUri".to_owned(), "url".to_owned()),
            ("itemTitle".to_owned(), "title".to_owned()),
            ("feedTitle".to_owned(), "missing".to_owned()),
            ("feedImage".to_owned(), "missing".to_owned()),
            ("feedImageFallback".to_owned(), "fallback_icon".to_owned()),
        ]);
        let mut doc = doc;
        doc["fallback_icon"] = json!("https://e.example/fallback.png");
        let feed = scrape(&doc, &map, "https://e.example/f.json", "Configured name").unwrap();
        assert_eq!(
            feed.title.as_deref(),
            Some("Configured name"),
            "a feedTitle path that finds nothing is the same as no path"
        );
        assert_eq!(feed.icon.as_deref(), Some("https://e.example/fallback.png"));
    }

    #[test]
    fn a_map_with_no_item_path_is_a_configuration_error() {
        let doc = json!({ "items": [] });
        let err = scrape(&doc, &BTreeMap::new(), "https://e.example/", "n").unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");
        assert!(err.to_string().contains("item"), "{err}");
    }

    #[test]
    fn no_items_is_an_empty_feed_whatever_shape_the_path_found() {
        let map = BTreeMap::from([("item".to_owned(), "items".to_owned())]);
        for doc in [
            json!({ "items": [] }),
            json!({ "items": {} }),
            json!({}),
            // An object is refused rather than iterated: `serde_json`'s map is key-ordered, so
            // iterating it would quietly reorder the articles.
            json!({ "items": { "b": { "url": "https://e/2" }, "a": { "url": "https://e/1" } } }),
        ] {
            let err = scrape(&doc, &map, "https://e.example/", "n").unwrap_err();
            assert!(matches!(err, FeedError::Empty), "{doc}: {err}");
        }
    }

    #[test]
    fn a_time_format_decides_how_the_timestamp_is_read() {
        let doc = json!({ "items": [{ "url": "https://e/1", "d": "01/10/2026 08:30" }] });
        let base = BTreeMap::from([
            ("item".to_owned(), "items".to_owned()),
            ("itemUri".to_owned(), "url".to_owned()),
            ("itemTimestamp".to_owned(), "d".to_owned()),
        ]);
        assert_eq!(
            scrape(&doc, &base, "https://e/", "n").unwrap().items[0].date,
            None,
            "an ambiguous slash date is not guessed at"
        );

        let mut map = base.clone();
        map.insert("itemTimeFormat".to_owned(), "d/m/Y H:i".to_owned());
        assert_eq!(
            scrape(&doc, &map, "https://e/", "n").unwrap().items[0].date,
            Some(1_790_843_400)
        );

        map.insert("itemTimeFormat".to_owned(), "U".to_owned());
        let epoch = json!({ "items": [{ "url": "https://e/1", "d": "1790843400123" }] });
        assert_eq!(
            scrape(&epoch, &map, "https://e/", "n").unwrap().items[0].date,
            Some(1_790_843_400),
            "milliseconds need no special case"
        );
    }

    #[test]
    fn categories_may_be_one_string_or_a_list() {
        let one = json!({ "items": [{ "url": "https://e/1", "c": "Solo" }] });
        let many = json!({ "items": [{ "url": "https://e/1", "c": ["a", "b", 3, null] }] });
        let map = BTreeMap::from([
            ("item".to_owned(), "items".to_owned()),
            ("itemUri".to_owned(), "url".to_owned()),
            ("itemCategories".to_owned(), "c".to_owned()),
        ]);
        assert_eq!(
            scrape(&one, &map, "https://e/", "n").unwrap().items[0].tags,
            ["Solo"]
        );
        assert_eq!(
            scrape(&many, &map, "https://e/", "n").unwrap().items[0].tags,
            ["a", "b", "3"],
            "a number is a tag, a null is not"
        );
    }

    #[test]
    fn an_attachment_length_may_be_a_string() {
        let doc = json!({ "items": [{
            "url": "https://e/1",
            "a": [{ "u": "https://e/a.mp3", "l": "2048" }, { "u": "https://e/b.mp3", "l": 0 }]
        }] });
        let map = BTreeMap::from([
            ("item".to_owned(), "items".to_owned()),
            ("itemUri".to_owned(), "url".to_owned()),
            ("itemAttachment".to_owned(), "a".to_owned()),
            ("itemAttachmentUrl".to_owned(), "u".to_owned()),
            ("itemAttachmentLength".to_owned(), "l".to_owned()),
        ]);
        let feed = scrape(&doc, &map, "https://e/", "n").unwrap();
        assert_eq!(feed.items[0].enclosures[0].length, Some(2048));
        assert_eq!(
            feed.items[0].enclosures[1].length, None,
            "zero is no length"
        );
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let err = entry("{not json", FeedKind::JsonDotNotation, &standard_map(), "").unwrap_err();
        assert!(matches!(err, FeedError::Parse(_)), "{err}");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    // --- kind 35: JSON inside HTML -----------------------------------------

    const JSON_LD: &str = r#"{"@type":"Blog","headline":"From the page","blogPost":[
        {"url":"https://e.example/1","headline":"One","datePublished":"1790843400"},
        {"url":"https://e.example/2","headline":"Two"}
    ]}"#;

    fn html_with_json(json: &str) -> String {
        format!(
            r#"<html><head><script type="application/ld+json">{json}</script></head><body><p>x</p></body></html>"#
        )
    }

    fn ld_map() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("feedTitle".to_owned(), "0.headline".to_owned()),
            ("item".to_owned(), "0.blogPost".to_owned()),
            ("itemUri".to_owned(), "url".to_owned()),
            ("itemTitle".to_owned(), "headline".to_owned()),
            ("itemTimestamp".to_owned(), "datePublished".to_owned()),
            ("itemTimeFormat".to_owned(), "U".to_owned()),
        ])
    }

    #[test]
    fn json_found_by_xpath_is_wrapped_in_an_array() {
        let page = html_with_json(JSON_LD);
        let feed = entry(
            &page,
            FeedKind::HtmlXpathJsonDotNotation,
            &ld_map(),
            "//script[@type='application/ld+json']",
        )
        .unwrap();
        // One script tag, and still `0.` — PHP collects node results into an array, and existing
        // configurations are written against that.
        assert_eq!(feed.title.as_deref(), Some("From the page"));
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.items[0].title.as_deref(), Some("One"));
        assert_eq!(feed.items[0].date, Some(1_790_843_400));
    }

    #[test]
    fn an_expression_returning_a_string_is_the_document_itself() {
        let page = html_with_json(JSON_LD);
        let mut map = ld_map();
        map.insert("feedTitle".to_owned(), "headline".to_owned());
        map.insert("item".to_owned(), "blogPost".to_owned());
        let feed = entry(
            &page,
            FeedKind::HtmlXpathJsonDotNotation,
            &map,
            "string(//script[@type='application/ld+json'])",
        )
        .unwrap();
        assert_eq!(
            feed.items.len(),
            2,
            "no `0.` prefix, because nothing was wrapped"
        );
    }

    #[test]
    fn several_json_blocks_become_several_array_entries() {
        let page = r#"<html><body><script type="application/ld+json">{"n":1}</script><script type="application/ld+json">{"n":2}</script></body></html>"#;
        let doc = super::super::dom::Doc::from_html(page);
        let json = super::super::json_from_html(&doc, "//script").expect("found");
        assert_eq!(json, serde_json::json!([{ "n": 1 }, { "n": 2 }]));
    }

    #[test]
    fn kind_35_needs_to_be_told_where_the_json_is() {
        let err = entry(
            &html_with_json(JSON_LD),
            FeedKind::HtmlXpathJsonDotNotation,
            &ld_map(),
            "",
        )
        .unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");

        // A script tag whose contents are not JSON is a parse failure, not a silently empty feed.
        let err = entry(
            "<html><body><script>var x = 1;</script></body></html>",
            FeedKind::HtmlXpathJsonDotNotation,
            &ld_map(),
            "//script",
        )
        .unwrap_err();
        assert!(matches!(err, FeedError::Parse(_)), "{err}");
    }

    // --- agreement with the dedicated JSON Feed parser ----------------------

    #[test]
    fn the_standard_map_agrees_with_the_json_feed_parser() {
        // JSON Feed has its own parser because it has fields a generic map cannot express; where the
        // two overlap they must not disagree, or the same document would produce different articles
        // depending on which kind the feed was configured as.
        let document = r#"{
            "version": "https://jsonfeed.org/version/1.1",
            "title": "Example",
            "home_page_url": "https://e.example/",
            "icon": "https://e.example/icon.png",
            "items": [
                {
                    "id": "1",
                    "url": "https://e.example/1",
                    "title": "One",
                    "content_html": "<p>One</p>",
                    "date_published": "2026-10-01T08:00:00-00:00",
                    "image": "https://e.example/1.png",
                    "tags": ["a", "b"],
                    "attachments": [{ "url": "https://e.example/a.mp3", "mime_type": "audio/mpeg", "size_in_bytes": 7 }]
                }
            ]
        }"#;
        let scraped = entry(document, FeedKind::JsonDotNotation, &standard_map(), "").unwrap();
        let parsed = super::super::jsonfeed::parse(document).unwrap();

        assert_eq!(scraped.title, parsed.title);
        assert_eq!(scraped.icon, parsed.icon);
        assert_eq!(scraped.items.len(), parsed.items.len());
        let (a, b) = (&scraped.items[0], &parsed.items[0]);
        assert_eq!(a.guid, b.guid);
        assert_eq!(a.link, b.link);
        assert_eq!(a.title, b.title);
        assert_eq!(a.content_html, b.content_html);
        assert_eq!(a.date, b.date);
        assert_eq!(a.thumbnail, b.thumbnail);
        assert_eq!(a.tags, b.tags);
        assert_eq!(a.enclosures.len(), b.enclosures.len());
        assert_eq!(a.enclosures[0].url, b.enclosures[0].url);
        assert_eq!(a.enclosures[0].mime, b.enclosures[0].mime);
        assert_eq!(a.enclosures[0].length, b.enclosures[0].length);
    }
}
