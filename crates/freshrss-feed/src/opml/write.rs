//! Writing an [`Opml`] document.
//!
//! The output is byte-for-byte the shape PHP FreshRSS produces: the same element order, the same
//! attribute order, tab indentation, and the same set of omissions. That matters in both directions —
//! a file exported here has to import cleanly into a PHP FreshRSS, and one exported there has to
//! import cleanly here, and the cheapest way to keep the two compatible is for neither to invent
//! anything the other does not already write.

use std::collections::BTreeMap;

use super::model::{type_of_kind, Opml, Outline, XPATH_KEYS};
use crate::kind::FeedKind;

/// The namespace every `frss:*` attribute lives in.
const FRSS_NAMESPACE: &str = "https://freshrss.org/opml";

/// Serializes a document.
pub fn write(doc: &Opml) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<opml xmlns:frss=\"");
    out.push_str(FRSS_NAMESPACE);
    out.push_str("\" version=\"2.0\">\n");

    out.push_str("\t<head>\n");
    element(&mut out, 2, "title", &doc.title);
    if let Some(date) = &doc.date_created {
        element(&mut out, 2, "dateCreated", date);
    }
    out.push_str("\t</head>\n");

    out.push_str("\t<body>\n");
    for outline in &doc.outlines {
        write_outline(&mut out, 2, outline);
    }
    out.push_str("\t</body>\n");

    out.push_str("</opml>\n");
    out
}

/// Writes one `<head>` child.
fn element(out: &mut String, depth: usize, name: &str, value: &str) {
    out.push_str(&indent(depth));
    out.push('<');
    out.push_str(name);
    out.push('>');
    out.push_str(&escape_text(value));
    out.push_str("</");
    out.push_str(name);
    out.push_str(">\n");
}

fn write_outline(out: &mut String, depth: usize, o: &Outline) {
    let attrs = attributes(o);
    let pad = indent(depth);
    out.push_str(&pad);
    out.push_str("<outline");
    for (name, value) in &attrs {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        out.push_str(&escape_attr(value));
        out.push('"');
    }
    if o.children.is_empty() {
        out.push_str("/>\n");
        return;
    }
    out.push_str(">\n");
    for child in &o.children {
        write_outline(out, depth + 1, child);
    }
    out.push_str(&pad);
    out.push_str("</outline>\n");
}

/// The attributes of one outline, in the order the PHP exporter emits them.
///
/// Every rule here is an omission rule copied from that exporter, which drops null and empty values
/// after building the array. Omitting rather than writing an empty attribute is what keeps the file
/// small enough to read by hand, and what makes "not configured" indistinguishable from "configured
/// as the default" — which is correct, because the importer treats them the same way.
fn attributes(o: &Outline) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();

    // `name()` rather than `text`: the display name may have arrived in `title`, and PHP's importer
    // only falls back to `title` when `text` is *absent*, so writing `text="" title="X"` would import
    // there as a feed with no name.
    out.push(("text".to_owned(), o.name().to_owned()));

    if let Some(url) = &o.xml_url {
        push(&mut out, "type", type_of_kind(o.kind));
        push(&mut out, "xmlUrl", url);
    }
    push(&mut out, "htmlUrl", &o.html_url);
    push(&mut out, "description", &o.description);

    if let Some(priority) = o.priority {
        push(&mut out, "frss:priority", priority.as_str());
    }
    match &o.unicity_criteria {
        Some(criteria) if criteria != "id" => push(&mut out, "frss:unicityCriteria", criteria),
        _ => {}
    }
    if o.unicity_criteria_forced {
        push(&mut out, "frss:unicityCriteriaForced", "true");
    }
    // Signed, and 0 is the reader's own default, so it is omitted exactly as `TTL_DEFAULT` is.
    match o.ttl {
        Some(ttl) if ttl != 0 => push(&mut out, "frss:ttl", &ttl.to_string()),
        _ => {}
    }

    // Each map is written only for the kinds that read it, matching the export switch. Writing a
    // stale map for a feed whose kind has since changed would resurrect the old scraper settings.
    match o.kind {
        FeedKind::HtmlXpath | FeedKind::XmlXpath => selectors(&mut out, "xPath", &o.xpath),
        FeedKind::JsonDotNotation | FeedKind::HtmlXpathJsonDotNotation => {
            selectors(&mut out, "json", &o.json_dotnotation);
            if o.kind == FeedKind::HtmlXpathJsonDotNotation {
                if let Some(xpath) = &o.xpath_to_json {
                    push(&mut out, "frss:xPathToJson", xpath);
                }
            }
        }
        _ => {}
    }

    if !o.filters_action_read.is_empty() {
        push(
            &mut out,
            "frss:filtersActionRead",
            &o.filters_action_read.join("\n"),
        );
    }
    if let Some(path_entries) = &o.path_entries {
        push(&mut out, "frss:cssFullContent", path_entries);
    }
    if !o.path_entries_conditions.is_empty() {
        push(
            &mut out,
            "frss:cssFullContentConditions",
            &o.path_entries_conditions.join("\n"),
        );
    }
    if let Some(filter) = &o.path_entries_filter {
        push(&mut out, "frss:cssContentFilter", filter);
    }

    out
}

/// Appends an attribute unless its value is empty, which is the exporter's `array_filter`.
fn push(out: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !value.is_empty() {
        out.push((name.to_owned(), value.to_owned()));
    }
}

/// Writes one selector map in the canonical key order.
fn selectors(out: &mut Vec<(String, String)>, prefix: &str, map: &BTreeMap<String, String>) {
    for key in XPATH_KEYS {
        match map.get(*key) {
            Some(value) if !value.is_empty() => {
                out.push((format!("frss:{prefix}{}", capitalize(key)), value.clone()));
            }
            _ => {}
        }
    }
}

/// `item` → `Item`, `itemTitle` → `ItemTitle`.
fn capitalize(key: &str) -> String {
    match key.chars().next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(key.chars().skip(1)).collect(),
    }
}

fn indent(depth: usize) -> String {
    "\t".repeat(depth)
}

/// Escapes element content.
fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes an attribute value.
///
/// Whitespace becomes character references, and that is load-bearing rather than cosmetic: XML
/// attribute-value normalization turns a literal newline, tab or carriage return into a space when
/// the document is read back, so the newline-joined lists — `frss:filtersActionRead` above all —
/// would come back as one long space-separated line and stop being a list of searches.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opml::OpmlPriority;

    fn doc(outlines: Vec<Outline>) -> Opml {
        Opml {
            title: "Subscriptions".to_owned(),
            date_created: Some("Mon, 12 Oct 2026 08:30:00 +0000".to_owned()),
            outlines,
        }
    }

    #[test]
    fn the_document_shape_matches_the_php_exporter() {
        let out = write(&doc(vec![Outline::folder(
            "News",
            vec![Outline::feed("Example", "https://e.example/feed.xml")],
        )]));
        assert_eq!(
            out,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <opml xmlns:frss=\"https://freshrss.org/opml\" version=\"2.0\">\n\
             \t<head>\n\
             \t\t<title>Subscriptions</title>\n\
             \t\t<dateCreated>Mon, 12 Oct 2026 08:30:00 +0000</dateCreated>\n\
             \t</head>\n\
             \t<body>\n\
             \t\t<outline text=\"News\">\n\
             \t\t\t<outline text=\"Example\" type=\"rss\" xmlUrl=\"https://e.example/feed.xml\"/>\n\
             \t\t</outline>\n\
             \t</body>\n\
             </opml>\n"
        );
    }

    #[test]
    fn an_empty_document_still_has_a_head_and_body() {
        let out = write(&Opml::default());
        // The head title element is always written, empty or not, because that is what the PHP
        // exporter emits and an OPML reader is entitled to find it.
        assert!(
            out.contains("\t<head>\n\t\t<title></title>\n\t</head>\n"),
            "{out}"
        );
        assert!(out.contains("\t<body>\n\t</body>\n"), "{out}");
        assert!(!out.contains("dateCreated"), "{out}");
    }

    #[test]
    fn empty_values_are_omitted_not_written_blank() {
        let out = write(&doc(vec![Outline::feed("Example", "https://e.example/")]));
        assert!(!out.contains("htmlUrl"), "{out}");
        assert!(!out.contains("description"), "{out}");
        assert!(!out.contains("frss:"), "{out}");
    }

    #[test]
    fn a_folder_writes_only_its_name() {
        let out = write(&doc(vec![Outline::folder("News", Vec::new())]));
        assert!(out.contains("<outline text=\"News\"/>"), "{out}");
        assert!(!out.contains("type="), "{out}");
    }

    #[test]
    fn the_name_falls_back_to_title_on_export_too() {
        let mut o = Outline::feed("", "https://e.example/");
        o.title = Some("From title".into());
        // PHP's importer only falls back when `text` is absent, so the name has to be written there.
        assert!(write(&doc(vec![o])).contains(r#"text="From title""#));
    }

    #[test]
    fn values_are_escaped() {
        let written = write(&Opml {
            title: "Café <x> & more".to_owned(),
            outlines: vec![Outline::feed(
                "A & B <tag> \"q\"",
                "https://e.example/?a=1&b=2",
            )],
            ..Opml::default()
        });
        assert!(
            written.contains("<title>Café &lt;x&gt; &amp; more</title>"),
            "{written}"
        );
        assert!(
            written.contains(r#"text="A &amp; B &lt;tag&gt; &quot;q&quot;""#),
            "{written}"
        );
        assert!(
            written.contains(r#"xmlUrl="https://e.example/?a=1&amp;b=2""#),
            "{written}"
        );
    }

    #[test]
    fn a_description_is_written_as_an_attribute() {
        let mut o = Outline::feed("a", "https://e.example/");
        o.description = "x < y & z".into();
        let out = write(&doc(vec![o]));
        assert!(out.contains(r#"description="x &lt; y &amp; z""#), "{out}");
        assert!(!out.contains("<description>"), "{out}");
    }

    #[test]
    fn newlines_in_attributes_become_character_references() {
        let mut o = Outline::feed("a", "https://e.example/");
        o.filters_action_read = vec!["intitle:ad".into(), "author:bot".into()];
        let out = write(&doc(vec![o]));
        assert!(
            out.contains("frss:filtersActionRead=\"intitle:ad&#10;author:bot\""),
            "{out}"
        );
        assert!(
            !out.contains("frss:filtersActionRead=\"intitle:ad\n"),
            "a literal newline would be normalized to a space on re-read: {out}"
        );
    }

    #[test]
    fn the_priority_default_is_omitted() {
        let mut o = Outline::feed("a", "https://e.example/");
        assert!(!write(&doc(vec![o.clone()])).contains("frss:priority"));

        o.priority = Some(OpmlPriority::Hidden);
        assert!(write(&doc(vec![o.clone()])).contains(r#"frss:priority="hidden""#));

        // Spelled out rather than defaulted, so it is written and reads back the same.
        o.priority = Some(OpmlPriority::Main);
        assert!(write(&doc(vec![o])).contains(r#"frss:priority="main""#));
    }

    #[test]
    fn the_ttl_default_is_omitted_and_a_muted_feed_is_not() {
        let mut o = Outline::feed("a", "https://e.example/");
        o.ttl = Some(0);
        assert!(!write(&doc(vec![o.clone()])).contains("frss:ttl"));
        o.ttl = Some(-3600);
        assert!(write(&doc(vec![o.clone()])).contains(r#"frss:ttl="-3600""#));
        o.ttl = Some(900);
        assert!(write(&doc(vec![o])).contains(r#"frss:ttl="900""#));
    }

    #[test]
    fn the_id_unicity_criteria_is_the_one_that_is_omitted() {
        let mut o = Outline::feed("a", "https://e.example/");
        o.unicity_criteria = Some("id".into());
        assert!(!write(&doc(vec![o.clone()])).contains("unicityCriteria"));
        o.unicity_criteria = Some("link".into());
        o.unicity_criteria_forced = true;
        let out = write(&doc(vec![o]));
        assert!(out.contains(r#"frss:unicityCriteria="link""#), "{out}");
        assert!(
            out.contains(r#"frss:unicityCriteriaForced="true""#),
            "{out}"
        );
    }

    #[test]
    fn selector_maps_are_written_in_key_order_for_the_kinds_that_use_them() {
        let mut o = Outline::feed("a", "https://e.example/page");
        o.kind = FeedKind::HtmlXpath;
        o.xpath = BTreeMap::from([
            ("itemTitle".to_owned(), ".//h1".to_owned()),
            ("item".to_owned(), "//article".to_owned()),
            ("itemUid".to_owned(), String::new()),
        ]);
        let out = write(&doc(vec![o]));
        let item = out.find("frss:xPathItem=").expect("item written");
        let title = out.find("frss:xPathItemTitle=").expect("title written");
        assert!(item < title, "canonical order, not map order: {out}");
        assert!(
            !out.contains("frss:xPathItemUid"),
            "empty selector omitted: {out}"
        );

        // The same map on an RSS feed is not written: the kind no longer reads it.
        let mut stale = Outline::feed("a", "https://e.example/");
        stale.xpath = BTreeMap::from([("item".to_owned(), "//article".to_owned())]);
        let stale_out = write(&doc(vec![stale]));
        assert!(!stale_out.contains("frss:xPath"), "{stale_out}");
    }

    #[test]
    fn the_json_map_and_xpathtojson_are_written_for_kind_35() {
        let mut o = Outline::feed("a", "https://e.example/page");
        o.kind = FeedKind::HtmlXpathJsonDotNotation;
        o.json_dotnotation = BTreeMap::from([("item".to_owned(), "items".to_owned())]);
        o.xpath_to_json = Some("//script[@type='application/json']".to_owned());
        let out = write(&doc(vec![o]));
        assert!(
            out.contains(r#"type="HTML+XPath+JSON+DotNotation""#),
            "{out}"
        );
        assert!(out.contains(r#"frss:jsonItem="items""#), "{out}");
        assert!(out.contains("frss:xPathToJson="), "{out}");
        assert!(!out.contains("frss:xPathItem"), "{out}");
    }

    #[test]
    fn the_full_text_selectors_are_written_when_configured() {
        let mut o = Outline::feed("a", "https://e.example/");
        o.path_entries = Some("article.main".into());
        o.path_entries_conditions =
            vec!["html.example/sport".into(), "html.example/culture".into()];
        o.path_entries_filter = Some(".share".into());
        let out = write(&doc(vec![o]));
        assert!(
            out.contains(r#"frss:cssFullContent="article.main""#),
            "{out}"
        );
        assert!(
            out.contains(
                "frss:cssFullContentConditions=\"html.example/sport&#10;html.example/culture\""
            ),
            "{out}"
        );
        assert!(out.contains(r#"frss:cssContentFilter=".share""#), "{out}");
    }

    #[test]
    fn every_kind_has_a_type_word() {
        for kind in FeedKind::ALL {
            let mut o = Outline::feed("a", "https://e.example/");
            o.kind = kind;
            assert!(write(&doc(vec![o])).contains("type=\""), "{kind:?}");
        }
    }
}
