//! JSON Feed 1.0 / 1.1.
//!
//! This is the one format where parsing is trivial — serde does it — and the work is in the
//! *semantics*: `id` is never a permalink, `content_html` and `content_text` are both bodies at
//! different fidelities, 1.0 used a singular `author` where 1.1 uses `authors`, and `attachments`
//! carry a MIME type rather than a Media RSS `medium`.

use serde::Deserialize;

use super::common::{is_remote_uri, mime_top_level, unique_in_place};
use super::date;
use super::FeedError;
use crate::raw::{FeedFormat, RawEnclosure, RawFeed, RawItem, RawThumbnail};

/// Parses a JSON Feed document.
///
/// # Errors
///
/// [`FeedError::Parse`] when the body is not JSON, or is JSON but not a JSON Feed — a plain object
/// with no `version` and no `items` is a JSON API response, and reporting it as an empty feed would
/// hide the fact that the URL is wrong.
pub fn parse(body: &str) -> Result<RawFeed, FeedError> {
    let doc: Document = serde_json::from_str(body)
        .map_err(|e| FeedError::Parse(format!("invalid JSON Feed: {e}")))?;

    if doc.version.is_none() && doc.items.is_none() {
        return Err(FeedError::Parse(
            "JSON document is not a JSON Feed (no `version` and no `items`)".to_owned(),
        ));
    }

    Ok(RawFeed {
        title: doc.title.as_deref().and_then(nonempty),
        website: doc.home_page_url.as_deref().and_then(nonempty),
        self_url: doc.feed_url.as_deref().and_then(nonempty),
        description: doc
            .description
            .or(doc.user_comment)
            .as_deref()
            .and_then(nonempty),
        icon: doc
            .icon
            .or(doc.favicon)
            .as_deref()
            .filter(|u| is_remote_uri(u))
            .and_then(nonempty),
        hub_url: doc.hubs.and_then(|hubs| {
            hubs.into_iter()
                .find_map(|h| h.url.filter(|u| is_remote_uri(u)))
        }),
        format: FeedFormat::JsonFeed,
        items: doc
            .items
            .unwrap_or_default()
            .into_iter()
            .filter_map(convert_item)
            .collect(),
        ..RawFeed::default()
    })
}

/// Trims and maps a blank string to `None`, so a feed that sends `"title": ""` is treated the same
/// as one that omits the field.
fn nonempty(s: &str) -> Option<String> {
    crate::text::trim_to_none(s)
}

fn convert_item(item: Item) -> Option<RawItem> {
    // A summary is a body only when the item carried no content of its own.
    let content_text = item
        .content_text
        .as_deref()
        .and_then(nonempty)
        .or_else(|| item.summary.as_deref().and_then(nonempty));

    let mut raw = RawItem {
        // A JSON Feed `id` is explicitly "not a permalink" — it is an opaque identifier — so
        // `guid_is_permalink` stays false and the id never becomes the article's URL.
        guid: item.id.map(|g| crate::guid::safe_ascii(g.trim())),
        guid_is_permalink: false,
        title: item.title.as_deref().and_then(nonempty),
        link: item
            .url
            .or(item.external_url)
            .as_deref()
            .filter(|u| is_remote_uri(u))
            .and_then(nonempty),
        date: item.date_published.as_deref().and_then(date::parse_date),
        updated: item.date_modified.as_deref().and_then(date::parse_date),
        content_html: item.content_html.as_deref().and_then(nonempty),
        content_text,
        authors: person_names(item.authors.or_else(|| item.author.map(|a| vec![a]))),
        tags: item
            .tags
            .unwrap_or_default()
            .iter()
            .filter_map(|t| nonempty(t))
            .collect(),
        enclosures: item
            .attachments
            .unwrap_or_default()
            .into_iter()
            .filter_map(convert_attachment)
            .collect(),
        thumbnail: item
            .image
            .or(item.banner_image)
            .as_deref()
            .filter(|u| is_remote_uri(u))
            .and_then(nonempty)
            .map(|url| RawThumbnail {
                url,
                height: None,
                width: None,
                time: None,
            }),
    };

    unique_in_place(&mut raw.tags);
    (!raw.is_empty()).then_some(raw)
}

fn person_names(authors: Option<Vec<Person>>) -> Vec<String> {
    authors
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.name.or(p.url))
        .filter_map(|n| nonempty(&n))
        .collect()
}

fn convert_attachment(a: Attachment) -> Option<RawEnclosure> {
    let url = a
        .url
        .as_deref()
        .filter(|u| is_remote_uri(u))
        .and_then(nonempty)?;
    // Lowercased before the medium is derived, so `AUDIO/MPEG` yields `audio` and not `AUDIO`.
    let mime = a
        .mime_type
        .as_deref()
        .and_then(nonempty)
        .map(|m| m.to_ascii_lowercase());
    Some(RawEnclosure {
        medium: mime_top_level(mime.as_deref()),
        url,
        mime,
        length: a.size_in_bytes.filter(|n| *n > 0),
        title: a.title.as_deref().and_then(nonempty),
        description: None,
        height: None,
        width: None,
    })
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    home_page_url: Option<String>,
    #[serde(default)]
    feed_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    user_comment: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    favicon: Option<String>,
    /// JSON Feed 1.1's plural form.
    #[serde(default)]
    hubs: Option<Vec<Hub>>,
    #[serde(default)]
    items: Option<Vec<Item>>,
}

#[derive(Debug, Deserialize)]
struct Hub {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    external_url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content_html: Option<String>,
    #[serde(default)]
    content_text: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    banner_image: Option<String>,
    #[serde(default)]
    date_published: Option<String>,
    #[serde(default)]
    date_modified: Option<String>,
    /// JSON Feed 1.1's plural form.
    #[serde(default)]
    authors: Option<Vec<Person>>,
    /// JSON Feed 1.0's singular form.
    #[serde(default)]
    author: Option<Person>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Deserialize)]
struct Person {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Attachment {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    size_in_bytes: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(items: &str) -> String {
        format!(
            r#"{{
              "version": "https://jsonfeed.org/version/1.1",
              "title": "Example JSON Feed",
              "home_page_url": "https://example.com/",
              "feed_url": "https://example.com/feed.json",
              "description": "An example",
              "icon": "https://example.com/icon.png",
              "favicon": "https://example.com/favicon.ico",
              "hubs": [{{"type": "WebSub", "url": "https://hub.example.com"}}],
              "items": [{items}]
            }}"#
        )
    }

    fn one(item: &str) -> RawItem {
        let feed = parse(&feed(item)).unwrap();
        assert_eq!(feed.items.len(), 1, "expected exactly one item");
        feed.items.into_iter().next().unwrap()
    }

    #[test]
    fn feed_metadata_is_read() {
        let f = parse(&feed("")).unwrap();
        assert_eq!(f.title.as_deref(), Some("Example JSON Feed"));
        assert_eq!(f.website.as_deref(), Some("https://example.com/"));
        assert_eq!(f.self_url.as_deref(), Some("https://example.com/feed.json"));
        assert_eq!(f.description.as_deref(), Some("An example"));
        // `icon` wins over `favicon`, matching the plan's mapping.
        assert_eq!(f.icon.as_deref(), Some("https://example.com/icon.png"));
        assert_eq!(f.hub_url.as_deref(), Some("https://hub.example.com"));
        assert_eq!(f.format, FeedFormat::JsonFeed);
    }

    #[test]
    fn favicon_is_used_when_there_is_no_icon() {
        let body = r#"{"version":"https://jsonfeed.org/version/1.1","favicon":"https://example.com/f.ico","items":[]}"#;
        assert_eq!(
            parse(body).unwrap().icon.as_deref(),
            Some("https://example.com/f.ico")
        );
    }

    #[test]
    fn the_basic_item_shape() {
        let item = one(
            r#"{"id":"abc","url":"https://example.com/a","title":"Hello",
                "content_html":"<p>Body</p>","date_published":"2006-01-02T15:04:05-05:00"}"#,
        );
        assert_eq!(item.guid.as_deref(), Some("abc"));
        assert!(
            !item.guid_is_permalink,
            "a JSON Feed id is never a permalink"
        );
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
        assert_eq!(item.title.as_deref(), Some("Hello"));
        assert_eq!(item.date, Some(1_136_232_245));
        assert_eq!(item.effective_content(), Some("<p>Body</p>"));
    }

    #[test]
    fn content_html_outranks_content_text() {
        let item =
            one(r#"{"id":"1","title":"T","content_html":"<p>full</p>","content_text":"plain"}"#);
        assert_eq!(item.content_html.as_deref(), Some("<p>full</p>"));
        assert_eq!(item.content_text.as_deref(), Some("plain"));
        assert_eq!(item.effective_content(), Some("<p>full</p>"));
    }

    #[test]
    fn content_text_alone_is_a_body() {
        let item = one(r#"{"id":"1","title":"T","content_text":"just text"}"#);
        assert_eq!(item.effective_content(), Some("just text"));
    }

    #[test]
    fn summary_is_a_body_only_when_there_is_no_content() {
        let with_content = one(r#"{"id":"1","title":"T","content_text":"real","summary":"short"}"#);
        assert_eq!(with_content.effective_content(), Some("real"));

        let summary_only = one(r#"{"id":"1","title":"T","summary":"short"}"#);
        assert_eq!(summary_only.effective_content(), Some("short"));
    }

    #[test]
    fn external_url_is_the_link_when_there_is_no_url() {
        let item = one(r#"{"id":"1","title":"T","external_url":"https://other.example/x"}"#);
        assert_eq!(item.link.as_deref(), Some("https://other.example/x"));
    }

    #[test]
    fn url_outranks_external_url() {
        let item = one(
            r#"{"id":"1","title":"T","url":"https://example.com/a","external_url":"https://other.example/x"}"#,
        );
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn date_modified_becomes_updated() {
        let item = one(
            r#"{"id":"1","title":"T","date_published":"2006-01-02T15:04:05Z","date_modified":"2006-01-03T15:04:05Z"}"#,
        );
        assert_eq!(item.date, Some(1_136_214_245));
        assert_eq!(item.updated, Some(1_136_300_645));
    }

    #[test]
    fn an_unparseable_date_is_none() {
        let item = one(r#"{"id":"1","title":"T","date_published":"whenever"}"#);
        assert_eq!(item.date, None);
    }

    #[test]
    fn version_1_1_plural_authors_are_read() {
        let item = one(
            r#"{"id":"1","title":"T","authors":[{"name":"Ada"},{"name":"Grace","url":"https://g.example"}]}"#,
        );
        assert_eq!(item.authors, vec!["Ada", "Grace"]);
    }

    #[test]
    fn version_1_0_singular_author_is_read() {
        let item = one(r#"{"id":"1","title":"T","author":{"name":"Ada"}}"#);
        assert_eq!(item.authors, vec!["Ada"]);
    }

    #[test]
    fn an_author_with_no_name_falls_back_to_their_url() {
        let item = one(r#"{"id":"1","title":"T","authors":[{"url":"https://ada.example"}]}"#);
        assert_eq!(item.authors, vec!["https://ada.example"]);
    }

    #[test]
    fn tags_are_kept_in_order_without_duplicates() {
        let item = one(r#"{"id":"1","title":"T","tags":["rust","rss","rust"," "]}"#);
        assert_eq!(item.tags, vec!["rust", "rss"]);
    }

    #[test]
    fn attachments_become_enclosures() {
        let item = one(r#"{"id":"1","title":"T","attachments":[
                 {"url":"https://example.com/e.mp3","mime_type":"AUDIO/MPEG",
                  "title":"Ep 1","size_in_bytes":1024,"duration_in_seconds":600}]}"#);
        assert_eq!(item.enclosures.len(), 1);
        let e = &item.enclosures[0];
        assert_eq!(e.url, "https://example.com/e.mp3");
        assert_eq!(e.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(
            e.medium.as_deref(),
            Some("audio"),
            "derived from the MIME type"
        );
        assert_eq!(e.title.as_deref(), Some("Ep 1"));
        assert_eq!(e.length, Some(1024));
    }

    #[test]
    fn a_non_remote_attachment_is_dropped() {
        let item = one(r#"{"id":"1","title":"T","attachments":[{"url":"file:///etc/passwd"}]}"#);
        assert!(item.enclosures.is_empty());
    }

    #[test]
    fn image_becomes_the_thumbnail() {
        let item = one(r#"{"id":"1","title":"T","image":"https://example.com/i.jpg"}"#);
        assert_eq!(
            item.thumbnail.as_ref().map(|t| t.url.as_str()),
            Some("https://example.com/i.jpg")
        );

        let banner = one(r#"{"id":"1","title":"T","banner_image":"https://example.com/b.jpg"}"#);
        assert_eq!(
            banner.thumbnail.as_ref().map(|t| t.url.as_str()),
            Some("https://example.com/b.jpg")
        );
    }

    #[test]
    fn a_non_remote_image_is_not_a_thumbnail() {
        let item = one(r#"{"id":"1","title":"T","image":"data:image/png;base64,AAAA"}"#);
        assert_eq!(item.thumbnail, None);
    }

    #[test]
    fn an_item_with_no_title_link_or_content_is_dropped() {
        let f = parse(&feed(
            r#"{"id":"1","date_published":"2006-01-02T15:04:05Z"}"#,
        ))
        .unwrap();
        assert!(f.items.is_empty());
    }

    #[test]
    fn a_missing_id_leaves_the_guid_absent_rather_than_empty() {
        let item = one(r#"{"title":"T"}"#);
        assert_eq!(item.guid, None, "so the fallback chain can synthesize one");
    }

    #[test]
    fn unknown_top_level_and_item_fields_are_ignored() {
        let item = one(r#"{"id":"1","title":"T","_blue_shed":{"deep":"value"},"language":"en"}"#);
        assert_eq!(item.title.as_deref(), Some("T"));
    }

    #[test]
    fn items_keep_their_document_order() {
        let f = parse(&feed(
            r#"{"id":"1","title":"newest"},{"id":"2","title":"oldest"}"#,
        ))
        .unwrap();
        let titles: Vec<_> = f.items.iter().filter_map(|i| i.title.as_deref()).collect();
        assert_eq!(titles, vec!["newest", "oldest"]);
    }

    #[test]
    fn a_versionless_document_with_items_is_still_a_feed() {
        let f = parse(r#"{"title":"T","items":[{"id":"1","title":"x"}]}"#).unwrap();
        assert_eq!(f.items.len(), 1);
        assert_eq!(f.format, FeedFormat::JsonFeed);
    }

    #[test]
    fn an_items_only_document_is_a_feed() {
        let f = parse(r#"{"items":[]}"#).unwrap();
        assert!(f.items.is_empty());
    }

    #[test]
    fn a_json_document_that_is_not_a_feed_is_an_error() {
        let err = parse(r#"{"status":"ok","results":[]}"#).unwrap_err();
        assert!(err.to_string().contains("not a JSON Feed"), "{err}");
    }

    #[test]
    fn malformed_json_is_an_error() {
        let err = parse("{not json").unwrap_err();
        assert!(err.to_string().contains("invalid JSON Feed"), "{err}");
    }

    #[test]
    fn a_json_array_at_the_root_is_an_error() {
        assert!(parse("[1,2,3]").is_err());
    }
}
