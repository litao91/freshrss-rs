//! Reading an OPML document into [`Opml`].
//!
//! The reader is deliberately forgiving in the places FreshRSS's is, and strict in one place it is
//! not: a document whose root is not `<opml>` is rejected, a truncated one yields the outlines that
//! were complete, and a `frss:` value we cannot make sense of is dropped rather than guessed at.
//! Dropping is the safe direction for every one of those values, because each of them ends up
//! changing how a feed is fetched or how its entries are identified.

use std::collections::BTreeMap;

use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::Reader;

use super::model::{kind_of_type, Opml, OpmlPriority, Outline, JSON_KEYS, XPATH_KEYS};
use crate::parse::xml::{self, attr, attrs_prefixed, local, skip, text, walk, Attrs};

/// How many `<outline>` levels are descended before the document is rejected.
///
/// OPML files are uploaded and nesting is recursive, so this is what stops a deeply nested document
/// from overflowing the stack. Real exports nest two or three levels; FreshRSS's own importer
/// flattens everything below the first anyway.
const MAX_DEPTH: usize = 64;

/// Builds the reader.
///
/// More forgiving than the feed readers about dangling end tags, and the difference is deliberate.
/// A feed body is fetched again on every TTL, so failing loudly on a malformed one costs nothing and
/// tells the user something true. A subscription list is uploaded once, by hand, and the alternative
/// to importing most of it is importing none of it.
fn reader(text: &str) -> Reader<&[u8]> {
    let mut r = xml::reader(text);
    r.config_mut().allow_unmatched_ends = true;
    r
}

/// Why an OPML document could not be read.
#[derive(Debug, thiserror::Error)]
pub enum OpmlError {
    /// The root element is not `<opml>`, or there is no root element at all.
    #[error("not an OPML document: {0}")]
    NotOpml(String),

    /// The bytes are not well-formed XML.
    #[error("malformed XML: {0}")]
    Malformed(String),

    #[error("outlines are nested deeper than the {limit} level limit", limit = MAX_DEPTH)]
    TooDeep,
}

/// Reads an OPML document.
///
/// The input must already be UTF-8 text: XML declarations naming another encoding are rare in
/// practice for subscription lists, and guessing a code page would corrupt the accented feed names
/// that make a sidebar readable.
///
/// # Errors
///
/// [`OpmlError::NotOpml`] when the root element is something else, [`OpmlError::Malformed`] when
/// the XML cannot be read, [`OpmlError::TooDeep`] for pathological nesting.
pub fn parse(text: &str) -> Result<Opml, OpmlError> {
    let mut r = reader(text);
    let mut doc = Opml::default();

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) if local(e.name()).as_ref() == "opml" => break,
            // `<opml/>` is a well-formed document with nothing in it.
            Ok(Event::Empty(e)) if local(e.name()).as_ref() == "opml" => return Ok(doc),
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                return Err(OpmlError::NotOpml(format!(
                    "root element is <{}>",
                    local(e.name())
                )));
            }
            Ok(Event::Eof) => {
                return Err(OpmlError::NotOpml(
                    "the document has no root element".to_owned(),
                ));
            }
            Err(e) => return Err(OpmlError::Malformed(e.to_string())),
            // Declaration, comment, doctype, processing instruction, whitespace.
            _ => {}
        }
    }

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => match local(e.name()).as_ref() {
                "head" => read_head(&mut r, &mut doc),
                "body" => doc.outlines = outlines(&mut r, "body", 0)?,
                _ => skip(&mut r, e.name()),
            },
            Ok(Event::End(e)) if local(e.name()).as_ref() == "opml" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(OpmlError::Malformed(e.to_string())),
            _ => {}
        }
    }
    Ok(doc)
}

/// Reads `<head>`, keeping the two elements an importer has any use for.
fn read_head(r: &mut Reader<&[u8]>, doc: &mut Opml) {
    walk(r, "head", |name, _, is_empty, r| {
        if is_empty {
            return;
        }
        match local(QName(name)).as_ref() {
            "title" => doc.title = text(r, QName(name)),
            "dateCreated" => doc.date_created = Some(text(r, QName(name))),
            _ => skip(r, QName(name)),
        }
    });
}

/// Collects `<outline>` elements until `</{terminator}>`.
///
/// The terminator is `"body"` at the top level and `"outline"` inside an outline, which is what makes
/// this one function rather than two: an outline's children are read by the same code that reads the
/// document's.
fn outlines(
    r: &mut Reader<&[u8]>,
    terminator: &str,
    depth: usize,
) -> Result<Vec<Outline>, OpmlError> {
    if depth > MAX_DEPTH {
        return Err(OpmlError::TooDeep);
    }
    let mut out = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                if local(e.name()).as_ref() == "outline" {
                    let mut o = outline(&attrs_prefixed(&e));
                    o.children = outlines(r, "outline", depth + 1)?;
                    out.push(o);
                } else {
                    // Not part of the format. Skipping the subtree rather than descending keeps a
                    // stray element from being mistaken for an outline's contents.
                    skip(r, e.name());
                }
            }
            Ok(Event::Empty(e)) => {
                if local(e.name()).as_ref() == "outline" {
                    out.push(outline(&attrs_prefixed(&e)));
                }
            }
            Ok(Event::End(e)) if local(e.name()).as_ref() == terminator => return Ok(out),
            // A truncated upload keeps the outlines that were complete. Losing the whole import
            // over a missing close tag would be worse than importing most of it.
            Ok(Event::Eof) => return Ok(out),
            Err(e) => return Err(OpmlError::Malformed(e.to_string())),
            _ => {}
        }
    }
}

/// Builds one outline from its attributes.
fn outline(a: &Attrs) -> Outline {
    Outline {
        text: plain(a, "text").unwrap_or_default().to_owned(),
        title: plain(a, "title").map(str::to_owned),
        xml_url: plain(a, "xmlUrl").map(str::to_owned),
        html_url: plain(a, "htmlUrl").unwrap_or_default().to_owned(),
        description: plain(a, "description").unwrap_or_default().to_owned(),
        category: plain(a, "category").map(categories).unwrap_or_default(),
        kind: kind_of_type(plain(a, "type").unwrap_or_default()),
        priority: frss(a, "priority").and_then(OpmlPriority::parse),
        ttl: frss(a, "ttl").and_then(|v| v.trim().parse().ok()),
        unicity_criteria: frss(a, "unicityCriteria").and_then(unicity_criteria),
        unicity_criteria_forced: frss(a, "unicityCriteriaForced").is_some_and(boolean),
        xpath: selector_map(a, "xPath", XPATH_KEYS),
        json_dotnotation: selector_map(a, "json", JSON_KEYS),
        xpath_to_json: frss(a, "xPathToJson").and_then(non_empty),
        path_entries: frss(a, "cssFullContent").and_then(non_empty),
        path_entries_conditions: lines(frss(a, "cssFullContentConditions")),
        // `cssFullContentFilter` is the pre-1.24 spelling of the same attribute.
        path_entries_filter: frss(a, "cssContentFilter")
            .or_else(|| frss(a, "cssFullContentFilter"))
            .and_then(non_empty),
        filters_action_read: lines(frss(a, "filtersActionRead")),
        children: Vec::new(),
    }
}

/// An unprefixed OPML attribute.
///
/// Exact match on the qualified name, so `xhtml:title` is not mistaken for `title`.
fn plain<'a>(a: &'a Attrs, key: &str) -> Option<&'a str> {
    attr(a, key)
}

/// A FreshRSS attribute, with or without its `frss:` prefix.
///
/// The prefixed spelling is what the exporter writes. The bare one is accepted as well because a
/// file that has been through a tool which drops namespace declarations still carries the values,
/// and refusing them would turn a carefully configured subscription list into a pile of
/// default-priority RSS feeds — the failure this whole module exists to prevent.
fn frss<'a>(a: &'a Attrs, key: &str) -> Option<&'a str> {
    let prefixed = format!("frss:{key}");
    attr(a, &prefixed).or_else(|| attr(a, key))
}

/// Reads one of the two selector maps, `frss:xPathItem*` or `frss:jsonItem*`.
///
/// Empty expressions are dropped: an absent selector means "this feed does not scrape that field",
/// while an empty one would mean "scrape it with no expression", which is nonsense.
fn selector_map(a: &Attrs, prefix: &str, keys: &[&str]) -> BTreeMap<String, String> {
    keys.iter()
        .filter_map(|key| {
            let name = format!("{prefix}{}", capitalize(key));
            frss(a, &name)
                .and_then(non_empty)
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

/// `item` → `Item`, `itemTitle` → `ItemTitle`.
fn capitalize(key: &str) -> String {
    match key.chars().next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(key.chars().skip(1)).collect(),
    }
}

/// Splits one of the newline-joined list attributes.
///
/// Blank lines are dropped, and that matters most for `frss:filtersActionRead`: an empty search
/// matches every entry, so one stray blank line would silently mark the whole feed read on the next
/// refresh. Values that survive are kept verbatim — a search string's own whitespace is meaningful.
fn lines(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// The comma-separated OPML `category` attribute.
fn categories(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Validates `frss:unicityCriteria` the way FreshRSS's importer does, with one deliberate difference.
///
/// The charset check is not cosmetic: the criteria becomes part of every entry's GUID, so an
/// arbitrary string from an uploaded file would flow straight into the identity scheme and change
/// which articles are considered duplicates. A value that fails is dropped, leaving the feed on the
/// default, and `id` is dropped too because that *is* the default and the exporter omits it.
///
/// The difference is that digits are allowed. PHP checks `/^[a-z:_-]{2,64}$/`, which rejects every
/// `sha1:…` criteria — and those are the only values a feed acquires on its own, when a duplicate
/// GUID degrades its policy. Porting that check literally would mean an OPML export of a degraded
/// feed re-imports with its GUID policy silently reset, which is the visible bug the check exists to
/// prevent.
fn unicity_criteria(value: &str) -> Option<String> {
    if value == "id" || !(2..=64).contains(&value.len()) {
        return None;
    }
    value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '_' | '-'))
        .then(|| value.to_owned())
}

/// `filter_var(…, FILTER_VALIDATE_BOOLEAN)`: four truthy spellings, everything else false.
fn boolean(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// Turns a present-but-empty attribute into an absent one.
///
/// The exporter drops empty values, so a file can only contain one if it was hand-edited or written
/// by another tool, and in both cases it means the same thing as no attribute at all.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::FeedKind;

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml xmlns:frss="https://freshrss.org/opml" version="2.0">
	<head>
		<title>My subscriptions</title>
		<dateCreated>Mon, 12 Oct 2026 08:30:00 +0000</dateCreated>
	</head>
	<body>
		<outline text="News">
			<outline text="Example &amp; Co" type="rss" xmlUrl="https://e.example/feed.xml" htmlUrl="https://e.example/" description="An example" frss:priority="important" frss:ttl="-3600"/>
			<outline title="Title only" type="rss" xmlUrl="https://t.example/rss"/>
		</outline>
		<outline text="Scraped" type="HTML+XPath" xmlUrl="https://s.example/page" frss:xPathItem="//article" frss:xPathItemTitle=".//h1" frss:cssFullContent="article.main" frss:cssFullContentConditions="html.example/sport" frss:filtersActionRead="intitle:ad&#10;author:bot"/>
		<outline text="Flat" type="rss" xmlUrl="https://f.example/atom.xml" category="News, Tech"/>
	</body>
</opml>"#;

    #[test]
    fn reads_the_head() {
        let doc = parse(DOC).unwrap();
        assert_eq!(doc.title, "My subscriptions");
        assert_eq!(
            doc.date_created.as_deref(),
            Some("Mon, 12 Oct 2026 08:30:00 +0000")
        );
    }

    #[test]
    fn reads_the_tree_shape() {
        let doc = parse(DOC).unwrap();
        assert_eq!(doc.outlines.len(), 3);
        assert_eq!(doc.outlines[0].name(), "News");
        assert!(!doc.outlines[0].is_feed());
        assert_eq!(doc.outlines[0].children.len(), 2);
        assert!(doc.outlines[0].children[0].is_feed());
    }

    #[test]
    fn reads_a_feed_outline() {
        let feed = &parse(DOC).unwrap().outlines[0].children[0];
        assert_eq!(feed.name(), "Example & Co", "entities are decoded");
        assert_eq!(feed.xml_url.as_deref(), Some("https://e.example/feed.xml"));
        assert_eq!(feed.html_url, "https://e.example/");
        assert_eq!(feed.description, "An example");
        assert_eq!(feed.kind, FeedKind::Rss);
        assert_eq!(feed.priority, Some(OpmlPriority::Important));
        assert_eq!(feed.ttl, Some(-3600), "a negative ttl means muted");
    }

    #[test]
    fn the_name_falls_back_to_title() {
        let feed = &parse(DOC).unwrap().outlines[0].children[1];
        assert_eq!(feed.text, "");
        assert_eq!(feed.name(), "Title only");
    }

    #[test]
    fn reads_the_xpath_map_and_the_newline_joined_lists() {
        let feed = &parse(DOC).unwrap().outlines[1];
        assert_eq!(
            feed.kind,
            FeedKind::HtmlXpath,
            "the type word is case-folded"
        );
        assert_eq!(feed.xpath["item"], "//article");
        assert_eq!(feed.xpath["itemTitle"], ".//h1");
        assert_eq!(feed.xpath.len(), 2, "absent selectors are not invented");
        assert_eq!(feed.path_entries.as_deref(), Some("article.main"));
        assert_eq!(feed.path_entries_conditions, ["html.example/sport"]);
        assert_eq!(
            feed.filters_action_read,
            ["intitle:ad", "author:bot"],
            "&#10; survives attribute normalization"
        );
    }

    #[test]
    fn reads_the_category_attribute() {
        let feed = &parse(DOC).unwrap().outlines[2];
        assert_eq!(feed.category, ["News", "Tech"]);
    }

    #[test]
    fn a_document_that_is_not_opml_is_rejected() {
        let err = parse("<rss><channel/></rss>").unwrap_err();
        assert!(matches!(err, OpmlError::NotOpml(_)), "{err}");
        assert!(err.to_string().contains("<rss>"), "{err}");
        assert!(matches!(parse("").unwrap_err(), OpmlError::NotOpml(_)));
        assert!(matches!(
            parse("not xml at all").unwrap_err(),
            OpmlError::NotOpml(_)
        ));
    }

    #[test]
    fn an_empty_opml_is_valid() {
        let doc = parse(r#"<opml version="2.0"/>"#).unwrap();
        assert!(doc.outlines.is_empty());
        assert!(doc.title.is_empty());

        let doc = parse(r#"<opml version="2.0"><head/><body/></opml>"#).unwrap();
        assert!(doc.outlines.is_empty());
    }

    #[test]
    fn a_truncated_document_keeps_the_outlines_it_got() {
        let doc = parse(r#"<opml><body><outline text="a" xmlUrl="https://a"/><outline text="b">"#)
            .unwrap();
        // The outline whose start tag was complete is kept, childless. Its own attributes were read
        // before the document ended, so dropping it would lose a feed the user can see in the file.
        assert_eq!(doc.outlines.len(), 2);
        assert_eq!(doc.outlines[0].name(), "a");
        assert_eq!(doc.outlines[1].name(), "b");
        assert!(doc.outlines[1].children.is_empty());

        // A cut *inside* a tag is a syntax error rather than a short document: there is no way to
        // know which attributes were meant to follow, so guessing would import a feed with a
        // half-configured identity.
        assert!(matches!(
            parse(r#"<opml><body><outline text="a" xmlUrl="https://a/"/><outline text="b"#)
                .unwrap_err(),
            OpmlError::Malformed(_)
        ));
    }

    #[test]
    fn a_foreign_element_inside_body_is_skipped_not_descended_into() {
        let doc = parse(
            r#"<opml><body><xhtml:div xmlns:xhtml="http://www.w3.org/1999/xhtml"><outline text="hidden" xmlUrl="https://h"/></xhtml:div><outline text="real" xmlUrl="https://r"/></body></opml>"#,
        )
        .unwrap();
        assert_eq!(doc.outlines.len(), 1);
        assert_eq!(doc.outlines[0].name(), "real");
    }

    #[test]
    fn unbalanced_close_tags_do_not_swallow_siblings() {
        // `check_end_names` is off, so the stray `</outline>` ends the first outline's children and
        // the sibling after it is still read.
        let doc = parse(
            r#"<opml><body><outline text="folder"><outline text="inner" xmlUrl="https://i"/></outline></outline><outline text="after" xmlUrl="https://a"/></body></opml>"#,
        )
        .unwrap();
        assert_eq!(doc.outlines.len(), 2);
        assert_eq!(doc.outlines[0].children.len(), 1);
        assert_eq!(doc.outlines[1].name(), "after");
    }

    #[test]
    fn pathological_nesting_is_refused() {
        let depth = MAX_DEPTH + 2;
        let doc = format!(
            "<opml><body>{}{}</body></opml>",
            r#"<outline text="x">"#.repeat(depth),
            "</outline>".repeat(depth)
        );
        assert!(matches!(parse(&doc).unwrap_err(), OpmlError::TooDeep));

        let ok = MAX_DEPTH - 1;
        let doc = format!(
            "<opml><body>{}{}</body></opml>",
            r#"<outline text="x">"#.repeat(ok),
            "</outline>".repeat(ok)
        );
        let parsed = parse(&doc).unwrap();
        let mut depth = 0;
        let mut cursor = &parsed.outlines[..];
        while !cursor.is_empty() {
            depth += 1;
            cursor = &cursor[0].children;
        }
        assert_eq!(depth, ok);
    }

    #[test]
    fn attributes_without_the_namespace_prefix_are_still_read() {
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/" priority="hidden" ttl="60" cssFullContent="article"/>"#,
        )
        .unwrap();
        let feed = &doc.outlines[0];
        assert_eq!(feed.priority, Some(OpmlPriority::Hidden));
        assert_eq!(feed.ttl, Some(60));
        assert_eq!(feed.path_entries.as_deref(), Some("article"));
    }

    #[test]
    fn a_foreign_namespace_cannot_impersonate_the_freshrss_one() {
        let doc = parse(
            r#"<opml><body><outline xmlns:o="urn:other" text="a" xmlUrl="https://a/" o:priority="hidden"/>"#,
        )
        .unwrap();
        assert_eq!(doc.outlines[0].priority, None);
    }

    #[test]
    fn type_words_map_to_kinds() {
        for (word, kind) in [
            ("rss", FeedKind::Rss),
            ("atom", FeedKind::Rss),
            ("", FeedKind::Rss),
            ("link", FeedKind::Rss),
            ("HTML+XPath", FeedKind::HtmlXpath),
            ("html+xpath", FeedKind::HtmlXpath),
            ("XML+XPath", FeedKind::XmlXpath),
            ("JSON+DotNotation", FeedKind::JsonDotNotation),
            ("JSON+DotPath", FeedKind::JsonDotNotation),
            ("JSONFeed", FeedKind::JsonFeed),
            (
                "HTML+XPath+JSON+DotNotation",
                FeedKind::HtmlXpathJsonDotNotation,
            ),
        ] {
            let doc = parse(&format!(
                r#"<opml><body><outline text="a" xmlUrl="https://a/" type="{word}"/></body></opml>"#
            ))
            .unwrap();
            assert_eq!(doc.outlines[0].kind, kind, "type={word:?}");
        }
    }

    #[test]
    fn an_unreadable_ttl_is_ignored_rather_than_guessed() {
        for (raw, expected) in [
            ("3600", Some(3600)),
            ("-3600", Some(-3600)),
            ("0", Some(0)),
            ("  90 ", Some(90)),
            ("weekly", None),
            ("", None),
        ] {
            let doc = parse(&format!(
                r#"<opml><body><outline text="a" xmlUrl="https://a/" frss:ttl="{raw}"/></body></opml>"#
            ))
            .unwrap();
            assert_eq!(doc.outlines[0].ttl, expected, "frss:ttl={raw:?}");
        }
    }

    #[test]
    fn unicity_criteria_is_validated() {
        for (raw, expected) in [
            ("link", Some("link")),
            (
                "sha1:link_published_title",
                Some("sha1:link_published_title"),
            ),
            ("id", None),
            ("", None),
            ("x", None),
            ("UPPER", None),
            ("with space", None),
            ("drop;table", None),
            (&"a".repeat(65), None),
            (&"a".repeat(64), Some(&"a".repeat(64))),
        ] {
            let doc = parse(&format!(
                r#"<opml><body><outline text="a" xmlUrl="https://a/" frss:unicityCriteria="{raw}"/></body></opml>"#
            ))
            .unwrap();
            assert_eq!(
                doc.outlines[0].unicity_criteria.as_deref(),
                expected,
                "frss:unicityCriteria={raw:?}"
            );
        }
    }

    #[test]
    fn the_forced_flag_uses_php_boolean_words() {
        for (raw, expected) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("on", true),
            ("yes", true),
            ("false", false),
            ("0", false),
            ("", false),
            ("maybe", false),
        ] {
            let doc = parse(&format!(
                r#"<opml><body><outline text="a" xmlUrl="https://a/" frss:unicityCriteriaForced="{raw}"/></body></opml>"#
            ))
            .unwrap();
            assert_eq!(doc.outlines[0].unicity_criteria_forced, expected, "{raw:?}");
        }
    }

    #[test]
    fn blank_lines_are_dropped_from_the_joined_lists() {
        let doc = parse(
            "<opml><body><outline text=\"a\" xmlUrl=\"https://a/\" frss:filtersActionRead=\"intitle:x&#10;&#10;   &#10;author:y&#10;\"/></body></opml>",
        )
        .unwrap();
        // An empty search matches every entry, so a blank line here would mark the whole feed read.
        assert_eq!(
            doc.outlines[0].filters_action_read,
            ["intitle:x", "author:y"]
        );
    }

    #[test]
    fn the_legacy_filter_attribute_name_is_accepted() {
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/" frss:cssFullContentFilter=".share"/></body></opml>"#,
        )
        .unwrap();
        assert_eq!(
            doc.outlines[0].path_entries_filter.as_deref(),
            Some(".share")
        );
    }

    #[test]
    fn the_current_filter_attribute_name_wins_over_the_legacy_one() {
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/" frss:cssContentFilter=".new" frss:cssFullContentFilter=".old"/></body></opml>"#,
        )
        .unwrap();
        assert_eq!(doc.outlines[0].path_entries_filter.as_deref(), Some(".new"));
    }

    #[test]
    fn the_json_dot_notation_map_uses_the_same_keys() {
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/" type="JSON+DotNotation" frss:jsonItem="items" frss:jsonItemUid="id" frss:jsonItemTimeFormat="Y-m-d"/></body></opml>"#,
        )
        .unwrap();
        let feed = &doc.outlines[0];
        assert_eq!(feed.kind, FeedKind::JsonDotNotation);
        assert_eq!(feed.json_dotnotation["item"], "items");
        assert_eq!(feed.json_dotnotation["itemUid"], "id");
        assert_eq!(feed.json_dotnotation["itemTimeFormat"], "Y-m-d");
        assert!(feed.xpath.is_empty());
    }

    #[test]
    fn an_empty_selector_is_the_same_as_an_absent_one() {
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/" type="HTML+XPath" frss:xPathItem="" frss:cssFullContent="" frss:xPathToJson=""/></body></opml>"#,
        )
        .unwrap();
        let feed = &doc.outlines[0];
        assert!(feed.xpath.is_empty());
        assert_eq!(feed.path_entries, None);
        assert_eq!(feed.xpath_to_json, None);
    }

    #[test]
    fn an_outline_can_be_both_a_feed_and_a_folder_and_the_model_keeps_both() {
        // A broken exporter can produce this. The model records what is there; which of the two the
        // importer honours is the importer's decision, not the reader's.
        let doc = parse(
            r#"<opml><body><outline text="a" xmlUrl="https://a/"><outline text="b" xmlUrl="https://b/"/></outline></body></opml>"#,
        )
        .unwrap();
        assert!(doc.outlines[0].is_feed());
        assert_eq!(doc.outlines[0].children.len(), 1);
    }
}
