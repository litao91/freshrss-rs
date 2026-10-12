//! Turning feed bytes into a [`RawFeed`](crate::raw::RawFeed).
//!
//! The entry point is [`parse`], which sniffs the format and dispatches. Format-specific modules are
//! private to this one because their element handling only makes sense next to each other.

mod atom;
mod common;
pub mod date;
pub(crate) mod dom;
mod dotnotation;
mod jsonfeed;
mod rdf;
mod rss;
pub(crate) mod xml;
mod xpath;

use std::collections::BTreeMap;

use crate::error::FeedError;
use crate::kind::FeedKind;
use crate::raw::{FeedFormat, RawFeed};

pub use date::{parse_date, parse_millis, parse_with_php_format, scraped_timestamp};

/// Parses a feed document.
///
/// `hint` is the feed's configured kind. [`FeedKind::Rss`] means "work it out from the bytes", which
/// is the normal case; the other XML/JSON kinds force a specific reader.
///
/// # Errors
///
/// [`FeedError::Empty`] when the body is empty, [`FeedError::Parse`] when it is not valid UTF-8 or
/// not recognizable as any supported format.
pub fn parse(body: &[u8], hint: FeedKind) -> Result<RawFeed, FeedError> {
    let text = decode_body(body)?;
    let trimmed = text.trim_start();

    match hint {
        FeedKind::JsonFeed => jsonfeed::parse(&text),
        FeedKind::RssForced => parse_xml(&text).or_else(|_| rss::parse(&text)),
        // A scraping kind is not a syndication format, and reading one as if it were would report
        // "unrecognized XML root element <html>" for a page that is perfectly good. [`scrape`] is
        // the entry point for these; it needs the selector map, which this function does not have.
        FeedKind::HtmlXpath
        | FeedKind::XmlXpath
        | FeedKind::JsonXpath
        | FeedKind::JsonDotNotation
        | FeedKind::HtmlXpathJsonDotNotation => Err(FeedError::UnsupportedKind(hint.as_i16())),
        _ if trimmed.starts_with('{') || trimmed.starts_with('[') => jsonfeed::parse(&text),
        _ => parse_xml(&text),
    }
}

/// Everything a scraping kind needs besides the document bytes.
///
/// All of it comes off the feed row, which is why it is one struct rather than five arguments: the
/// refresh pipeline builds it once per feed and hands it to [`scrape`].
#[derive(Debug, Clone)]
pub struct Scrape<'a> {
    /// Which scraper runs, and therefore how the bytes are read.
    pub kind: FeedKind,
    /// The `frss:xPathItem*` or `frss:jsonItem*` map.
    pub map: &'a BTreeMap<String, String>,
    /// `frss:xPathToJson`: the expression that finds the JSON inside an HTML page. Only
    /// [`FeedKind::HtmlXpathJsonDotNotation`] reads it.
    pub x_path_to_json: &'a str,
    /// Where the document came from. Becomes the feed's website and resolves its relative links.
    pub url: &'a str,
    /// The subscription's configured name, used when the map has no `feedTitle` or it finds nothing.
    pub fallback_title: &'a str,
}

/// Scrapes a feed out of a document that is not one.
///
/// The kind decides how the bytes are read and which vocabulary the map uses:
///
/// | kind | bytes read as | map |
/// |---|---|---|
/// | [`FeedKind::HtmlXpath`] | HTML — lenient, names lowercased | `frss:xPathItem*` |
/// | [`FeedKind::XmlXpath`] | XML — strict, namespaces resolved | `frss:xPathItem*` |
/// | [`FeedKind::JsonDotNotation`] | JSON | `frss:jsonItem*` |
/// | [`FeedKind::HtmlXpathJsonDotNotation`] | HTML, then the JSON `x_path_to_json` finds | `frss:jsonItem*` |
///
/// One data model backs both XML and HTML, so the same expressions behave the same way against
/// either; the JSON kinds do not use XPath at all, because a JSON document has no axes and
/// `items.0.title` already says everything an expression would.
///
/// # Errors
///
/// [`FeedError::Empty`] when the body is empty or the item selector matched nothing,
/// [`FeedError::Parse`] when the bytes are not valid UTF-8, not well-formed XML for a kind that
/// requires it, or not valid JSON for one that does, [`FeedError::BadConfig`] when an expression is
/// invalid or a required one is missing, and [`FeedError::UnsupportedKind`] for a kind this function
/// does not scrape — the syndication formats go through [`parse`], and kind 20 is vestigial.
pub fn scrape(body: &[u8], config: &Scrape<'_>) -> Result<RawFeed, FeedError> {
    let text = decode_body(body)?;
    match config.kind {
        FeedKind::HtmlXpath => {
            let document = dom::Doc::from_html(&text);
            xpath::scrape(&document, config.map, config.url, config.fallback_title)
        }
        FeedKind::XmlXpath => {
            let document = dom::Doc::from_xml(&text)?;
            xpath::scrape(&document, config.map, config.url, config.fallback_title)
        }
        FeedKind::JsonDotNotation => {
            let document = parse_json(&text)?;
            dotnotation::scrape(&document, config.map, config.url, config.fallback_title)
        }
        FeedKind::HtmlXpathJsonDotNotation => {
            let document = dom::Doc::from_html(&text);
            let json = json_from_html(&document, config.x_path_to_json)?;
            dotnotation::scrape(&json, config.map, config.url, config.fallback_title)
        }
        other => Err(FeedError::UnsupportedKind(other.as_i16())),
    }
}

/// Reads a JSON document, reporting the position the parser gave.
fn parse_json(text: &str) -> Result<serde_json::Value, FeedError> {
    serde_json::from_str(text).map_err(|err| FeedError::Parse(format!("not valid JSON: {err}")))
}

/// Finds the JSON inside an HTML page with `frss:xPathToJson`.
///
/// An expression that evaluates to a string yields that string as the document. One that matches
/// nodes yields each node's text decoded and collected into an **array**, so a page with a single
/// `<script type="application/ld+json">` produces a one-element array and the map's `item` path has
/// to start at `0.`. That is surprising, and it is also what PHP FreshRSS does, which means the
/// configurations already written against it depend on it.
fn json_from_html(document: &dom::Doc, expression: &str) -> Result<serde_json::Value, FeedError> {
    if expression.trim().is_empty() {
        return Err(FeedError::BadConfig(
            "an HTML+XPath+JSON feed needs an `xPathToJson` expression to find the JSON".to_owned(),
        ));
    }
    let expression = dom::Doc::compile(expression)?;
    use xpath_eval::Node as _;
    match document.evaluate(&expression, document.root())? {
        xpath_eval::Value::String(text) => parse_json(&text),
        xpath_eval::Value::NodeSet(nodes) => {
            let mut found = Vec::new();
            for node in nodes {
                // Only objects and arrays are kept: a node whose text is `42` or `null` is not a
                // document a dot-notation map can read, and PHP's `is_array` check drops it too.
                if let Ok(value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
                    serde_json::from_str(&node.string_value())
                {
                    found.push(value);
                }
            }
            if found.is_empty() {
                return Err(FeedError::Parse(
                    "the `xPathToJson` expression matched no JSON document".to_owned(),
                ));
            }
            Ok(serde_json::Value::Array(found))
        }
        other => parse_json(&other.to_xpath_string()),
    }
}

/// Interprets the body as XML and picks the RSS/Atom/RDF reader from the root element.
fn parse_xml(text: &str) -> Result<RawFeed, FeedError> {
    match sniff_xml_root(text) {
        Some("feed") => atom::parse(text),
        Some("RDF") => rdf::parse(text),
        Some("rss") => rss::parse(text),
        Some(other) => Err(FeedError::Parse(format!(
            "unrecognized XML root element <{other}>"
        ))),
        None => Err(FeedError::Parse("no XML root element found".to_owned())),
    }
}

/// Decodes the body to UTF-8.
///
/// A body that is not valid UTF-8 is an error rather than a lossy conversion. Feeds that declare
/// `windows-1252` do exist, but guessing at a code page turns every accented character into a
/// replacement character — and since titles feed the change-detection hash, that silently marks the
/// whole backlog as updated. A loud failure the user can see is cheaper than that.
fn decode_body(body: &[u8]) -> Result<String, FeedError> {
    if body.is_empty() {
        return Err(FeedError::Empty);
    }
    // Strip a UTF-8 BOM: legal, but it makes the first element name look like "\u{feff}rss".
    let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(body);
    std::str::from_utf8(body)
        .map(str::to_owned)
        .map_err(|_| FeedError::Parse("feed body is not valid UTF-8".to_owned()))
}

/// Finds the local name of the root element without building a document.
///
/// Skips the XML declaration, comments, doctypes and processing instructions. Returns the name with
/// any namespace prefix removed, so `<atom:feed>` and `<feed>` both yield `"feed"`.
fn sniff_xml_root(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        // An unterminated declaration or comment means the document is truncated: there is no root
        // to find, and falling through would report `?xml` as an element name.
        if rest.starts_with("<?") {
            i += rest.find("?>").map(|p| p + 2)?;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").map(|p| p + 3)?;
            continue;
        }
        if rest.starts_with("<!") {
            i += rest.find('>')? + 1;
            continue;
        }
        if rest.starts_with("</") {
            // A close tag before any open tag: there is no root.
            return None;
        }
        return element_name(&rest[1..]).map(local_name);
    }
    None
}

/// Reads an element name from just after its `<`, stopping at whitespace, `>` or `/`.
fn element_name(text: &str) -> Option<&str> {
    let end = text
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || *c == '>' || *c == '/')
        .map_or(text.len(), |(idx, _)| idx);
    (!text[..end].is_empty()).then_some(&text[..end])
}

/// Drops a namespace prefix, so `rdf:RDF` reads as `RDF`.
fn local_name(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((_, local)) if !local.is_empty() => local,
        _ => name,
    }
}

/// The format a document would be parsed as, without parsing it.
pub fn detect_format(body: &[u8]) -> FeedFormat {
    let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(body);
    let Ok(text) = std::str::from_utf8(body) else {
        return FeedFormat::Unknown;
    };
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return FeedFormat::JsonFeed;
    }
    match sniff_xml_root(trimmed) {
        Some("feed") => FeedFormat::Atom,
        Some("RDF") => FeedFormat::Rss1,
        Some("rss") => FeedFormat::Rss2,
        _ => FeedFormat::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_root_elements_past_declarations_and_comments() {
        assert_eq!(
            sniff_xml_root(r#"<?xml version="1.0"?><rss version="2.0">"#),
            Some("rss")
        );
        assert_eq!(sniff_xml_root("\n\n  <feed xmlns='x'>"), Some("feed"));
        assert_eq!(sniff_xml_root("<!-- c --><RDF>"), Some("RDF"));
        assert_eq!(
            sniff_xml_root(r#"<?xml version="1.0"?><?php exit; ?><rss>"#),
            Some("rss")
        );
        assert_eq!(sniff_xml_root("<!DOCTYPE html><html>"), Some("html"));
    }

    #[test]
    fn sniffs_through_a_namespace_prefix() {
        assert_eq!(sniff_xml_root("<atom:feed xmlns:atom='x'>"), Some("feed"));
        assert_eq!(sniff_xml_root("<rdf:RDF xmlns:rdf='x'>"), Some("RDF"));
    }

    #[test]
    fn self_closing_and_attributeless_roots() {
        assert_eq!(sniff_xml_root("<rss/>"), Some("rss"));
        assert_eq!(sniff_xml_root("<rss >"), Some("rss"));
    }

    #[test]
    fn a_document_that_only_closes_has_no_root() {
        assert_eq!(sniff_xml_root("</rss>"), None);
        assert_eq!(sniff_xml_root(""), None);
        assert_eq!(sniff_xml_root("no markup"), None);
        // An unterminated declaration must not loop forever.
        assert_eq!(sniff_xml_root("<?xml version='1.0'"), None);
        assert_eq!(sniff_xml_root("<!-- never closed"), None);
    }

    #[test]
    fn detect_format_recognizes_the_three_xml_roots_and_json() {
        assert_eq!(
            detect_format(b"<rss version=\"2.0\"></rss>"),
            FeedFormat::Rss2
        );
        assert_eq!(
            detect_format(b"<feed xmlns=\"a\"></feed>"),
            FeedFormat::Atom
        );
        assert_eq!(detect_format(b"<rdf:RDF></rdf:RDF>"), FeedFormat::Rss1);
        assert_eq!(
            detect_format(br#"{"version":"https://jsonfeed.org/version/1.1"}"#),
            FeedFormat::JsonFeed
        );
        assert_eq!(detect_format(b"<html></html>"), FeedFormat::Unknown);
        assert_eq!(detect_format(&[0xFF, 0xFE, 0x00]), FeedFormat::Unknown);
    }

    #[test]
    fn a_utf8_bom_is_stripped_before_the_format_is_read() {
        let with_bom = b"\xEF\xBB\xBF<?xml version=\"1.0\"?><rss></rss>";
        assert_eq!(detect_format(with_bom), FeedFormat::Rss2);
        assert_eq!(sniff_xml_root(&decode_body(with_bom).unwrap()), Some("rss"));
    }

    #[test]
    fn an_empty_body_is_an_error_not_an_empty_feed() {
        assert!(matches!(decode_body(b""), Err(FeedError::Empty)));
    }

    #[test]
    fn utf8_bodies_pass_through_unchanged() {
        let body = "<rss><title>Café</title></rss>".as_bytes();
        assert_eq!(decode_body(body).unwrap(), "<rss><title>Café</title></rss>");
    }

    #[test]
    fn a_non_utf8_body_is_a_clear_error_rather_than_mangled_text() {
        let body =
            b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><rss><title>Caf\xE9</title></rss>";
        let err = decode_body(body).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn dispatch_rejects_an_unknown_xml_root() {
        let err = parse(b"<html><body>not a feed</body></html>", FeedKind::Rss).unwrap_err();
        assert!(err.to_string().contains("<html>"), "{err}");
    }

    #[test]
    fn dispatch_rejects_a_document_with_no_root_at_all() {
        let err = parse(b"just some text", FeedKind::Rss).unwrap_err();
        assert!(err.to_string().contains("no XML root"), "{err}");
    }
}
