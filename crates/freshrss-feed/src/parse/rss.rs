//! RSS 2.0.
//!
//! Also handles the namespace extensions that effectively every real RSS feed uses: `content:encoded`
//! for the full body, `dc:creator`/`dc:date` for authorship and publication, `media:*` for
//! thumbnails and attachments, `atom:link` for self/hub, and `itunes:*` for podcast metadata.
//!
//! Matching is on the *local* element name, because the prefix a generator picks is arbitrary — the
//! same `content:encoded` appears as `content:`, `c:` or nothing at all depending on the software.

use quick_xml::name::QName;
use quick_xml::Reader;

use super::common::{
    enclosure_from_attrs, is_remote_uri, split_tags, thumbnail_from_attrs, unique_in_place,
};
use super::xml::{attr, attrs, reader, split, text, walk};
use super::{date, FeedError};
use crate::raw::{FeedFormat, RawEnclosure, RawFeed, RawItem, RawThumbnail};

/// Parses an RSS 2.0 document.
///
/// # Errors
///
/// [`FeedError::Parse`] when there is no `<channel>` element at all. A channel with no items is
/// *not* an error: an empty feed is a legitimate state, and treating it as a parse failure would
/// mark a healthy subscription as broken.
pub fn parse(document: &str) -> Result<RawFeed, FeedError> {
    let mut r = reader(document);
    let mut feed = RawFeed {
        format: FeedFormat::Rss2,
        ..RawFeed::default()
    };

    // Find `<channel>`. In a well-formed document it is `<rss>`'s only child, but feeds do put
    // stray elements and whitespace around it.
    let Some((channel_name, base_url)) = find_element(&mut r, "channel") else {
        return Err(FeedError::Parse(
            "RSS document has no <channel> element".to_owned(),
        ));
    };
    feed.base_url = base_url;

    walk(&mut r, &channel_name, |name, e, is_empty, r| {
        let (_, tag) = split(name);
        match tag {
            "title" if feed.title.is_none() => feed.title = leaf(r, name, is_empty),
            "description" if feed.description.is_none() => {
                feed.description = leaf(r, name, is_empty);
            }
            "link" => {
                // `<atom:link href=…>` and RSS's own `<link>` share a local name; the `href`
                // attribute is what tells them apart.
                let a = attrs(e, r);
                match attr(&a, "href") {
                    Some(href) => apply_feed_link(&mut feed, &a, href),
                    None if !is_empty => {
                        if let Some(url) = leaf(r, name, false).filter(|s| !s.is_empty()) {
                            if feed.website.is_none() {
                                feed.website = Some(url);
                            }
                        }
                    }
                    None => {}
                }
            }
            "image" if !is_empty => {
                if let Some(icon) = image_url(r, name) {
                    feed.icon.get_or_insert(icon);
                }
            }
            "item" if !is_empty => {
                if let Some(item) = parse_item(r, name) {
                    feed.items.push(item);
                }
            }
            // `lastBuildDate`, `language`, `generator`, `copyright`, `managingEditor`, …: nothing
            // downstream reads them, and skipping keeps the loop cheap on large feeds.
            _ if !is_empty => super::xml::skip(r, QName(name)),
            _ => {}
        }
    });

    Ok(feed)
}

/// Advances to the first element with the given local name.
///
/// Returns the element's full name plus any `xml:base` seen on it or on an element passed over on
/// the way — which in practice means `<rss>`, since that is the only thing before `<channel>`.
fn find_element(r: &mut Reader<&[u8]>, wanted: &str) -> Option<(String, Option<String>)> {
    let mut base = None;
    loop {
        match r.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let name = e.name().into_inner().to_owned();
                let found = attrs(&e, r);
                if let Some(href) = attr(&found, "base")
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                {
                    base.get_or_insert(href.to_owned());
                }
                if split(&name).1 == wanted {
                    return Some((name, base));
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Reads a leaf element's text, trimmed and whitespace-normalized, or `None` when blank.
fn leaf(r: &mut Reader<&[u8]>, name: &str, is_empty: bool) -> Option<String> {
    if is_empty {
        return None;
    }
    let raw = text(r, QName(name));
    let normalized = crate::text::normalize_space(&raw);
    (!normalized.is_empty()).then(|| normalized.into_owned())
}

/// Records an `<atom:link>` on the channel.
fn apply_feed_link(feed: &mut RawFeed, a: &super::xml::Attrs, href: &str) {
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

/// Reads `<image><url>`, the channel icon.
fn image_url(r: &mut Reader<&[u8]>, name: &str) -> Option<String> {
    let mut url = None;
    walk(r, name, |child, e, is_empty, r| {
        if split(child).1 == "url" && url.is_none() {
            let a = attrs(e, r);
            url = attr(&a, "href")
                .map(str::to_owned)
                .or_else(|| leaf(r, child, is_empty));
        } else if !is_empty {
            super::xml::skip(r, QName(child));
        }
    });
    url.filter(|u| is_remote_uri(u))
}

/// Parses one `<item>`.
fn parse_item(r: &mut Reader<&[u8]>, name: &str) -> Option<RawItem> {
    let mut item = RawItem::default();
    // `dc:creator` is a person's name; RSS `<author>` is an email address. FreshRSS prefers the
    // name, so they are collected separately and merged at the end.
    let mut creators: Vec<String> = Vec::new();
    let mut emails: Vec<String> = Vec::new();
    let mut categories: Vec<String> = Vec::new();

    walk(r, name, |child, e, is_empty, r| {
        let (prefix, tag) = split(child);
        let is_media = prefix == Some("media");
        let a = attrs(e, r);

        match tag {
            "title" if !is_media => {
                if let Some(value) = leaf(r, child, is_empty) {
                    item.title.get_or_insert(value);
                }
            }
            "link" => {
                match attr(&a, "href") {
                    // `<atom:link rel="enclosure" href=…>` is an attachment, not the article URL.
                    Some(_) if attr(&a, "rel") == Some("enclosure") => {
                        if let Some(enc) = enclosure_from_attrs(&a) {
                            item.enclosures.push(enc);
                        }
                    }
                    Some(href) => {
                        if is_remote_uri(href) {
                            item.link.get_or_insert_with(|| href.to_owned());
                        }
                    }
                    None if !is_empty => {
                        if let Some(url) = leaf(r, child, false) {
                            item.link.get_or_insert(url);
                        }
                    }
                    None => {}
                }
            }
            "guid" => {
                // `isPermaLink` defaults to true in RSS 2.0, and only an explicit false opts out.
                item.guid_is_permalink = !matches!(
                    attr(&a, "isPermaLink")
                        .map(|v| v.trim().to_ascii_lowercase())
                        .as_deref(),
                    Some("false") | Some("0") | Some("no")
                );
                let value = if is_empty {
                    String::new()
                } else {
                    text(r, QName(child))
                };
                // `Some("")` is preserved: a feed that emits an empty `<guid/>` did supply an id
                // element, and that is a different situation from supplying none at all.
                item.guid = Some(crate::guid::safe_ascii(value.trim()));
            }
            "pubDate" | "date" | "published" if item.date.is_none() => {
                let value = leaf_or_empty(r, child, is_empty);
                item.date = date::parse_date(&value);
            }
            "modified" | "updated" if item.updated.is_none() => {
                let value = leaf_or_empty(r, child, is_empty);
                item.updated = date::parse_date(&value);
            }
            "creator" => creators.push(leaf_or_empty(r, child, is_empty)),
            "author" => {
                // `<itunes:author>` is a name; RSS 2.0 `<author>` is `email (name)`.
                let value = leaf_or_empty(r, child, is_empty);
                if prefix == Some("itunes") {
                    creators.push(value);
                } else {
                    emails.push(value);
                }
            }
            "encoded" => {
                // `content:encoded` is the full body and wins over `<description>`.
                let body = if is_empty {
                    String::new()
                } else {
                    text(r, QName(child))
                };
                if !body.trim().is_empty() {
                    item.content_html.get_or_insert(body);
                }
            }
            "description" if !is_media => {
                let body = if is_empty {
                    String::new()
                } else {
                    text(r, QName(child))
                };
                if !body.trim().is_empty() {
                    item.content_text.get_or_insert(body);
                }
            }
            "category" if !is_media => {
                let label = leaf_or_empty(r, child, is_empty);
                let from_attr = attr(&a, "term").unwrap_or_default().to_owned();
                categories.push(if label.is_empty() { from_attr } else { label });
            }
            "enclosure" => {
                if let Some(enc) = enclosure_from_attrs(&a) {
                    item.enclosures.push(enc);
                }
            }
            "thumbnail" if is_media => {
                if let Some(t) = thumbnail_from_attrs(&a) {
                    item.thumbnail.get_or_insert(t);
                }
            }
            "content" if is_media => {
                if let Some(enc) = media_content(&a) {
                    item.enclosures.push(enc);
                }
            }
            "group" if is_media && !is_empty => parse_media_group(r, child, &mut item),
            "player" | "credit" | "rating" | "copyright" | "rights" | "summary" | "keywords"
            | "subtitle" | "content" | "description" | "title"
                if is_media =>
            {
                // Media RSS elements with text we do not model.
                if !is_empty {
                    super::xml::skip(r, QName(child));
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });

    item.authors = merge_authors(&creators, &emails);
    item.tags = categories
        .iter()
        .flat_map(|label| split_tags(label))
        .collect::<Vec<_>>();
    unique_in_place(&mut item.tags);

    // A permalink GUID doubles as the article URL when the item has no `<link>`.
    if item.link.is_none() && item.guid_is_permalink {
        if let Some(guid) = item.guid.as_deref() {
            if is_remote_uri(guid) {
                item.link = Some(guid.to_owned());
            }
        }
    }
    if item.thumbnail.is_none() {
        item.thumbnail = thumbnail_from_enclosures(&item.enclosures);
    }

    (!item.is_empty()).then_some(item)
}

/// Reads the renditions inside a `<media:group>`, keeping exactly one.
///
/// A group advertises alternative encodings of the *same* attachment, so pushing all of them would
/// show a video four times in the reader. FreshRSS keeps the `isDefault` rendition when the feed
/// marks one, otherwise the first.
fn parse_media_group(r: &mut Reader<&[u8]>, name: &str, item: &mut RawItem) {
    let mut renditions: Vec<(bool, RawEnclosure)> = Vec::new();

    walk(r, name, |child, e, is_empty, r| {
        let (_, tag) = split(child);
        let a = attrs(e, r);
        match tag {
            "content" => {
                if let Some(enc) = media_content(&a) {
                    let is_default = attr(&a, "isDefault").is_some_and(|v| {
                        matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
                    });
                    renditions.push((is_default, enc));
                }
            }
            "thumbnail" if item.thumbnail.is_none() => {
                if let Some(t) = thumbnail_from_attrs(&a) {
                    item.thumbnail = Some(t);
                }
            }
            _ if !is_empty => super::xml::skip(r, QName(child)),
            _ => {}
        }
    });

    if let Some((_, enc)) = renditions
        .iter()
        .find(|(is_default, _)| *is_default)
        .or_else(|| renditions.first())
    {
        item.enclosures.push(enc.clone());
    }
}

/// A `media:content` element as an enclosure.
///
/// `medium` is what lets the reader decide between an inline image, an `<audio>` player and a
/// `<video>` player, so an image rendition is kept out of the enclosure list — it belongs in
/// `thumbnail`, not in the download list.
fn media_content(a: &super::xml::Attrs) -> Option<RawEnclosure> {
    let mut enc = enclosure_from_attrs(a)?;
    if enc.medium.is_none() {
        enc.medium = enc
            .mime
            .as_deref()
            .and_then(|m| m.split('/').next())
            .filter(|t| matches!(*t, "image" | "audio" | "video" | "text" | "application"))
            .map(str::to_owned);
    }
    Some(enc)
}

/// Falls back to an image enclosure as the thumbnail when the feed sent no `media:thumbnail`.
fn thumbnail_from_enclosures(enclosures: &[RawEnclosure]) -> Option<RawThumbnail> {
    enclosures.iter().find_map(|e| {
        let is_image = e.medium.as_deref() == Some("image")
            || e.mime.as_deref().is_some_and(|m| m.starts_with("image/"));
        is_image.then(|| RawThumbnail {
            url: e.url.clone(),
            height: e.height,
            width: e.width,
            time: None,
        })
    })
}

/// Merges `dc:creator`/`itunes:author` names with RSS `<author>` emails.
///
/// FreshRSS's rule is `name != '' ? name : email` per author, which in aggregate means a feed that
/// supplies real names never shows the reader a bare email address.
fn merge_authors(creators: &[String], emails: &[String]) -> Vec<String> {
    let picked: &[String] = if creators.iter().any(|c| !c.trim().is_empty()) {
        creators
    } else {
        emails
    };
    let mut out: Vec<String> = picked
        .iter()
        .map(|raw| author_from_rss(raw))
        .filter(|name| !name.is_empty())
        .collect();
    out.dedup();
    out
}

/// Extracts the display name from an RSS 2.0 `<author>` value.
///
/// The spec says the element holds an email address, and in practice feeds write either
/// `mail@example.com (Jane Doe)` or just `Jane Doe`. Showing the address when a name is available
/// is the whole reason this exists.
fn author_from_rss(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.find('(') {
        if let Some(close) = trimmed[open..].rfind(')') {
            let name = trimmed[open + 1..open + close].trim();
            if !name.is_empty() {
                return crate::text::normalize_space(name).into_owned();
            }
        }
    }
    crate::text::normalize_space(trimmed).into_owned()
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

    fn rss(items: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"
     xmlns:content="http://purl.org/rss/1.0/modules/content/"
     xmlns:dc="http://purl.org/dc/elements/1.1/"
     xmlns:media="http://search.yahoo.com/mrss/"
     xmlns:atom="http://www.w3.org/2005/Atom">
  <channel>
    <title>Example Feed</title>
    <link>https://example.com</link>
    <description>Just an example</description>
    <image><url>https://example.com/icon.png</url><title>Example</title></image>
    <atom:link rel="self" href="https://example.com/feed.xml"/>
    <atom:link rel="hub" href="https://hub.example.com"/>
    {items}
  </channel>
</rss>"#
        )
    }

    fn one(item: &str) -> RawItem {
        let feed = parse(&rss(item)).unwrap();
        assert_eq!(feed.items.len(), 1, "expected exactly one item");
        feed.items.into_iter().next().unwrap()
    }

    #[test]
    fn channel_metadata_is_read() {
        let feed = parse(&rss("")).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Example Feed"));
        assert_eq!(feed.website.as_deref(), Some("https://example.com"));
        assert_eq!(feed.description.as_deref(), Some("Just an example"));
        assert_eq!(feed.icon.as_deref(), Some("https://example.com/icon.png"));
        assert_eq!(
            feed.self_url.as_deref(),
            Some("https://example.com/feed.xml")
        );
        assert_eq!(feed.hub_url.as_deref(), Some("https://hub.example.com"));
        assert_eq!(feed.format, FeedFormat::Rss2);
    }

    #[test]
    fn a_document_with_no_channel_is_an_error() {
        let err = parse("<rss version=\"2.0\"></rss>").unwrap_err();
        assert!(err.to_string().contains("<channel>"), "{err}");
    }

    #[test]
    fn items_after_a_media_group_are_not_swallowed() {
        // Regression: the `media:group` walk used to compare its full element name against the end
        // tag's local name, never match, and run to EOF — losing every item after the first group.
        let feed = parse(&rss(r#"<item><title>One</title><media:group>
                 <media:content url="https://x/a.mp4" medium="video"/>
               </media:group></item>
               <item><title>Two</title></item>
               <item><title>Three</title></item>"#))
        .unwrap();
        let titles: Vec<_> = feed
            .items
            .iter()
            .filter_map(|i| i.title.as_deref())
            .collect();
        assert_eq!(titles, vec!["One", "Two", "Three"]);
        assert_eq!(feed.items[0].enclosures.len(), 1);
    }

    #[test]
    fn an_empty_feed_is_not_an_error() {
        let feed = parse(&rss("")).unwrap();
        assert!(feed.items.is_empty());
        assert_eq!(feed.title.as_deref(), Some("Example Feed"));
    }

    #[test]
    fn the_basic_item_shape() {
        let item = one(r#"<item>
                 <title>Hello</title>
                 <link>https://example.com/a</link>
                 <guid>tag:example,2006:a</guid>
                 <pubDate>Mon, 02 Jan 2006 15:04:05 +0000</pubDate>
                 <description>&lt;p&gt;Body&lt;/p&gt;</description>
               </item>"#);
        assert_eq!(item.title.as_deref(), Some("Hello"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));
        assert_eq!(item.guid.as_deref(), Some("tag:example,2006:a"));
        assert!(item.guid_is_permalink, "isPermaLink defaults to true");
        assert_eq!(item.date, Some(1_136_214_245));
        assert_eq!(item.effective_content(), Some("<p>Body</p>"));
    }

    #[test]
    fn content_encoded_wins_over_description() {
        let item = one(r#"<item>
                 <title>T</title>
                 <description>summary only</description>
                 <content:encoded><![CDATA[<p>the whole thing</p>]]></content:encoded>
               </item>"#);
        assert_eq!(item.content_text.as_deref(), Some("summary only"));
        assert_eq!(item.content_html.as_deref(), Some("<p>the whole thing</p>"));
        assert_eq!(item.effective_content(), Some("<p>the whole thing</p>"));
    }

    #[test]
    fn cdata_and_escaped_markup_produce_the_same_body() {
        let escaped =
            one(r#"<item><title>T</title><description>&lt;p&gt;Hi&lt;/p&gt;</description></item>"#);
        let cdata =
            one(r#"<item><title>T</title><description><![CDATA[<p>Hi</p>]]></description></item>"#);
        assert_eq!(escaped.effective_content(), cdata.effective_content());
        assert_eq!(escaped.effective_content(), Some("<p>Hi</p>"));
    }

    #[test]
    fn unescaped_markup_inside_description_survives() {
        // Not well-formed XML, but common enough that losing the markup would be visible.
        let item = one(
            r#"<item><title>T</title><description><p>Hi <b>there</b></p></description></item>"#,
        );
        assert_eq!(item.effective_content(), Some("<p>Hi <b>there</b></p>"));
    }

    #[test]
    fn an_explicit_false_permalink_is_recorded() {
        let item = one(r#"<item><title>T</title><guid isPermaLink="false">urn:1</guid></item>"#);
        assert!(!item.guid_is_permalink);
        assert_eq!(item.guid.as_deref(), Some("urn:1"));
        assert_eq!(item.link, None, "a non-permalink guid is not a URL");
    }

    #[test]
    fn a_permalink_guid_supplies_the_link_when_there_is_none() {
        let item = one(r#"<item><title>T</title><guid>https://example.com/a</guid></item>"#);
        assert_eq!(item.link.as_deref(), Some("https://example.com/a"));

        // But a non-permalink guid that happens to look like a URL does not.
        let item = one(
            r#"<item><title>T</title><guid isPermaLink="false">https://example.com/a</guid></item>"#,
        );
        assert_eq!(item.link, None);
    }

    #[test]
    fn an_empty_guid_element_is_some_empty_not_none() {
        let item = one(r#"<item><title>T</title><guid></guid></item>"#);
        assert_eq!(item.guid, Some(String::new()));

        let item = one(r#"<item><title>T</title></item>"#);
        assert_eq!(item.guid, None, "no guid element at all");
    }

    #[test]
    fn dc_date_is_used_when_there_is_no_pubdate() {
        let item = one(r#"<item><title>T</title><dc:date>2006-01-02T15:04:05Z</dc:date></item>"#);
        assert_eq!(item.date, Some(1_136_214_245));
    }

    #[test]
    fn pubdate_wins_over_dc_date() {
        let item = one(r#"<item><title>T</title>
               <pubDate>Mon, 02 Jan 2006 15:04:05 +0000</pubDate>
               <dc:date>2020-01-01T00:00:00Z</dc:date></item>"#);
        assert_eq!(item.date, Some(1_136_214_245));
    }

    #[test]
    fn an_unparseable_date_is_none_not_zero() {
        let item = one(r#"<item><title>T</title><pubDate>not a date</pubDate></item>"#);
        assert_eq!(item.date, None);
    }

    #[test]
    fn dc_creator_is_preferred_over_the_rss_author_email() {
        let item = one(r#"<item><title>T</title>
               <author>jane@example.com (Jane Doe)</author>
               <dc:creator>Jane Q. Doe</dc:creator></item>"#);
        assert_eq!(item.authors, vec!["Jane Q. Doe"]);
    }

    #[test]
    fn an_rss_author_email_has_its_parenthesised_name_extracted() {
        let item =
            one(r#"<item><title>T</title><author>jane@example.com (Jane Doe)</author></item>"#);
        assert_eq!(item.authors, vec!["Jane Doe"]);
    }

    #[test]
    fn a_bare_author_value_is_kept_as_is() {
        let item = one(r#"<item><title>T</title><author>Jane Doe</author></item>"#);
        assert_eq!(item.authors, vec!["Jane Doe"]);
    }

    #[test]
    fn multiple_authors_are_all_kept_in_order() {
        let item = one(r#"<item><title>T</title>
               <dc:creator>Ada</dc:creator><dc:creator>Grace</dc:creator></item>"#);
        assert_eq!(item.authors, vec!["Ada", "Grace"]);
    }

    #[test]
    fn itunes_author_counts_as_a_name() {
        let item = one(
            r#"<item xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
               <title>T</title><itunes:author>The Host</itunes:author></item>"#,
        );
        assert_eq!(item.authors, vec!["The Host"]);
    }

    #[test]
    fn categories_become_tags_and_are_split_on_commas() {
        let item = one(r#"<item><title>T</title>
               <category>rust</category>
               <category>rss, reader</category>
               <category>rust</category></item>"#);
        assert_eq!(item.tags, vec!["rust", "rss", "reader"]);
    }

    #[test]
    fn a_category_term_attribute_is_used_when_the_element_is_empty() {
        let item = one(r#"<item><title>T</title><category term="atom-style"/></item>"#);
        assert_eq!(item.tags, vec!["atom-style"]);
    }

    #[test]
    fn enclosures_are_collected_with_their_metadata() {
        let item = one(r#"<item><title>T</title>
               <enclosure url="https://example.com/e.mp3" length="1024" type="audio/mpeg"/>
             </item>"#);
        assert_eq!(item.enclosures.len(), 1);
        let e = &item.enclosures[0];
        assert_eq!(e.url, "https://example.com/e.mp3");
        assert_eq!(e.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(e.length, Some(1024));
    }

    #[test]
    fn an_atom_link_rel_enclosure_is_an_attachment_not_the_article_url() {
        let item = one(r#"<item><title>T</title>
               <atom:link rel="enclosure" href="https://example.com/e.mp3" type="audio/mpeg"/>
             </item>"#);
        assert_eq!(item.link, None);
        assert_eq!(item.enclosures.len(), 1);
        assert_eq!(item.enclosures[0].url, "https://example.com/e.mp3");
    }

    #[test]
    fn media_thumbnail_is_picked_up() {
        let item = one(r#"<item><title>T</title>
               <media:thumbnail url="https://example.com/t.jpg" width="160" height="90"/>
             </item>"#);
        let t = item.thumbnail.unwrap();
        assert_eq!(t.url, "https://example.com/t.jpg");
        assert_eq!((t.width, t.height), (Some(160), Some(90)));
    }

    #[test]
    fn an_image_enclosure_becomes_the_thumbnail_when_there_is_no_media_thumbnail() {
        let item = one(r#"<item><title>T</title>
               <media:content url="https://example.com/i.jpg" medium="image"/>
             </item>"#);
        assert_eq!(
            item.thumbnail.as_ref().map(|t| t.url.as_str()),
            Some("https://example.com/i.jpg")
        );
        assert_eq!(item.enclosures.len(), 1);
    }

    #[test]
    fn a_media_content_mime_implies_its_medium() {
        let item = one(r#"<item><title>T</title>
               <media:content url="https://example.com/v.mp4" type="video/mp4"/>
             </item>"#);
        assert_eq!(item.enclosures[0].medium.as_deref(), Some("video"));
    }

    #[test]
    fn a_media_group_yields_one_rendition_not_all_of_them() {
        let item = one(r#"<item><title>T</title>
               <media:group>
                 <media:content url="https://example.com/low.mp4" medium="video"/>
                 <media:content url="https://example.com/high.mp4" medium="video" isDefault="true"/>
               </media:group>
             </item>"#);
        // FreshRSS keeps the `isDefault` rendition when the feed marks one…
        assert_eq!(item.enclosures.len(), 1);
        assert_eq!(item.enclosures[0].url, "https://example.com/high.mp4");

        // …and the first one when it does not.
        let item = one(r#"<item><title>T</title>
               <media:group>
                 <media:content url="https://example.com/low.mp4" medium="video"/>
                 <media:content url="https://example.com/high.mp4" medium="video"/>
               </media:group>
             </item>"#);
        assert_eq!(item.enclosures.len(), 1);
        assert_eq!(item.enclosures[0].url, "https://example.com/low.mp4");
    }

    #[test]
    fn non_remote_enclosure_and_thumbnail_urls_are_dropped() {
        let item = one(r#"<item><title>T</title>
               <enclosure url="file:///etc/passwd" type="text/plain"/>
               <media:thumbnail url="data:image/png;base64,AAAA"/>
             </item>"#);
        assert!(item.enclosures.is_empty());
        assert_eq!(item.thumbnail, None);
    }

    #[test]
    fn an_item_with_no_title_link_or_body_is_dropped() {
        let feed = parse(&rss(
            r#"<item><pubDate>Mon, 02 Jan 2006 15:04:05 +0000</pubDate></item>"#,
        ))
        .unwrap();
        assert!(feed.items.is_empty(), "a date alone is not an article");
    }

    #[test]
    fn multiple_items_keep_their_document_order() {
        let feed = parse(&rss(r#"<item><title>newest</title></item>
               <item><title>middle</title></item>
               <item><title>oldest</title></item>"#))
        .unwrap();
        let titles: Vec<_> = feed
            .items
            .iter()
            .filter_map(|i| i.title.as_deref())
            .collect();
        assert_eq!(titles, vec!["newest", "middle", "oldest"]);
        // …and `chronological` is what the refresh pipeline uses for id allocation.
        let chrono: Vec<_> = feed
            .chronological()
            .iter()
            .filter_map(|i| i.title.as_deref())
            .collect();
        assert_eq!(chrono, vec!["oldest", "middle", "newest"]);
    }

    #[test]
    fn whitespace_in_a_title_is_normalized() {
        let item = one("<item><title>\n   Hello\n   world  \n</title></item>");
        assert_eq!(item.title.as_deref(), Some("Hello world"));
    }

    #[test]
    fn html_entities_in_a_title_are_left_for_the_entity_pass() {
        // The XML parser resolves `&amp;`; `&eacute;` is not XML and survives to
        // `html_only_entity_decode`, which the caller runs.
        let item = one(r#"<item><title>Tom &amp; Caf&eacute;</title></item>"#);
        assert_eq!(item.title.as_deref(), Some("Tom & Caf&eacute;"));
        assert_eq!(
            crate::entities::html_only_entity_decode(item.title.as_deref().unwrap()),
            "Tom & Café"
        );
    }

    #[test]
    fn elements_the_parser_does_not_model_are_skipped_without_losing_items() {
        let feed = parse(&rss(
            r#"<item><title>One</title><source url="https://other.example">Other</source></item>
               <item><title>Two</title></item>"#,
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
    fn a_truncated_document_yields_every_item_it_can() {
        let body = r#"<rss><channel><title>T</title>
            <item><title>One</title></item>
            <item><title>Two</title>"#;
        let feed = parse(body).unwrap();
        let titles: Vec<_> = feed
            .items
            .iter()
            .filter_map(|i| i.title.as_deref())
            .collect();
        // The unterminated `<title>` runs to EOF, so "Two" is recovered rather than lost. A feed
        // truncated mid-write is worth reading partially; discarding it is not.
        assert_eq!(titles, vec!["One", "Two"]);
    }

    #[test]
    fn author_from_rss_handles_the_edge_shapes() {
        assert_eq!(author_from_rss("a@b.c (Jane)"), "Jane");
        assert_eq!(
            author_from_rss("a@b.c ()"),
            "a@b.c ()",
            "empty parens are not a name"
        );
        assert_eq!(author_from_rss("  Jane   Doe  "), "Jane Doe");
        assert_eq!(author_from_rss(""), "");
        // The outermost pair is the name, so an address containing parens still works.
        assert_eq!(author_from_rss("(nested (parens))"), "nested (parens)");
        assert_eq!(author_from_rss("Jane (no address)"), "no address");
    }
}
