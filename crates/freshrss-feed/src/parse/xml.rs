//! Shared plumbing for the XML readers.
//!
//! RSS, Atom and RDF differ in vocabulary but not in shape, so the event-loop mechanics live here
//! and each parser only says which element it is looking at.

use std::borrow::Cow;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::{Reader, XmlVersion};

/// Builds a reader with the leniency real feeds need.
///
/// `check_end_names` is off because mismatched close tags are common in hand-generated RSS and
/// rejecting the whole document over one loses a subscription the user can see is otherwise fine.
/// Text is *not* trimmed here: `<title> </title>` and `<title></title>` must stay distinguishable,
/// and collapsing whitespace is the caller's decision via [`crate::text::normalize_space`].
pub fn reader(text: &str) -> Reader<&[u8]> {
    let mut r = Reader::from_str(text);
    r.config_mut().check_end_names = false;
    r.config_mut().trim_text(false);
    r
}

/// The element name without its namespace prefix.
///
/// Feed namespaces are declared inconsistently — the same `content:encoded` element appears under
/// whatever prefix the generator felt like — so matching on the local name is what makes the
/// parsers work across generators.
pub fn local(name: QName<'_>) -> Cow<'_, str> {
    split(name.into_inner()).1.into()
}

/// Splits a qualified name into its prefix and local part.
///
/// The prefix matters in exactly one place: `media:title` and `media:description` would otherwise be
/// indistinguishable from an item's own `<title>` and `<description>`.
pub fn split(full: &str) -> (Option<&str>, &str) {
    match full.split_once(':') {
        Some((prefix, rest)) if !rest.is_empty() => (Some(prefix), rest),
        _ => (None, full),
    }
}

/// An element's attributes, keyed by local name.
pub type Attrs = Vec<(String, String)>;

/// Reads an element's attributes, normalizing their values per the XML spec.
///
/// A value we cannot normalize is kept raw rather than dropping the attribute or failing the
/// document: a feed with one odd `media:` attribute still has perfectly good articles in it.
pub fn attrs(e: &BytesStart<'_>, _r: &Reader<&[u8]>) -> Attrs {
    let mut out = Vec::new();
    for attr in e.attributes().flatten() {
        let key = local(attr.key).into_owned();
        let value = attr
            .normalized_value(XmlVersion::default())
            .map(Cow::into_owned)
            .unwrap_or_else(|_| attr.value.into_owned());
        out.push((key, value));
    }
    out
}

/// Finds an attribute by local name.
pub fn attr<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Reads an element's content as text, consuming through its end tag.
///
/// `read_text` returns the *raw* inner span — markup, `CDATA` wrappers and all — which is what lets
/// all three of these produce `<p>Hi</p>`:
///
/// ```text
/// <description>&lt;p&gt;Hi&lt;/p&gt;</description>
/// <description><![CDATA[<p>Hi</p>]]></description>
/// <description><p>Hi</p></description>       ← not well-formed XML, but ubiquitous
/// ```
///
/// The third form is why this is used instead of collecting `Event::Text`: a strict text-only read
/// would descend into `<p>`, return `Hi`, and lose the markup.
///
/// Entity resolution is then [`xml_unescape`], which handles the five XML entities and numeric
/// references but leaves HTML-only names for [`html_only_entity_decode`](crate::html_only_entity_decode).
pub fn text(r: &mut Reader<&[u8]>, name: QName<'_>) -> String {
    match r.read_text(name) {
        Ok(bytes) => decode_content(&bytes),
        Err(_) => String::new(),
    }
}

/// Unwraps `CDATA` sections and resolves XML entities in everything outside them.
///
/// `CDATA` content is literal by definition, so running it through the unescaper would corrupt a
/// body that legitimately contains `&amp;`. quick-xml's own `unescape` cannot do this — it errors on
/// the bare `&` inside `CDATA` — which is why the split happens here.
fn decode_content(raw: &str) -> String {
    if !raw.contains(CDATA_START) {
        return crate::entities::xml_unescape(raw).into_owned();
    }

    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find(CDATA_START) {
        let body = start + CDATA_START.len();
        out.push_str(&crate::entities::xml_unescape(&rest[..start]));
        match rest[body..].find(CDATA_END) {
            // An unterminated section runs to the end of the document, as in HTML.
            None => {
                out.push_str(&rest[body..]);
                return out;
            }
            Some(len) => {
                out.push_str(&rest[body..body + len]);
                rest = &rest[body + len + CDATA_END.len()..];
            }
        }
    }
    out.push_str(&crate::entities::xml_unescape(rest));
    out
}

const CDATA_START: &str = "<![CDATA[";
const CDATA_END: &str = "]]>";

/// Skips a subtree, used for elements this parser does not model.
pub fn skip(r: &mut Reader<&[u8]>, name: QName<'_>) {
    let _ = r.read_to_end(name);
}

/// Runs an event loop over the children of the element just opened, stopping at its end tag.
///
/// `on_start` sees every `Start` and `Empty` child, with the **full** element name (prefix intact,
/// see [`split`]) and `is_empty` telling the two apart — the distinction matters because [`text`]
/// must not be called on an `Empty` element, since there is no end tag to consume and the reader
/// would run to EOF.
///
/// `until` may be given with or without its prefix; the comparison is always local-name to
/// local-name. That is not a convenience: a caller holding the full name `"media:group"` and
/// comparing it against the end tag's local name `"group"` would never match, and the walk would run
/// to EOF — silently swallowing every item after the first `media:group`.
pub fn walk(
    r: &mut Reader<&[u8]>,
    until: &str,
    mut on_start: impl FnMut(&str, &BytesStart<'_>, bool, &mut Reader<&[u8]>),
) {
    let until_local = split(until).1;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name().into_inner().to_owned();
                if split(&name).1 == until_local {
                    return;
                }
                on_start(&name, &e, false, r);
            }
            Ok(Event::Empty(e)) => {
                let name = e.name().into_inner().to_owned();
                on_start(&name, &e, true, r);
            }
            Ok(Event::End(e)) => {
                if local(e.name()) == until_local {
                    return;
                }
            }
            Ok(Event::Eof) | Err(_) => return,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_start(doc: &str) -> (String, Attrs) {
        let mut r = reader(doc);
        loop {
            match r.read_event().unwrap() {
                Event::Start(e) | Event::Empty(e) => {
                    return (local(e.name()).into_owned(), attrs(&e, &r));
                }
                Event::Eof => return (String::new(), Vec::new()),
                _ => {}
            }
        }
    }

    #[test]
    fn local_drops_the_namespace_prefix() {
        assert_eq!(local(QName("content:encoded")), "encoded");
        assert_eq!(local(QName("title")), "title");
        assert_eq!(local(QName("xml:base")), "base");
        assert_eq!(local(QName(":weird")), "weird");
        assert_eq!(local(QName("trailing:")), "trailing:");
    }

    #[test]
    fn split_returns_the_prefix_so_media_title_is_distinguishable() {
        assert_eq!(split("media:title"), (Some("media"), "title"));
        assert_eq!(split("title"), (None, "title"));
        // XML names allow at most one colon, so an invalid second one stays in the local part.
        assert_eq!(split("a:b:c"), (Some("a"), "b:c"));
    }

    #[test]
    fn attrs_decode_entities_in_values() {
        let (_, a) = first_start(r#"<enclosure url="https://x/a?b=1&amp;c=2" type="audio/mpeg"/>"#);
        assert_eq!(attr(&a, "url"), Some("https://x/a?b=1&c=2"));
        assert_eq!(attr(&a, "type"), Some("audio/mpeg"));
        assert_eq!(attr(&a, "missing"), None);
    }

    #[test]
    fn attrs_are_keyed_by_local_name() {
        let (_, a) = first_start(r#"<x frss:kind="10" plain="1"/>"#);
        assert_eq!(attr(&a, "kind"), Some("10"));
        assert_eq!(attr(&a, "plain"), Some("1"));
    }

    #[test]
    fn text_unescapes_entities() {
        let mut r = reader("<t>&lt;p&gt;Hi &amp;amp; bye&lt;/p&gt;</t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "<p>Hi &amp; bye</p>");
    }

    #[test]
    fn text_keeps_cdata_content_literal() {
        let mut r = reader("<t><![CDATA[<p>Hi &amp; bye</p>]]></t>");
        let _ = r.read_event().unwrap();
        // Inside CDATA nothing is a reference, so `&amp;` must survive as four characters.
        assert_eq!(text(&mut r, QName("t")), "<p>Hi &amp; bye</p>");
    }

    #[test]
    fn text_handles_mixed_cdata_and_escaped_text() {
        let mut r = reader("<t>before <![CDATA[a < b]]> after &amp; end</t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "before a < b after & end");
    }

    #[test]
    fn an_unterminated_cdata_runs_to_the_end() {
        // Exercised through `decode_content` directly: an element containing an unterminated CDATA
        // has no end tag for `read_text` to stop at, so the reader never returns a span at all.
        assert_eq!(decode_content("a<![CDATA[never closed"), "anever closed");
        assert_eq!(decode_content("<![CDATA[<p>&amp;</p>"), "<p>&amp;</p>");
    }

    #[test]
    fn text_leaves_html_only_entities_for_the_entity_pass() {
        let mut r = reader("<t>Caf&eacute; &amp; bar</t>");
        let _ = r.read_event().unwrap();
        let raw = text(&mut r, QName("t"));
        assert_eq!(raw, "Caf&eacute; & bar");
        assert_eq!(crate::html_only_entity_decode(&raw), "Café & bar");
    }

    #[test]
    fn text_unwraps_cdata() {
        let mut r = reader("<t><![CDATA[<p>Hi</p>]]></t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "<p>Hi</p>");
    }

    #[test]
    fn text_keeps_inner_markup_that_was_not_escaped() {
        let mut r = reader("<t><p>Hi <b>there</b></p></t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "<p>Hi <b>there</b></p>");
    }

    #[test]
    fn text_of_a_nested_same_named_element_is_balanced() {
        let mut r = reader("<t>outer <t>inner</t> tail</t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "outer <t>inner</t> tail");
    }

    #[test]
    fn text_is_not_trimmed() {
        let mut r = reader("<t>  spaced  </t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "  spaced  ");
    }

    #[test]
    fn text_of_an_empty_element_is_empty() {
        // The caller must not call `text` on an `Empty` event; `walk` reports which is which.
        let mut r = reader("<t>");
        let _ = r.read_event().unwrap();
        assert_eq!(text(&mut r, QName("t")), "");
    }

    #[test]
    fn walk_visits_children_and_stops_at_the_end_tag() {
        let mut r = reader("<channel><title>T</title><item/><link>L</link></channel>");
        let _ = r.read_event().unwrap(); // <channel>
        let mut seen = Vec::new();
        walk(&mut r, "channel", |name, _, is_empty, _| {
            seen.push((name.to_owned(), is_empty));
        });
        assert_eq!(
            seen,
            vec![
                ("title".to_owned(), false),
                ("item".to_owned(), true),
                ("link".to_owned(), false)
            ]
        );
        assert!(matches!(r.read_event(), Ok(Event::Eof)));
    }

    #[test]
    fn walk_keeps_the_prefix_so_media_title_is_distinguishable() {
        let mut r = reader("<item><title>Real</title><media:title>Media</media:title></item>");
        let _ = r.read_event().unwrap();
        let mut seen = Vec::new();
        walk(&mut r, "item", |name, _, _, _| seen.push(name.to_owned()));
        assert_eq!(seen, vec!["title", "media:title"]);
        assert_eq!(split(&seen[1]), (Some("media"), "title"));
    }

    #[test]
    fn walk_matches_a_prefixed_terminator_by_local_name() {
        // Regression: a caller holding the full name "media:group" compared against the end tag's
        // local name "group" would never match, and the walk would run to EOF — silently swallowing
        // every sibling after it.
        let mut r = reader("<media:group><a/><b/></media:group><after/>");
        let _ = r.read_event().unwrap();
        let mut seen = Vec::new();
        walk(&mut r, "media:group", |name, _, _, _| {
            seen.push(name.to_owned())
        });
        assert_eq!(seen, vec!["a", "b"]);
        // The reader must be positioned after the group, not at EOF.
        let mut after = Vec::new();
        walk(&mut r, "never", |name, _, _, _| after.push(name.to_owned()));
        assert_eq!(after, vec!["after"]);
    }

    #[test]
    fn walk_accepts_either_spelling_of_the_terminator() {
        for until in ["group", "media:group"] {
            let mut r = reader("<media:group><a/></media:group><sibling/>");
            let _ = r.read_event().unwrap();
            let mut seen = Vec::new();
            walk(&mut r, until, |name, _, _, _| seen.push(name.to_owned()));
            assert_eq!(seen, vec!["a"], "terminator {until:?}");
        }
    }

    #[test]
    fn walk_stops_at_eof_when_the_end_tag_is_missing() {
        let mut r = reader("<channel><title>T</title>");
        let _ = r.read_event().unwrap();
        let mut count = 0;
        walk(&mut r, "channel", |_, _, _, _| count += 1);
        assert_eq!(count, 1, "terminated by EOF rather than looping forever");
    }

    #[test]
    fn a_stray_end_tag_does_not_abort_the_walk() {
        // `check_end_names` is off, so an `</c>` that closes nothing is ignored rather than fatal
        // and the sibling after it is still visited.
        let mut r = reader("<a><b>x</b></c><d>y</d></a>");
        let _ = r.read_event().unwrap();
        let mut seen = Vec::new();
        walk(&mut r, "a", |name, _, _, r| {
            seen.push((name.to_owned(), text(r, QName(name))));
        });
        assert_eq!(
            seen,
            vec![("b".into(), "x".into()), ("d".into(), "y".into())]
        );
    }
}
