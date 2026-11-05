//! The JSON shapes the Google Reader API emits, ported from `FreshRSS_Entry::toGReader('compat')`
//! and the surrounding `entriesToArray`.
//!
//! Field names, string-vs-number choices and key order quirks are the PHP's, because clients parse
//! them positionally in practice: item ids are hex tag URIs here and decimal elsewhere, timestamps
//! are seconds in one key and microsecond strings in the next, and `summary` exists while `content`
//! does not.

use serde_json::{json, Value};

use crate::api::greader::escape::{decode_url, escape_to_unicode_alternative};
use crate::domain::entry::Entry;
use crate::domain::feed::Feed;
use crate::domain::priority::Priority;

/// `str_pad(dechex(id), 16, '0', STR_PAD_LEFT)`, wrapped in the tag URI every Reader client knows.
pub fn item_tag_id(id: i64) -> String {
    format!("tag:google.com,2005:reader/item/{:016x}", id)
}

/// The decimal microsecond id, which is what continuation tokens and `stream/items/ids` carry.
pub fn decimal_id(id: i64) -> String {
    id.to_string()
}

/// Parses either item-id spelling: the hex tag URI (or a bare 16-hex) and the decimal string.
///
/// The PHP's rule is "not all digits, or starts with `0`, therefore hex", and a leading zero in a
/// microsecond timestamp is impossible, so the same rule works here.
pub fn parse_item_id(raw: &str) -> i64 {
    let bare = raw.rsplit('/').next().unwrap_or(raw);
    let all_digits = !bare.is_empty() && bare.chars().all(|c| c.is_ascii_digit());
    if all_digits && !bare.starts_with('0') {
        bare.parse::<i64>().unwrap_or(0)
    } else {
        i64::from_str_radix(bare, 16).unwrap_or(0)
    }
}

/// The summary body: the article HTML with enclosures appended, cut at 500 000 bytes.
///
/// `display_enclosures = false` on the feed suppresses the append, which is the one knob FreshRSS
/// gives a reader whose enclosures double the body size.
pub fn summary_content(entry: &Entry, feed: Option<&Feed>) -> String {
    let with_enclosures = feed
        .and_then(|f| f.attributes.get_bool("display_enclosures"))
        .unwrap_or(true);
    let mut content = entry.content.clone();
    if with_enclosures {
        if let Some(thumbnail) = entry.thumbnail() {
            content.push_str(&format!("<p><img src=\"{}\" /></p>", thumbnail.url));
        }
        for enclosure in entry.enclosures() {
            if enclosure.url.is_empty() {
                continue;
            }
            content.push_str(&format!(
                "<p><a href=\"{}\">{}</a></p>",
                enclosure.url,
                enclosure.url
            ));
        }
    }
    cut_at_char_boundary(&content, 500_000)
}

fn cut_at_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_owned()
}

/// One item, in the compat shape.
#[allow(clippy::too_many_arguments)]
pub fn item_json(
    entry: &Entry,
    feed: &Feed,
    category_name: Option<&str>,
    label_names: &[String],
    tag_names: &[String],
) -> Value {
    let id = entry.id.micros();
    let link = decode_url(&entry.link);

    let mut categories = vec!["user/-/state/com.google/reading-list".to_owned()];
    if let Some(name) = category_name {
        categories.push(format!("user/-/label/{}", escape_to_unicode_alternative(name, false)));
    }
    let priority = i64::from(feed.priority.get());
    if priority >= i64::from(Priority::MAIN_STREAM.get()) {
        categories.push("user/-/state/org.freshrss/main".to_owned());
    }
    if priority >= i64::from(Priority::IMPORTANT.get()) {
        categories.push("user/-/state/org.freshrss/important".to_owned());
    }
    if priority <= i64::from(Priority::HIDDEN.get()) {
        categories.push("user/-/state/org.freshrss/hidden".to_owned());
    }
    if entry.is_read {
        categories.push("user/-/state/com.google/read".to_owned());
    }
    if entry.is_favorite {
        categories.push("user/-/state/com.google/starred".to_owned());
    }
    for label in label_names {
        categories.push(format!("user/-/label/{}", escape_to_unicode_alternative(label, false)));
    }
    for tag in tag_names {
        // Feed-supplied keywords carry no prefix at all, which is how Reader clients tell them
        // apart from states and labels.
        categories.push(decode_url(tag));
    }

    let mut item = json!({
        "id": item_tag_id(id),
        "crawlTimeMsec": (id / 1000).to_string(),
        "timestampUsec": id.to_string(),
        "published": entry.date,
        "title": escape_to_unicode_alternative(&entry.title, false),
        "canonical": [{ "href": link }],
        "alternate": [{ "href": link }],
        "summary": { "content": summary_content(entry, Some(feed)) },
        "categories": categories,
        "origin": {
            "streamId": format!("feed/{}", feed.id),
            "htmlUrl": decode_url(&feed.website),
            "title": escape_to_unicode_alternative(&feed.name, true),
        },
    });
    if !entry.authors.is_empty() {
        let author = entry.authors.join("; ");
        item["author"] = Value::String(escape_to_unicode_alternative(&author, false));
    }
    let enclosures: Vec<Value> = entry
        .enclosures()
        .into_iter()
        .filter(|e| !e.url.is_empty())
        .map(|e| {
            let mut enc = json!({
                "href": decode_url(&e.url),
                "type": e.mime.clone().unwrap_or_else(|| "image".to_owned()),
            });
            if let Some(length) = e.length {
                enc["length"] = json!(length);
            }
            enc
        })
        .collect();
    if !enclosures.is_empty() {
        item["enclosure"] = Value::Array(enclosures);
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_round_trip_through_both_spellings() {
        let id = 1_791_856_800_123_456i64;
        let tag = item_tag_id(id);
        assert_eq!(tag, "tag:google.com,2005:reader/item/00065db3f4e5b9c0");
        assert_eq!(parse_item_id(&tag), id);
        assert_eq!(parse_item_id(&id.to_string()), id);
        // A leading zero means hex even when every character is a digit.
        assert_eq!(parse_item_id("00065db3f4e5b9c0"), id);
        assert_eq!(parse_item_id("nonsense"), 0);
    }

    #[test]
    fn the_summary_cut_never_splits_a_character() {
        let text = "é".repeat(300_000);
        let cut = cut_at_char_boundary(&text, 500_000);
        assert!(cut.len() <= 500_000);
        assert!(cut.is_char_boundary(cut.len()));
    }
}
