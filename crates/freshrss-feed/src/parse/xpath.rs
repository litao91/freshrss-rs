//! Scraping a feed out of a document with XPath expressions.
//!
//! This is the reader behind feed kinds 10 (HTML+XPath) and 15 (XML+XPath): a page that is not a
//! feed, plus a map of expressions saying which part of it is. The vocabulary is RSS-Bridge's, so a
//! bridge's expressions can be pasted into a feed's configuration and mean the same thing.
//!
//! PHP FreshRSS scrapes by evaluating the expressions, rendering the results into an RSS document
//! with a view template, and then handing that document to SimplePie. Going straight from the
//! expressions to [`RawItem`]s skips a serialize-and-reparse round trip whose only job was to reuse a
//! parser, and removes the HTML-encoding that the round trip forced on every field.

use std::collections::BTreeMap;

use sha1::{Digest as _, Sha1};
use xpath_eval::{Expr, Node as _, Value};

use super::date::scraped_timestamp;
use super::dom::{Doc, NodeRef};
use crate::error::FeedError;
use crate::raw::{FeedFormat, RawFeed, RawItem, RawThumbnail};
use crate::text::normalize_space;

/// Scrapes a document with a selector map.
///
/// `url` is the URL the document came from, used to resolve relative links and as the feed's own
/// website. `fallback_title` is the subscription's configured name, which is what the feed is called
/// when the map has no `feedTitle` expression or that expression matches nothing.
///
/// # Errors
///
/// - [`FeedError::BadConfig`] when the map has no `item` expression, or when any expression is not
///   valid XPath. Both are things the user can fix, and both must be distinguishable from a page that
///   merely changed shape.
/// - [`FeedError::Empty`] when the `item` expression matched nothing. This is the common failure of a
///   scraper — the site was redesigned — and it is reported as "no items" so the refresh pipeline
///   treats it the way it treats any other feed that stopped producing articles.
pub fn scrape(
    doc: &Doc,
    map: &BTreeMap<String, String>,
    url: &str,
    fallback_title: &str,
) -> Result<RawFeed, FeedError> {
    let map = Compiled::compile(map)?;
    let Value::NodeSet(items) = doc.evaluate(&map.item, doc.root())? else {
        return Err(FeedError::Empty);
    };
    if items.is_empty() {
        return Err(FeedError::Empty);
    }

    let base_url = doc.base_href(url);
    let title = match &map.feed_title {
        Some(expr) => {
            let found = text(doc, expr, doc.root())?;
            if found.is_empty() {
                fallback_title.to_owned()
            } else {
                found
            }
        }
        None => fallback_title.to_owned(),
    };

    let mut raw_items = Vec::with_capacity(items.len());
    for item in &items {
        if let Some(item) = scrape_item(doc, &map, *item)? {
            raw_items.push(item);
        }
    }

    Ok(RawFeed {
        title: (!title.is_empty()).then_some(title),
        base_url: Some(base_url),
        website: Some(url.to_owned()),
        description: None,
        icon: None,
        self_url: None,
        hub_url: None,
        effective_url: Some(url.to_owned()),
        items: raw_items,
        format: FeedFormat::Scraped,
    })
}

/// Every expression in the map, compiled once.
///
/// Compiling per item would re-lex the same dozen expressions for every article on the page, and a
/// page with two hundred items is not unusual.
struct Compiled {
    item: Expr,
    feed_title: Option<Expr>,
    title: Option<Expr>,
    content: Option<Expr>,
    uri: Option<Expr>,
    author: Option<Expr>,
    timestamp: Option<Expr>,
    /// Not an expression: a PHP `DateTime::createFromFormat` pattern.
    time_format: String,
    thumbnail: Option<Expr>,
    categories: Option<Expr>,
    uid: Option<Expr>,
}

impl Compiled {
    fn compile(map: &BTreeMap<String, String>) -> Result<Self, FeedError> {
        let Some(item) = expression(map, "item") else {
            return Err(FeedError::BadConfig(
                "the XPath map has no `item` expression, so there is nothing to scrape".to_owned(),
            ));
        };
        Ok(Compiled {
            item: Doc::compile(item)?,
            feed_title: normalized(map, "feedTitle")?,
            title: normalized(map, "itemTitle")?,
            content: raw_expression(map, "itemContent")?,
            uri: normalized(map, "itemUri")?,
            author: normalized(map, "itemAuthor")?,
            timestamp: normalized(map, "itemTimestamp")?,
            time_format: expression(map, "itemTimeFormat")
                .unwrap_or_default()
                .to_owned(),
            thumbnail: normalized(map, "itemThumbnail")?,
            categories: raw_expression(map, "itemCategories")?,
            uid: normalized(map, "itemUid")?,
        })
    }
}

/// Reads one item out of the document.
///
/// Returns `None` for an item with no title, no body and no link, which is what PHP does: an empty
/// element in the page would otherwise become a blank article with nothing to open.
fn scrape_item(doc: &Doc, map: &Compiled, node: NodeRef<'_>) -> Result<Option<RawItem>, FeedError> {
    let title = optional_text(doc, map.title.as_ref(), node)?;
    let link = optional_text(doc, map.uri.as_ref(), node)?;
    let content = optional_content(doc, map.content.as_ref(), node)?;
    if title.is_none() && link.is_none() && content.is_none() {
        return Ok(None);
    }

    let author = optional_text(doc, map.author.as_ref(), node)?;
    let thumbnail = optional_text(doc, map.thumbnail.as_ref(), node)?;
    let timestamp = optional_text(doc, map.timestamp.as_ref(), node)?;
    let uid = optional_text(doc, map.uid.as_ref(), node)?;

    Ok(Some(RawItem {
        // A scraped page has no permalink semantics to inherit, so a synthesized guid is never one.
        guid: Some(uid.unwrap_or_else(|| {
            synthetic_guid(
                title.as_deref().unwrap_or_default(),
                content.as_deref().unwrap_or_default(),
                link.as_deref().unwrap_or_default(),
            )
        })),
        guid_is_permalink: false,
        title,
        link,
        date: timestamp.and_then(|value| scraped_timestamp(&value, &map.time_format)),
        updated: None,
        content_html: content,
        content_text: None,
        authors: author.into_iter().collect(),
        enclosures: Vec::new(),
        tags: optional_tags(doc, map.categories.as_ref(), node)?,
        thumbnail: thumbnail.map(|url| RawThumbnail {
            url,
            height: None,
            width: None,
            time: None,
        }),
    }))
}

/// The identity PHP synthesizes when the map has no `itemUid`, or it matched nothing.
///
/// Without this, every article on the page would have the same (empty) guid and collapse into one
/// row, so a scraper with no uid expression still gets a stable identity: the same title, body and
/// link produce the same guid on every refresh.
fn synthetic_guid(title: &str, content: &str, link: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(title.as_bytes());
    digest.update(content.as_bytes());
    digest.update(link.as_bytes());
    format!("urn:sha1:{}", hex::encode(digest.finalize()))
}

/// Evaluates an expression already wrapped in `normalize-space`, and yields `None` for an empty
/// result so that "the expression matched nothing" and "it matched an empty string" both mean
/// "this field is absent".
///
/// # Errors
///
/// Passes through [`Doc::evaluate`]'s. An expression that cannot be evaluated is not an empty
/// field: swallowing it would give every article on the page a missing title and no explanation,
/// which is the failure a user cannot diagnose.
fn optional_text(
    doc: &Doc,
    expr: Option<&Expr>,
    node: NodeRef<'_>,
) -> Result<Option<String>, FeedError> {
    let Some(expr) = expr else { return Ok(None) };
    let value = text(doc, expr, node)?;
    Ok((!value.is_empty()).then_some(value))
}

fn text(doc: &Doc, expr: &Expr, node: NodeRef<'_>) -> Result<String, FeedError> {
    Ok(doc.evaluate(expr, node)?.to_xpath_string())
}

/// The article body, which is the one field that is *not* normalized.
///
/// A node-set is serialized as markup, because a scraped article's images, links and formatting are
/// the point of scraping it; a string, number or boolean result is taken as-is, which is what lets an
/// expression like `string(./div[@class='body'])` and one like `count(./p)` both be useful.
fn optional_content(
    doc: &Doc,
    expr: Option<&Expr>,
    node: NodeRef<'_>,
) -> Result<Option<String>, FeedError> {
    let Some(expr) = expr else { return Ok(None) };
    let value = match doc.evaluate(expr, node)? {
        Value::NodeSet(nodes) => nodes
            .iter()
            .map(|found| doc.serialize(*found))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_xpath_string(),
    };
    Ok((!value.trim().is_empty()).then_some(value))
}

/// The categories expression, which yields either one string or a node per tag.
///
/// Values are whitespace-normalized and blanks dropped. PHP keeps whatever `textContent` produced,
/// so a `<span>\n  News\n</span>` becomes a tag whose name is `"\n  News\n"` — and since tags are
/// keyed by name, that and `"News"` become two different tags in the sidebar.
fn optional_tags(
    doc: &Doc,
    expr: Option<&Expr>,
    node: NodeRef<'_>,
) -> Result<Vec<String>, FeedError> {
    let Some(expr) = expr else {
        return Ok(Vec::new());
    };
    let values = match doc.evaluate(expr, node)? {
        Value::NodeSet(nodes) => nodes.iter().map(|found| found.string_value()).collect(),
        other => vec![other.to_xpath_string()],
    };
    Ok(values
        .iter()
        .map(|value| normalize_space(value).into_owned())
        .filter(|value| !value.is_empty())
        .collect())
}

/// A map entry, trimmed, or `None` when absent or blank.
fn expression<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Compiles a scalar expression, wrapped in `normalize-space` the way PHP wraps it.
///
/// Wrapping textually rather than normalizing the result afterwards keeps one behaviour for every
/// expression a user can write, including the ones that return a node-set: `normalize-space` takes
/// the first node in document order, which is what a person reading `itemTitle` expects.
fn normalized(map: &BTreeMap<String, String>, key: &str) -> Result<Option<Expr>, FeedError> {
    match expression(map, key) {
        Some(expression) => Doc::compile(&format!("normalize-space({expression})")).map(Some),
        None => Ok(None),
    }
}

fn raw_expression(map: &BTreeMap<String, String>, key: &str) -> Result<Option<Expr>, FeedError> {
    match expression(map, key) {
        Some(expression) => Doc::compile(expression).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::kind::FeedKind;

    const PAGE: &str = r#"<html><head><base href="https://e.example/blog/"></head><body>
<h1>Café &amp; News</h1>
<article class="post" data-id="a1">
	<h2>  First   post  </h2>
	<a class="permalink" href="first">Read</a>
	<span class="author">Alice</span>
	<time datetime="2026-10-01T08:00:00Z">October</time>
	<img class="thumb" src="/img/1.png">
	<ul class="tags"><li>News</li><li>  Café  </li><li>  </li></ul>
	<div class="body"><p>One</p><p>Two</p></div>
</article>
<article class="post" data-id="a2">
	<h2>Second post</h2>
	<a class="permalink" href="https://other.example/second">Read</a>
	<div class="body"><p>Body</p></div>
</article>
<article class="post">
	<h2>   </h2>
</article>
</body></html>"#;

    /// The map an RSS-Bridge-style configuration would carry.
    fn bridge_map() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("feedTitle".to_owned(), "//h1".to_owned()),
            ("item".to_owned(), "//article[@class='post']".to_owned()),
            ("itemTitle".to_owned(), "./h2".to_owned()),
            ("itemContent".to_owned(), "./div[@class='body']".to_owned()),
            (
                "itemUri".to_owned(),
                "./a[@class='permalink']/@href".to_owned(),
            ),
            (
                "itemAuthor".to_owned(),
                "./span[@class='author']".to_owned(),
            ),
            ("itemTimestamp".to_owned(), "./time/@datetime".to_owned()),
            ("itemThumbnail".to_owned(), "./img/@src".to_owned()),
            (
                "itemCategories".to_owned(),
                "./ul[@class='tags']/li".to_owned(),
            ),
            ("itemUid".to_owned(), "./@data-id".to_owned()),
        ])
    }

    fn scrape_page(page: &str, map: &BTreeMap<String, String>) -> Result<RawFeed, FeedError> {
        let doc = Doc::from_html(page);
        scrape(
            &doc,
            map,
            "https://e.example/blog/index.html",
            "Configured name",
        )
    }

    /// The public entry point, which is what the refresh pipeline calls: bytes in, `RawFeed` out.
    fn scrape_html(body: &str, map: &BTreeMap<String, String>) -> Result<RawFeed, FeedError> {
        entry(body.as_bytes(), FeedKind::HtmlXpath, map)
    }

    fn entry(
        body: &[u8],
        kind: FeedKind,
        map: &BTreeMap<String, String>,
    ) -> Result<RawFeed, FeedError> {
        crate::parse::scrape(
            body,
            &crate::parse::Scrape {
                kind,
                map,
                x_path_to_json: "",
                url: "https://e.example/blog/index.html",
                fallback_title: "Configured name",
            },
        )
    }

    #[test]
    fn scrapes_the_whole_map() {
        let feed = scrape_page(PAGE, &bridge_map()).unwrap();
        assert_eq!(feed.format, FeedFormat::Scraped);
        assert_eq!(feed.title.as_deref(), Some("Café & News"));
        assert_eq!(feed.base_url.as_deref(), Some("https://e.example/blog/"));
        assert_eq!(
            feed.website.as_deref(),
            Some("https://e.example/blog/index.html")
        );
        assert_eq!(feed.items.len(), 2, "the third article has nothing in it");

        let first = &feed.items[0];
        assert_eq!(first.title.as_deref(), Some("First post"), "normalized");
        assert_eq!(first.link.as_deref(), Some("first"), "left relative");
        assert_eq!(first.authors, ["Alice"]);
        assert_eq!(first.date, Some(1_790_841_600), "2026-10-01T08:00:00Z");
        assert_eq!(first.guid.as_deref(), Some("a1"));
        assert_eq!(
            first.thumbnail.as_ref().map(|t| t.url.as_str()),
            Some("/img/1.png")
        );
        assert_eq!(
            first.tags,
            ["News", "Café"],
            "normalized, and the blank one dropped"
        );
        assert_eq!(
            first.content_html.as_deref(),
            Some("<div class=\"body\"><p>One</p><p>Two</p></div>"),
            "a node-set is serialized as markup"
        );
        assert!(!first.guid_is_permalink);
    }

    #[test]
    fn missing_optional_fields_are_absent_not_empty() {
        let second = &scrape_page(PAGE, &bridge_map()).unwrap().items[1];
        assert_eq!(second.title.as_deref(), Some("Second post"));
        assert_eq!(second.link.as_deref(), Some("https://other.example/second"));
        assert!(second.authors.is_empty());
        assert_eq!(second.date, None);
        assert_eq!(second.thumbnail, None);
        assert!(second.tags.is_empty());
        assert_eq!(second.guid.as_deref(), Some("a2"));
    }

    #[test]
    fn a_missing_uid_expression_synthesizes_a_stable_guid() {
        let mut map = bridge_map();
        map.remove("itemUid");
        let feed = scrape_page(PAGE, &map).unwrap();
        let first = feed.items[0].guid.as_deref().expect("a guid");
        assert!(first.starts_with("urn:sha1:"), "{first}");
        assert_eq!(first.len(), "urn:sha1:".len() + 40, "{first}");

        // The same page scrapes to the same guids, which is what keeps read state across refreshes.
        let again = scrape_page(PAGE, &map).unwrap();
        assert_eq!(again.items[0].guid.as_deref(), Some(first));
        assert_ne!(
            again.items[1].guid.as_deref(),
            Some(first),
            "different articles get different guids"
        );
    }

    #[test]
    fn the_title_falls_back_to_the_configured_name() {
        let mut without = bridge_map();
        without.remove("feedTitle");
        assert_eq!(
            scrape_page(PAGE, &without).unwrap().title.as_deref(),
            Some("Configured name")
        );

        let mut matching_nothing = bridge_map();
        matching_nothing.insert("feedTitle".to_owned(), "//h9".to_owned());
        assert_eq!(
            scrape_page(PAGE, &matching_nothing)
                .unwrap()
                .title
                .as_deref(),
            Some("Configured name"),
            "an expression that matches nothing is the same as no expression"
        );
    }

    #[test]
    fn a_content_expression_returning_a_string_is_taken_as_is() {
        let mut map = bridge_map();
        map.insert(
            "itemContent".to_owned(),
            "string(./div[@class='body'])".to_owned(),
        );
        let first = &scrape_page(PAGE, &map).unwrap().items[0];
        assert_eq!(first.content_html.as_deref(), Some("OneTwo"));
    }

    #[test]
    fn several_matched_nodes_are_concatenated_in_document_order() {
        let mut map = bridge_map();
        map.insert(
            "itemContent".to_owned(),
            "./div[@class='body']/p".to_owned(),
        );
        let first = &scrape_page(PAGE, &map).unwrap().items[0];
        assert_eq!(
            first.content_html.as_deref(),
            Some("<p>One</p>\n<p>Two</p>")
        );
    }

    /// A page with one article carrying `timestamp` as its date, for the format tests.
    fn dated(timestamp: &str) -> String {
        format!(
            r#"<html><body><article><h2>t</h2><span class="d">{timestamp}</span></article></body></html>"#
        )
    }

    fn dated_map() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("item".to_owned(), "//article".to_owned()),
            ("itemTitle".to_owned(), "./h2".to_owned()),
            ("itemTimestamp".to_owned(), "./span[@class='d']".to_owned()),
            ("itemUid".to_owned(), "./h2".to_owned()),
        ])
    }

    #[test]
    fn a_time_format_decides_how_the_timestamp_is_read() {
        // Without a format, the heuristic parser the feed parsers use reads it.
        let page = dated("01/10/2026 08:30");
        assert_eq!(
            scrape_page(&page, &dated_map()).unwrap().items[0].date,
            None,
            "an ambiguous slash date is not guessed at"
        );

        let mut map = dated_map();
        map.insert("itemTimeFormat".to_owned(), "d/m/Y H:i".to_owned());
        assert_eq!(
            scrape_page(&page, &map).unwrap().items[0].date,
            Some(1_790_843_400),
            "1 October 2026 08:30, read as UTC because the pattern carries no offset"
        );

        // A format that does not fit falls back to the heuristic parser, which is what PHP does when
        // `createFromFormat` fails: `itemTimeFormat` is free text and users get it wrong.
        map.insert("itemTimeFormat".to_owned(), "Y-m-d".to_owned());
        let fallback = dated("2026-10-01T08:30:00Z");
        assert_eq!(
            scrape_page(&fallback, &map).unwrap().items[0].date,
            Some(1_790_843_400),
            "the wrong format cost nothing because the value was parsable anyway"
        );

        // And when neither the format nor the heuristic can read it, there is no date rather than a
        // wrong one.
        assert_eq!(scrape_page(&page, &map).unwrap().items[0].date, None);
    }

    #[test]
    fn unix_seconds_and_milliseconds_both_work_under_format_u() {
        let page = r#"<html><body><article><h2>t</h2><b>1790757000</b></article><article><h2>u</h2><b>1790757000123</b></article></body></html>"#;
        let mut map = bridge_map();
        map.insert("item".to_owned(), "//article".to_owned());
        map.insert("itemTimestamp".to_owned(), "./b".to_owned());
        map.insert("itemUid".to_owned(), "./h2".to_owned());
        map.insert("itemTimeFormat".to_owned(), "U".to_owned());
        let feed = scrape_page(page, &map).unwrap();
        assert_eq!(feed.items[0].date, Some(1_790_757_000));
        assert_eq!(
            feed.items[1].date,
            Some(1_790_757_000),
            "milliseconds need no `drop the last three characters` special case"
        );
    }

    #[test]
    fn an_item_with_nothing_in_it_is_skipped() {
        let page = r#"<html><body><article class="post"></article><article class="post"><h2>x</h2></article></body></html>"#;
        let mut map = bridge_map();
        map.insert("item".to_owned(), "//article".to_owned());
        map.insert("itemTitle".to_owned(), "./h2".to_owned());
        let feed = scrape_page(page, &map).unwrap();
        assert_eq!(feed.items.len(), 1);
        assert_eq!(feed.items[0].title.as_deref(), Some("x"));
    }

    #[test]
    fn a_map_with_no_item_expression_is_a_configuration_error() {
        let mut map = bridge_map();
        map.remove("item");
        let err = scrape_page(PAGE, &map).unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");
        assert!(err.to_string().contains("item"), "{err}");

        let mut blank = bridge_map();
        blank.insert("item".to_owned(), "   ".to_owned());
        assert!(matches!(
            scrape_page(PAGE, &blank).unwrap_err(),
            FeedError::BadConfig(_)
        ));
    }

    #[test]
    fn an_item_expression_that_matches_nothing_is_an_empty_feed() {
        let mut map = bridge_map();
        map.insert("item".to_owned(), "//section".to_owned());
        assert!(matches!(
            scrape_page(PAGE, &map).unwrap_err(),
            FeedError::Empty
        ));
    }

    #[test]
    fn one_bad_expression_is_reported_rather_than_ignored() {
        let mut map = bridge_map();
        map.insert("itemTitle".to_owned(), "./h2[".to_owned());
        let err = scrape_page(PAGE, &map).unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");

        // An expression is compiled once for the whole page, so the error surfaces even for a page
        // whose items would never have reached it.
        let mut unused = bridge_map();
        unused.insert("itemAuthor".to_owned(), "nosuchfn()".to_owned());
        assert!(matches!(
            scrape_page(PAGE, &unused).unwrap_err(),
            FeedError::BadConfig(_)
        ));
    }

    #[test]
    fn xml_documents_are_scraped_the_same_way() {
        let xml = r#"<?xml version="1.0"?>
<catalog xmlns:dc="http://purl.org/dc/elements/1.1/">
	<entry><dc:title>One</dc:title><link href="/1"/><dc:date>2026-10-01T00:00:00Z</dc:date></entry>
	<entry><dc:title>Two</dc:title><link href="/2"/></entry>
</catalog>"#;
        let doc = Doc::from_xml(xml).unwrap();
        let map = BTreeMap::from([
            ("item".to_owned(), "//entry".to_owned()),
            ("itemTitle".to_owned(), "./dc:title".to_owned()),
            ("itemUri".to_owned(), "./link/@href".to_owned()),
            ("itemTimestamp".to_owned(), "./dc:date".to_owned()),
        ]);
        let feed = scrape(&doc, &map, "https://e.example/catalog.xml", "Catalog").unwrap();
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.items[0].title.as_deref(), Some("One"));
        assert_eq!(feed.items[0].link.as_deref(), Some("/1"));
        assert_eq!(feed.items[0].date, Some(1_790_812_800));
        assert_eq!(feed.items[1].date, None);
        assert_eq!(feed.title.as_deref(), Some("Catalog"));
    }

    #[test]
    fn the_public_entry_point_scrapes_html_and_refuses_the_other_kinds() {
        let feed = scrape_html(PAGE, &bridge_map()).unwrap();
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.title.as_deref(), Some("Café & News"));

        // The syndication formats go through `parse`, and kind 20 is vestigial: neither is an XPath
        // scraper, and saying so beats a confusing "unrecognized XML root element <html>".
        for kind in [
            FeedKind::Rss,
            FeedKind::RssForced,
            FeedKind::JsonFeed,
            FeedKind::JsonXpath,
        ] {
            let err = entry(PAGE.as_bytes(), kind, &bridge_map()).unwrap_err();
            assert!(
                matches!(err, FeedError::UnsupportedKind(_)),
                "{kind:?}: {err}"
            );
        }

        // An empty body is not a configuration problem.
        let err = entry(b"", FeedKind::HtmlXpath, &bridge_map()).unwrap_err();
        assert!(matches!(err, FeedError::Empty), "{err}");
    }

    #[test]
    fn xml_scraping_through_the_public_entry_point_rejects_malformed_bytes() {
        let err = entry(
            b"<catalog><entry>",
            FeedKind::XmlXpath,
            &BTreeMap::from([("item".to_owned(), "//entry".to_owned())]),
        )
        .unwrap_err();
        assert!(matches!(err, FeedError::Parse(_)), "{err}");
    }

    #[test]
    fn a_scraped_feed_has_no_enclosures_or_summary() {
        let first = &scrape_page(PAGE, &bridge_map()).unwrap().items[0];
        assert!(first.enclosures.is_empty());
        assert_eq!(first.content_text, None);
        assert_eq!(first.updated, None);
    }
}
