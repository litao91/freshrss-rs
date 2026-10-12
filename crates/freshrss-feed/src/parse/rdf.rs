//! RSS 1.0 / RDF.
//!
//! Best-effort, as the plan's scope line says: RDF feeds are rare now, but they are also the ones
//! where the document shape differs most, and getting that wrong means an entire class of feed
//! silently parses to zero items.
//!
//! The structural difference that matters is that `<item>` elements are **siblings** of `<channel>`
//! under `<rdf:RDF>`, not children of it — the opposite of RSS 2.0. Everything else is the RSS
//! vocabulary plus Dublin Core.

use quick_xml::name::QName;
use quick_xml::Reader;

use super::common::{
    enclosure_from_attrs, is_remote_uri, split_tags, thumbnail_from_attrs, unique_in_place,
};
use super::xml::{attr, attrs, reader, split, text, walk};
use super::{date, FeedError};
use crate::raw::{FeedFormat, RawFeed, RawItem};

/// Parses an RDF Site Summary document.
///
/// # Errors
///
/// [`FeedError::Parse`] when there is no `<rdf:RDF>` root.
pub fn parse(document: &str) -> Result<RawFeed, FeedError> {
    let mut r = reader(document);
    let mut feed = RawFeed {
        format: FeedFormat::Rss1,
        ..RawFeed::default()
    };

    let Some(root) = find_element(&mut r, "RDF") else {
        return Err(FeedError::Parse(
            "RDF document has no <rdf:RDF> root element".to_owned(),
        ));
    };

    // `<item>`s live at the root next to `<channel>`, so one walk collects both.
    walk(&mut r, &root, |name, e, is_empty, r| {
        let (_, tag) = split(name);
        match tag {
            "channel" if !is_empty => parse_channel(r, name, &mut feed),
            "image" if !is_empty => {
                if let Some(icon) = image_url(r, name) {
                    feed.icon.get_or_insert(icon);
                }
            }
            "item" if !is_empty => {
                let a = attrs(e, r);
                if let Some(item) = parse_item(r, name, attr(&a, "about")) {
                    feed.items.push(item);
                }
            }
            "textinput" if !is_empty => super::xml::skip(r, QName(name)),
            _ if !is_empty => super::xml::skip(r, QName(name)),
            _ => {}
        }
    });

    Ok(feed)
}

fn find_element(r: &mut Reader<&[u8]>, wanted: &str) -> Option<String> {
    loop {
        match r.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let name = e.name().into_inner().to_owned();
                if split(&name).1 == wanted {
                    return Some(name);
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn parse_channel(r: &mut Reader<&[u8]>, name: &str, feed: &mut RawFeed) {
    walk(r, name, |child, e, is_empty, r| {
        let (_, tag) = split(child);
        match tag {
            "title" if feed.title.is_none() => feed.title = leaf(r, child, is_empty),
            "description" if feed.description.is_none() => {
                feed.description = leaf(r, child, is_empty)
            }
            "link" if feed.website.is_none() => {
                let a = attrs(e, r);
                match attr(&a, "href") {
                    Some(href) => feed.website = is_remote_uri(href).then(|| href.to_owned()),
                    None => feed.website = leaf(r, child, is_empty),
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });
}

fn image_url(r: &mut Reader<&[u8]>, name: &str) -> Option<String> {
    let mut url = None;
    walk(r, name, |child, e, is_empty, r| {
        if split(child).1 == "url" && url.is_none() {
            let a = attrs(e, r);
            url = attr(&a, "href")
                .filter(|h| is_remote_uri(h))
                .map(str::to_owned)
                .or_else(|| leaf(r, child, is_empty).filter(|u| is_remote_uri(u)));
        } else if !is_empty {
            super::xml::skip(r, QName(child));
        }
    });
    url
}

/// Parses one RDF `<item>`.
///
/// `about` is the `rdf:about` attribute, which in RSS 1.0 is the item's canonical URL and is a
/// better identity than anything else the format offers.
fn parse_item(r: &mut Reader<&[u8]>, name: &str, about: Option<&str>) -> Option<RawItem> {
    let mut item = RawItem::default();
    let mut creators: Vec<String> = Vec::new();
    let mut categories: Vec<String> = Vec::new();

    walk(r, name, |child, e, is_empty, r| {
        let (prefix, tag) = split(child);
        let is_media = prefix == Some("media");
        let a = attrs(e, r);

        match tag {
            "title" if !is_media => {
                item.title
                    .get_or_insert_with(|| leaf_or_empty(r, child, is_empty));
            }
            "link" => match attr(&a, "href") {
                Some(_) if attr(&a, "rel") == Some("enclosure") => {
                    if let Some(enc) = enclosure_from_attrs(&a) {
                        item.enclosures.push(enc);
                    }
                }
                Some(href) => {
                    if is_remote_uri(href) {
                        item.link.get_or_insert(href.to_owned());
                    }
                }
                None if !is_empty => {
                    if let Some(url) = leaf(r, child, false) {
                        item.link.get_or_insert(url);
                    }
                }
                None => {}
            },
            // RDF has no `<guid>`; `rdf:about` fills the role.
            "about" if item.guid.is_none() => {}
            "date" | "pubDate" | "published" if item.date.is_none() => {
                item.date = date::parse_date(&leaf_or_empty(r, child, is_empty));
            }
            "creator" => creators.push(leaf_or_empty(r, child, is_empty)),
            "author" => creators.push(leaf_or_empty(r, child, is_empty)),
            "encoded" => {
                let body = leaf_raw(r, child, is_empty);
                if !body.trim().is_empty() {
                    item.content_html.get_or_insert(body);
                }
            }
            "description" if !is_media => {
                let body = leaf_raw(r, child, is_empty);
                if !body.trim().is_empty() {
                    item.content_text.get_or_insert(body);
                }
            }
            "subject" | "category" => {
                categories.push(attr(&a, "term").unwrap_or_default().to_owned());
                if !is_empty {
                    categories.push(leaf_or_empty(r, child, is_empty));
                }
            }
            "thumbnail" if is_media => {
                if let Some(t) = thumbnail_from_attrs(&a) {
                    item.thumbnail.get_or_insert(t);
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });

    // `rdf:about` is used when the item supplied no better identity.
    if item.guid.is_none() {
        if let Some(about) = about.map(str::trim).filter(|a| !a.is_empty()) {
            item.guid = Some(crate::guid::safe_ascii(about));
            if item.link.is_none() && is_remote_uri(about) {
                item.link = Some(about.to_owned());
            }
        }
    }

    item.authors = creators
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .collect();
    item.tags = categories
        .iter()
        .flat_map(|label| split_tags(label))
        .collect::<Vec<_>>();
    unique_in_place(&mut item.tags);

    (!item.is_empty()).then_some(item)
}

fn leaf(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> Option<String> {
    if is_empty {
        return None;
    }
    let raw = text(r, QName(name));
    let normalized = crate::text::normalize_space(&raw);
    (!normalized.is_empty()).then(|| normalized.into_owned())
}

fn leaf_or_empty(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> String {
    if is_empty {
        return String::new();
    }
    crate::text::normalize_space(&text(r, QName(name))).into_owned()
}

/// Like [`leaf_or_empty`] but keeps the whitespace inside the body intact.
///
/// Article markup is indented by its source document, and collapsing that would change the
/// change-detection hash on every refresh of a feed whose generator indents differently.
fn leaf_raw(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> String {
    if is_empty {
        return String::new();
    }
    text(r, QName(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rdf(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns="http://purl.org/rss/1.0/"
         xmlns:dc="http://purl.org/dc/elements/1.1/"
         xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel rdf:about="https://example.com">
    <title>Example RDF</title>
    <link>https://example.com</link>
    <description>An RDF feed</description>
  </channel>
  <image rdf:about="https://example.com/icon.png">
    <url>https://example.com/icon.png</url>
    <title>Example</title>
  </image>
  {body}
</rdf:RDF>"#
        )
    }

    fn one(item: &str) -> RawItem {
        let feed = parse(&rdf(item)).unwrap();
        assert_eq!(feed.items.len(), 1, "expected exactly one item");
        feed.items.into_iter().next().unwrap()
    }

    #[test]
    fn channel_metadata_is_read() {
        let feed = parse(&rdf("")).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Example RDF"));
        assert_eq!(feed.website.as_deref(), Some("https://example.com"));
        assert_eq!(feed.description.as_deref(), Some("An RDF feed"));
        assert_eq!(feed.icon.as_deref(), Some("https://example.com/icon.png"));
        assert_eq!(feed.format, FeedFormat::Rss1);
    }

    #[test]
    fn a_document_with_no_rdf_root_is_an_error() {
        let err = parse("<rss><channel></channel></rss>").unwrap_err();
        assert!(err.to_string().contains("rdf:RDF"), "{err}");
    }

    #[test]
    fn items_are_siblings_of_the_channel_not_children() {
        let feed = parse(&rdf(
            r#"<item rdf:about="https://example.com/a"><title>One</title></item>
               <item rdf:about="https://example.com/b"><title>Two</title></item>"#,
        ))
        .unwrap();
        let titles: Vec<_> = feed
            .items
            .iter()
            .filter_map(|i| i.title.as_deref())
            .collect();
        assert_eq!(titles, vec!["One", "Two"]);
    }

    #[test]
    fn rdf_about_supplies_the_guid_and_the_link() {
        let item = one(r#"<item rdf:about="https://example.com/a"><title>T</title></item>"#);
        assert_eq!(item.guid.as_deref(), Some("https://example.com/a"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn an_explicit_link_outranks_rdf_about() {
        let item = one(
            r#"<item rdf:about="urn:x"><title>T</title><link>https://example.com/a</link></item>"#,
        );
        assert_eq!(item.guid.as_deref(), Some("urn:x"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn a_non_remote_about_is_a_guid_but_not_a_link() {
        let item = one(r#"<item rdf:about="urn:not-a-url"><title>T</title></item>"#);
        assert_eq!(item.guid.as_deref(), Some("urn:not-a-url"));
        assert_eq!(item.link, None);
    }

    #[test]
    fn dc_date_is_the_publication_date() {
        let item = one(r#"<item rdf:about="https://example.com/a"><title>T</title>
               <dc:date>2006-01-02T15:04:05Z</dc:date></item>"#);
        assert_eq!(item.date, Some(1_136_214_245));
    }

    #[test]
    fn dc_creator_is_the_author() {
        let item = one(r#"<item rdf:about="https://example.com/a"><title>T</title>
               <dc:creator>Jane Doe</dc:creator></item>"#);
        assert_eq!(item.authors, vec!["Jane Doe"]);
    }

    #[test]
    fn content_encoded_outranks_description() {
        let item = one(r#"<item rdf:about="https://example.com/a"><title>T</title>
               <description>summary</description>
               <content:encoded><![CDATA[<p>full</p>]]></content:encoded></item>"#);
        assert_eq!(item.content_text.as_deref(), Some("summary"));
        assert_eq!(item.effective_content(), Some("<p>full</p>"));
    }

    #[test]
    fn dc_subject_becomes_a_tag() {
        let item = one(r#"<item rdf:about="https://example.com/a"><title>T</title>
               <dc:subject>rust, rss</dc:subject></item>"#);
        assert_eq!(item.tags, vec!["rust", "rss"]);
    }

    #[test]
    fn an_item_with_no_about_and_no_content_is_dropped() {
        let feed = parse(&rdf(
            r#"<item><dc:date>2006-01-02T15:04:05Z</dc:date></item>"#,
        ))
        .unwrap();
        assert!(feed.items.is_empty());
    }

    #[test]
    fn an_empty_rdf_feed_is_not_an_error() {
        let feed = parse(&rdf("")).unwrap();
        assert!(feed.items.is_empty());
        assert_eq!(feed.title.as_deref(), Some("Example RDF"));
    }

    #[test]
    fn body_whitespace_is_preserved_so_the_hash_is_stable() {
        let item = one(
            "<item rdf:about=\"https://example.com/a\"><title>T</title>\n  <description>\n   <p>x</p>\n  </description></item>",
        );
        assert_eq!(item.content_text.as_deref(), Some("\n   <p>x</p>\n  "));
    }
}
