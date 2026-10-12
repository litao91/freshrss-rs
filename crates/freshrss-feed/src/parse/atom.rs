//! Atom 1.0 (RFC 4287).
//!
//! Atom differs from RSS in the ways that most often trip up a port: `<link>` is an attribute-carrying
//! element rather than text, `<id>` is mandatory, `<content>` carries a `type` that decides whether
//! its body is markup or plain text, and there are two dates (`published` and `updated`) with distinct
//! meanings.

use quick_xml::name::QName;
use quick_xml::Reader;

use super::common::{
    enclosure_from_attrs, is_remote_uri, split_tags, thumbnail_from_attrs, unique_in_place,
};
use super::xml::{attr, attrs, reader, split, text, walk};
use super::{date, FeedError};
use crate::raw::{FeedFormat, RawFeed, RawItem};

/// Parses an Atom 1.0 document.
///
/// # Errors
///
/// [`FeedError::Parse`] when there is no `<feed>` root. A feed with no `<entry>` elements is not an
/// error.
pub fn parse(document: &str) -> Result<RawFeed, FeedError> {
    let mut r = reader(document);
    let mut feed = RawFeed {
        format: FeedFormat::Atom,
        ..RawFeed::default()
    };

    let Some((feed_name, root_attrs)) = find_element(&mut r, "feed") else {
        return Err(FeedError::Parse(
            "Atom document has no <feed> root element".to_owned(),
        ));
    };
    // `xml:base` on the root is the resolution base for every relative href in the document.
    if let Some(base) = attr(&root_attrs, "base").filter(|b| !b.trim().is_empty()) {
        feed.base_url = Some(base.trim().to_owned());
    }

    walk(&mut r, &feed_name, |name, e, is_empty, r| {
        let (_, tag) = split(name);
        let a = attrs(e, r);
        match tag {
            "title" if feed.title.is_none() => feed.title = leaf(r, name, is_empty),
            "subtitle" | "tagline" if feed.description.is_none() => {
                feed.description = content_body(r, name, is_empty, &a)
            }
            "link" => apply_feed_link(&mut feed, &a),
            "icon" | "logo" if feed.icon.is_none() => {
                feed.icon = leaf(r, name, is_empty).filter(|u| is_remote_uri(u));
            }
            "id" | "updated" | "author" | "contributor" | "rights" | "generator" | "category"
                if !is_empty =>
            {
                // Feed-level elements nothing downstream reads. `<author>` in particular is not
                // inherited by entries here: an entry without its own author shows no byline rather
                // than the feed's, which is what SimplePie does.
                super::xml::skip(r, QName(name));
            }
            "entry" if !is_empty => {
                if let Some(item) = parse_entry(r, name) {
                    feed.items.push(item);
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(name)),
            _ => {}
        }
    });

    Ok(feed)
}

/// Advances to the first element with the given local name, returning its full name and attributes.
fn find_element(r: &mut Reader<&[u8]>, wanted: &str) -> Option<(String, super::xml::Attrs)> {
    loop {
        match r.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let name = e.name().into_inner().to_owned();
                if split(&name).1 == wanted {
                    let found = attrs(&e, r);
                    return Some((name, found));
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn leaf(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> Option<String> {
    if is_empty {
        return None;
    }
    let raw = text(r, QName(name));
    let normalized = crate::text::normalize_space(&raw);
    (!normalized.is_empty()).then(|| normalized.into_owned())
}

/// Records an Atom `<link>` on the feed.
///
/// `rel` defaults to `alternate` per RFC 4287 §4.2.7, and the `alternate` link is the human-facing
/// site — the Atom equivalent of RSS's `<link>`.
fn apply_feed_link(feed: &mut RawFeed, a: &super::xml::Attrs) {
    let Some(href) = attr(a, "href") else { return };
    if !is_remote_uri(href) {
        return;
    }
    match attr(a, "rel").unwrap_or("alternate") {
        "self" => {
            feed.self_url.get_or_insert_with(|| href.to_owned());
        }
        "hub" => {
            feed.hub_url.get_or_insert_with(|| href.to_owned());
        }
        "alternate" => {
            feed.website.get_or_insert_with(|| href.to_owned());
        }
        _ => {}
    }
}

/// Parses one `<entry>`.
fn parse_entry(r: &mut Reader<&[u8]>, name: &str) -> Option<RawItem> {
    let mut item = RawItem::default();
    let mut categories: Vec<String> = Vec::new();
    let mut author_names: Vec<String> = Vec::new();
    let mut author_emails: Vec<String> = Vec::new();
    // `<content>` outranks `<summary>`, but they can arrive in either order.
    let mut summary: Option<String> = None;
    // Collected separately because `<updated>` may precede `<published>` in the document, and
    // publication date must win regardless of order.
    let mut published: Option<i64> = None;
    let mut updated: Option<i64> = None;

    walk(r, name, |child, e, is_empty, r| {
        let (prefix, tag) = split(child);
        let is_media = prefix == Some("media");
        let a = attrs(e, r);

        match tag {
            "id" if !is_media => {
                // Atom requires `<id>`, and it is never a URL — so `guid_is_permalink` stays false.
                item.guid = Some(crate::guid::safe_ascii(
                    leaf_or_empty(r, child, is_empty).trim(),
                ));
            }
            "title" if !is_media => {
                item.title.get_or_insert_with(|| content_body(r, child, is_empty, &a).unwrap_or_default());
            }
            "link" => {
                let Some(href) = attr(&a, "href") else {
                    if !is_empty {
                        super::xml::skip(r, QName(child));
                    }
                    return;
                };
                match attr(&a, "rel").unwrap_or("alternate") {
                    "enclosure" => {
                        if let Some(enc) = enclosure_from_attrs(&a) {
                            item.enclosures.push(enc);
                        }
                    }
                    "alternate" | "related"
                        if item.link.is_none() && is_remote_uri(href) =>
                    {
                        item.link = Some(href.to_owned());
                    }
                    _ => {}
                }
            }
            "published" if published.is_none() => {
                published = date::parse_date(&leaf_or_empty(r, child, is_empty));
            }
            "updated" | "modified" if updated.is_none() => {
                updated = date::parse_date(&leaf_or_empty(r, child, is_empty));
            }
            "content" if !is_media => {
                if let Some(body) = content_body(r, child, is_empty, &a) {
                    item.content_html.get_or_insert(body);
                }
            }
            "summary" if !is_media => {
                if let Some(body) = content_body(r, child, is_empty, &a) {
                    summary.get_or_insert(body);
                }
            }
            "author" | "contributor" if !is_empty => {
                let (names, emails) = parse_person(r, child);
                if tag == "author" {
                    author_names.extend(names);
                    author_emails.extend(emails);
                }
            }
            "category" if !is_media => {
                // Atom carries the keyword in `term` and an optional human label.
                let label = attr(&a, "label").filter(|l| !l.trim().is_empty());
                let term = attr(&a, "term").unwrap_or_default();
                categories.push(label.unwrap_or(term).to_owned());
            }
            "thumbnail" if is_media => {
                if let Some(t) = thumbnail_from_attrs(&a) {
                    item.thumbnail.get_or_insert(t);
                }
            }
            "rights" | "source" | "generator" | "issued" | "created" if !is_empty => {
                super::xml::skip(r, QName(child));
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });

    item.authors = if author_names.iter().any(|n| !n.trim().is_empty()) {
        author_names
    } else {
        author_emails
    };
    item.authors.retain(|a| !a.trim().is_empty());

    // `<updated>` is the only mandatory date in Atom, so a feed that publishes no `<published>`
    // still needs one for ordering.
    item.date = published.or(updated);
    item.updated = updated;

    item.tags = categories.iter().flat_map(|l| split_tags(l)).collect::<Vec<_>>();
    unique_in_place(&mut item.tags);

    // An entry with only a summary still has something to show.
    if item.content_html.is_none() && item.content_text.is_none() {
        item.content_text = summary;
    }

    (!item.is_empty()).then_some(item)
}

/// Reads an Atom person construct: `<author><name>…</name><email>…</email></author>`.
fn parse_person(r: &mut Reader<&[u8]>, name: &str) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut emails = Vec::new();
    walk(r, name, |child, _, is_empty, r| {
        match split(child).1 {
            "name" => {
                if let Some(v) = leaf(r, child, is_empty) {
                    names.push(v);
                }
            }
            "email" | "uri" => {
                if let Some(v) = leaf(r, child, is_empty) {
                    emails.push(v);
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });
    (names, emails)
}

/// Reads an Atom text construct, honouring its `type` attribute.
///
/// All three forms come back as HTML-safe markup, because everything downstream of the parser treats
/// a body as HTML:
///
/// - `type="xhtml"` — inline XML wrapped in an XHTML `<div>`. The wrapper must come off, or every
///   Atom article renders inside an extra block-level box and the change-detection hash shifts.
/// - `type="text"` — plain text. It is escaped, so a body containing `a < b` cannot be read as a tag.
/// - anything else (`html`, absent) — already-escaped markup, passed through.
fn content_body(
    r: &mut Reader<&[u8]>,
    name: &str,
    is_empty: bool,
    a: &super::xml::Attrs,
) -> Option<String> {
    if is_empty {
        return None;
    }
    let raw = text(r, QName(name));
    let kind = attr(a, "type").unwrap_or_default().trim().to_ascii_lowercase();
    let body = match kind.as_str() {
        "xhtml" => strip_xhtml_wrapper(&raw),
        "text" | "text/plain" => crate::entities::encode_specials(&raw).into_owned(),
        _ => raw,
    };
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Removes the XHTML wrapper `<div>` an Atom `type="xhtml"` construct requires.
///
/// Only a *balanced* outermost wrapper is removed. `<div>a</div><div>b</div>` is two sibling boxes,
/// not a wrapper, and stripping it would splice their contents together — so the check walks the
/// body tracking `div` depth and requires the wrapper to close exactly at the end.
fn strip_xhtml_wrapper(body: &str) -> String {
    let trimmed = body.trim();
    let Some(inner_start) = open_div_end(trimmed) else {
        return trimmed.to_owned();
    };
    match matching_close_div(&trimmed[inner_start..]) {
        Some(close_at) if inner_start + close_at + "</div>".len() == trimmed.len() => {
            trimmed[inner_start..inner_start + close_at].to_owned()
        }
        _ => trimmed.to_owned(),
    }
}

/// Finds the `</div>` that closes the innermost open `<div>`, relative to `body`.
///
/// Returns `None` when the tags never balance, which is how an unclosed wrapper is left alone.
fn matching_close_div(body: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let mut depth = 0usize;
    let mut at = 0usize;

    while let Some(rel) = lower[at..].find('<') {
        let pos = at + rel;
        let tag = &lower[pos..];
        if let Some(rest) = tag.strip_prefix("</div") {
            if rest.starts_with('>') || rest.starts_with(char::is_whitespace) {
                if depth == 0 {
                    return Some(pos);
                }
                depth -= 1;
            }
            at = pos + 5;
        } else if let Some(rest) = tag.strip_prefix("<div") {
            if rest.starts_with('>') || rest.starts_with(char::is_whitespace) || rest.starts_with('/') {
                depth += 1;
            }
            at = pos + 4;
        } else {
            at = pos + 1;
        }
    }
    None
}

/// Returns the index just past an opening `<div …>` at the start of `body`.
fn open_div_end(body: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let rest = lower.strip_prefix("<div")?;
    let next = rest.chars().next()?;
    if !(next.is_whitespace() || next == '>') {
        return None;
    }
    let close = rest.find('>')?;
    Some(body.len() - rest.len() + close + 1)
}

fn leaf_or_empty(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> String {
    if is_empty {
        return String::new();
    }
    crate::text::normalize_space(&text(r, QName(name))).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://example.com/">
  <title>Example Atom</title>
  <subtitle type="html">An &lt;b&gt;example&lt;/b&gt;</subtitle>
  <link rel="alternate" href="https://example.com/"/>
  <link rel="self" href="https://example.com/feed.atom"/>
  <link rel="hub" href="https://hub.example.com/"/>
  <icon>https://example.com/icon.png</icon>
  <updated>2006-01-02T15:04:05Z</updated>
  <id>urn:uuid:feed-1</id>
  {entries}
</feed>"#
        )
    }

    fn one(entry: &str) -> RawItem {
        let feed = parse(&atom(entry)).unwrap();
        assert_eq!(feed.items.len(), 1, "expected exactly one entry");
        feed.items.into_iter().next().unwrap()
    }

    #[test]
    fn feed_metadata_is_read() {
        let feed = parse(&atom("")).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Example Atom"));
        assert_eq!(feed.description.as_deref(), Some("An <b>example</b>"));
        assert_eq!(feed.website.as_deref(), Some("https://example.com/"));
        assert_eq!(feed.self_url.as_deref(), Some("https://example.com/feed.atom"));
        assert_eq!(feed.hub_url.as_deref(), Some("https://hub.example.com/"));
        assert_eq!(feed.icon.as_deref(), Some("https://example.com/icon.png"));
        assert_eq!(feed.base_url.as_deref(), Some("https://example.com/"));
        assert_eq!(feed.format, FeedFormat::Atom);
    }

    #[test]
    fn a_document_with_no_feed_root_is_an_error() {
        let err = parse("<html><body>x</body></html>").unwrap_err();
        assert!(err.to_string().contains("<feed>"), "{err}");
    }

    #[test]
    fn the_basic_entry_shape() {
        let item = one(
            r#"<entry>
                 <id>urn:uuid:1</id>
                 <title>Hello</title>
                 <link rel="alternate" href="https://example.com/a"/>
                 <published>2006-01-02T15:04:05Z</published>
                 <updated>2006-01-03T15:04:05Z</updated>
                 <content type="html">&lt;p&gt;Body&lt;/p&gt;</content>
               </entry>"#,
        );
        assert_eq!(item.guid.as_deref(), Some("urn:uuid:1"));
        assert!(!item.guid_is_permalink, "an Atom id is never a URL");
        assert_eq!(item.title.as_deref(), Some("Hello"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
        assert_eq!(item.date, Some(1_136_214_245));
        assert_eq!(item.updated, Some(1_136_300_645));
        assert_eq!(item.effective_content(), Some("<p>Body</p>"));
    }

    #[test]
    fn a_link_with_no_rel_is_the_alternate() {
        let item = one(
            r#"<entry><id>1</id><title>T</title><link href="https://example.com/a"/></entry>"#,
        );
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn rel_enclosure_becomes_an_attachment_not_the_link() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <link rel="enclosure" href="https://example.com/e.mp3" type="audio/mpeg" length="99"/>
             </entry>"#,
        );
        assert_eq!(item.link, None);
        assert_eq!(item.enclosures.len(), 1);
        assert_eq!(item.enclosures[0].length, Some(99));
    }

    #[test]
    fn rel_self_does_not_become_the_article_link() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <link rel="self" href="https://example.com/feed.atom#1"/>
               <link rel="alternate" href="https://example.com/a"/>
             </entry>"#,
        );
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn updated_is_used_when_there_is_no_published() {
        let item = one(
            r#"<entry><id>1</id><title>T</title><updated>2006-01-02T15:04:05Z</updated></entry>"#,
        );
        assert_eq!(item.date, Some(1_136_214_245), "an entry with only <updated> still gets a date");
        assert_eq!(item.updated, Some(1_136_214_245));
    }

    #[test]
    fn published_outranks_updated_for_the_publication_date() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <updated>2020-01-01T00:00:00Z</updated>
               <published>2006-01-02T15:04:05Z</published></entry>"#,
        );
        assert_eq!(item.date, Some(1_136_214_245));
        assert_eq!(item.updated, Some(1_577_836_800));
    }

    #[test]
    fn content_outranks_summary() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <summary>short</summary>
               <content type="html">&lt;p&gt;full&lt;/p&gt;</content></entry>"#,
        );
        assert_eq!(item.effective_content(), Some("<p>full</p>"));
    }

    #[test]
    fn a_summary_only_entry_still_has_a_body() {
        let item = one(r#"<entry><id>1</id><title>T</title><summary>short</summary></entry>"#);
        assert_eq!(item.effective_content(), Some("short"));
    }

    #[test]
    fn content_arriving_after_summary_still_wins() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <content type="html">&lt;p&gt;full&lt;/p&gt;</content>
               <summary>short</summary></entry>"#,
        );
        assert_eq!(item.effective_content(), Some("<p>full</p>"));
    }

    #[test]
    fn type_text_content_is_escaped_rather_than_treated_as_markup() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <content type="text">a &lt; b &amp; c</content></entry>"#,
        );
        // The XML parser already unescaped once, so the body is `a < b & c`. Escaping it again keeps
        // the `<` from becoming a tag when the sanitizer and the reader see the body as HTML.
        assert_eq!(item.effective_content(), Some("a &lt; b &amp; c"));
        assert!(!item.effective_content().unwrap().contains('<'));
    }

    #[test]
    fn type_xhtml_strips_the_wrapper_div() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>Body</p></div></content>
             </entry>"#,
        );
        assert_eq!(item.effective_content(), Some("<p>Body</p>"));
    }

    #[test]
    fn an_xhtml_wrapper_is_only_stripped_when_it_wraps_the_whole_body() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <content type="xhtml"><div>one</div><div>two</div></content>
             </entry>"#,
        );
        assert_eq!(item.effective_content(), Some("<div>one</div><div>two</div>"));
    }

    #[test]
    fn strip_xhtml_wrapper_leaves_a_non_div_body_alone() {
        assert_eq!(strip_xhtml_wrapper("<p>x</p>"), "<p>x</p>");
        assert_eq!(strip_xhtml_wrapper("<div>unclosed"), "<div>unclosed");
        assert_eq!(strip_xhtml_wrapper("<div>a</div> trailing"), "<div>a</div> trailing");
        assert_eq!(strip_xhtml_wrapper("<div class='c'>a</div>"), "a");
        assert_eq!(strip_xhtml_wrapper("<divider>a</divider>"), "<divider>a</divider>");
    }

    #[test]
    fn a_person_construct_yields_the_name_not_the_email() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <author><name>Jane Doe</name><email>jane@example.com</email></author>
             </entry>"#,
        );
        assert_eq!(item.authors, vec!["Jane Doe"]);
    }

    #[test]
    fn an_author_with_only_an_email_shows_the_email() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <author><email>jane@example.com</email></author></entry>"#,
        );
        assert_eq!(item.authors, vec!["jane@example.com"]);
    }

    #[test]
    fn multiple_authors_are_kept_in_order() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <author><name>Ada</name></author>
               <author><name>Grace</name></author></entry>"#,
        );
        assert_eq!(item.authors, vec!["Ada", "Grace"]);
    }

    #[test]
    fn category_term_becomes_a_tag_and_label_outranks_it() {
        let item = one(
            r#"<entry><id>1</id><title>T</title>
               <category term="rust"/>
               <category term="x" label="Reader, RSS"/>
             </entry>"#,
        );
        assert_eq!(item.tags, vec!["rust", "Reader", "RSS"]);
    }

    #[test]
    fn feed_level_author_is_not_inherited_by_entries() {
        let feed = parse(&atom(
            r#"<author><name>Feed Owner</name></author>
               <entry><id>1</id><title>T</title></entry>"#,
        ))
        .unwrap();
        assert!(feed.items[0].authors.is_empty());
    }

    #[test]
    fn media_thumbnail_is_picked_up() {
        let item = one(
            r#"<entry xmlns:media="http://search.yahoo.com/mrss/"><id>1</id><title>T</title>
               <media:thumbnail url="https://example.com/t.jpg"/></entry>"#,
        );
        assert_eq!(item.thumbnail.as_ref().map(|t| t.url.as_str()), Some("https://example.com/t.jpg"));
    }

    #[test]
    fn a_non_remote_icon_or_link_is_dropped() {
        let feed = parse(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
                 <title>T</title><icon>data:image/png;base64,AAAA</icon>
                 <link href="javascript:alert(1)"/>
               </feed>"#,
        )
        .unwrap();
        assert_eq!(feed.icon, None);
        assert_eq!(feed.website, None);
    }

    #[test]
    fn an_entry_with_nothing_but_an_id_is_dropped() {
        let feed = parse(&atom(r#"<entry><id>1</id></entry>"#)).unwrap();
        assert!(feed.items.is_empty());
    }

    #[test]
    fn entries_keep_their_document_order() {
        let feed = parse(&atom(
            r#"<entry><id>1</id><title>newest</title></entry>
               <entry><id>2</id><title>oldest</title></entry>"#,
        ))
        .unwrap();
        let titles: Vec<_> = feed.items.iter().filter_map(|i| i.title.as_deref()).collect();
        assert_eq!(titles, vec!["newest", "oldest"]);
    }

    #[test]
    fn an_empty_feed_is_not_an_error() {
        let feed = parse(&atom("")).unwrap();
        assert!(feed.items.is_empty());
        assert_eq!(feed.title.as_deref(), Some("Example Atom"));
    }
}
