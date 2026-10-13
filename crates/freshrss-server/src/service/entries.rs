//! Turning stored entries into what the reader shows.
//!
//! The listing hands back rows; the reader needs a feed name, a folder name, the article's labels and
//! a short plaintext excerpt. Each of those lives in a different table, so the naive implementation is
//! four queries per article — two hundred round trips for a page of fifty. Everything here is batched
//! instead: one query for the page's feeds, one for their folders, one for every label on every
//! article in the page.
//!
//! Also here is the cursor codec. The cursor is opaque to the client by design — its shape is an
//! implementation detail of the sort ladder — but it is not encrypted, so it must not carry anything
//! that would matter if a reader edited it. It carries only values the reader can already see.

use std::collections::HashMap;

use base64::Engine as _;
use sqlx::SqliteConnection;

use crate::api::dto::{EntryFullDto, EntrySummaryDto, FeedRefDto, LabelRefDto};
use crate::db::dao::entry_query::Cursor;
use crate::db::dao::{category as cat_dao, feed as feed_dao, tag as tag_dao};
use crate::domain::entry::Entry;
use crate::domain::feed::Feed;
use crate::error::{Error, Result};

/// How much of the body the list pane shows.
///
/// Long enough for two lines at the reader's default density, short enough that a page of fifty
/// summaries stays in the tens of kilobytes.
pub const SUMMARY_CHARS: usize = 240;

/// The lookups a page of summaries shares.
///
/// Built once per request. Every map is keyed by an id the entries already carry, so a missing entry
/// means the row is orphaned — which the foreign keys forbid, but which still has to render rather
/// than panic.
struct Lookups {
    feeds: HashMap<i64, Feed>,
    categories: HashMap<i64, String>,
    labels: HashMap<i64, Vec<LabelRefDto>>,
}

impl Lookups {
    async fn load(conn: &mut SqliteConnection, id_user: i64, entries: &[Entry]) -> Result<Self> {
        let feed_ids: Vec<i64> = unique(entries.iter().map(|e| e.id_feed));
        let entry_ids: Vec<i64> = entries.iter().map(|e| e.id.micros()).collect();

        let mut feeds = HashMap::with_capacity(feed_ids.len());
        let mut category_ids = Vec::new();
        for id in feed_ids {
            // One primary-key seek per feed rather than a single `IN` query: a page touches a handful
            // of feeds at most, and this keeps the folder ids to look up in the same pass.
            if let Some(feed) = feed_dao::find_by_id(conn, id, id_user).await? {
                category_ids.extend(feed.category);
                feeds.insert(id, feed);
            }
        }

        let mut categories = HashMap::with_capacity(category_ids.len());
        for id in unique(category_ids.into_iter()) {
            if let Some(cat) = cat_dao::find_by_id(conn, id, id_user).await? {
                categories.insert(id, cat.name);
            }
        }

        let labels = tag_dao::labels_for_entries(conn, &entry_ids)
            .await?
            .into_iter()
            .map(|(entry, tags)| {
                (
                    entry,
                    tags.into_iter()
                        .map(|t| LabelRefDto {
                            id: t.id,
                            name: t.name,
                        })
                        .collect(),
                )
            })
            .collect();

        Ok(Self {
            feeds,
            categories,
            labels,
        })
    }

    fn feed(&self, id: i64) -> Option<&Feed> {
        self.feeds.get(&id)
    }
}

/// Renders a page of entries as summaries, in the order given.
pub async fn summaries(
    conn: &mut SqliteConnection,
    id_user: i64,
    entries: &[Entry],
) -> Result<Vec<EntrySummaryDto>> {
    let lookups = Lookups::load(conn, id_user, entries).await?;
    Ok(entries.iter().map(|e| summary_of(e, &lookups)).collect())
}

/// Renders one entry with its body, for the reading pane.
pub async fn full(
    conn: &mut SqliteConnection,
    entry: &Entry,
) -> Result<(EntryFullDto, FeedRefDto)> {
    let lookups = Lookups::load(conn, entry.id_user, std::slice::from_ref(entry)).await?;
    let summary = summary_of(entry, &lookups);
    let feed = feed_ref(entry.id_feed, lookups.feed(entry.id_feed));
    Ok((
        EntryFullDto {
            summary,
            content_html: entry.content.clone(),
            content_text: strip_tags(&entry.content),
            full_content: entry
                .attributes
                .get_str(crate::domain::attributes::keys::FULL_CONTENT)
                .map(str::to_owned),
            guid: entry.guid.clone(),
            enclosures: entry.enclosures(),
        },
        feed,
    ))
}

fn summary_of(entry: &Entry, lookups: &Lookups) -> EntrySummaryDto {
    let feed = lookups.feed(entry.id_feed);
    let playable = entry.playable_enclosure();
    EntrySummaryDto {
        id: entry.id,
        feed_id: entry.id_feed,
        feed_name: feed
            .map(|f| f.display_name().to_owned())
            .unwrap_or_default(),
        category_id: feed.and_then(|f| f.category),
        category_name: feed
            .and_then(|f| f.category)
            .and_then(|c| lookups.categories.get(&c).cloned()),
        title: entry.display_title(),
        link: entry.link.clone(),
        author: entry.authors.join("; "),
        summary: entry.summary(SUMMARY_CHARS),
        date: entry.date,
        last_modified: entry.last_modified,
        last_seen: entry.last_seen,
        is_read: entry.is_read,
        is_favorite: entry.is_favorite,
        tags: entry.tags.clone(),
        labels: lookups
            .labels
            .get(&entry.id.micros())
            .cloned()
            .unwrap_or_default(),
        thumbnail: entry.thumbnail().map(|t| t.url),
        has_enclosures: entry.has_enclosures(),
        has_media: playable.is_some_and(|e| e.is_audio() || e.is_video()),
        // A refresh that found the body had changed upstream sets `lastModified` to its own mtime, so
        // a value above the publication date means "this article was edited after we first saw it".
        is_updated: entry.last_modified.is_some_and(|m| m > entry.date),
    }
}

fn feed_ref(id: i64, feed: Option<&Feed>) -> FeedRefDto {
    FeedRefDto {
        id,
        name: feed
            .map(|f| f.display_name().to_owned())
            .unwrap_or_default(),
        website: feed.map(|f| f.website.clone()).unwrap_or_default(),
    }
}

/// The feed a single entry belongs to, for the reading pane's header.
pub async fn feed_of(conn: &mut SqliteConnection, entry: &Entry) -> Result<FeedRefDto> {
    let feed = feed_dao::find_by_id(conn, entry.id_feed, entry.id_user).await?;
    Ok(feed_ref(entry.id_feed, feed.as_ref()))
}

fn strip_tags(html: &str) -> String {
    freshrss_feed::text::plain_text(html, usize::MAX)
}

/// Order-preserving de-duplication, so a page of fifty articles from three feeds makes three lookups.
fn unique<I: Iterator<Item = i64>>(iter: I) -> Vec<i64> {
    let mut seen = Vec::new();
    for v in iter {
        if !seen.contains(&v) {
            seen.push(v);
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// The cursor codec
// ---------------------------------------------------------------------------

/// Base64url without padding: the cursor goes into a query string, so `+` and `/` would have to be
/// escaped and a trailing `=` reads like a typo.
const CURSOR_ENGINE: base64::engine::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Encodes a cursor for the wire.
pub fn encode_cursor(cursor: &Cursor) -> Result<String> {
    let json = serde_json::to_vec(cursor)?;
    Ok(CURSOR_ENGINE.encode(json))
}

/// Decodes a cursor the client sent back.
///
/// A cursor that does not decode is a `400`, not a `500`: the client either truncated it, edited it,
/// or is holding one from a build whose ladder was shaped differently. Starting again from the top of
/// the list would be worse than saying so, because the reader would see articles they had already read
/// and have no idea why.
pub fn decode_cursor(text: &str) -> Result<Cursor> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(Error::Validation("a cursor cannot be empty".to_owned()));
    }
    let bytes = CURSOR_ENGINE
        .decode(trimmed)
        .map_err(|_| Error::Validation("the cursor is not valid base64url".to_owned()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Validation("the cursor does not describe a page boundary".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dao::entry_query::CursorValue;
    use crate::domain::ids::EntryId;

    fn cursor() -> Cursor {
        Cursor {
            id: EntryId::from_micros(1_700_000_000_123_456),
            values: vec![CursorValue::Text("News".into()), CursorValue::Int(42)],
        }
    }

    #[test]
    fn a_cursor_round_trips() {
        let c = cursor();
        let encoded = encode_cursor(&c).expect("encodes");
        assert_eq!(decode_cursor(&encoded).expect("decodes"), c);
    }

    #[test]
    fn the_wire_form_is_url_safe_and_unpadded() {
        // It goes into a query string, so `+`, `/` and `=` would all have to be escaped.
        let encoded = encode_cursor(&cursor()).expect("encodes");
        assert!(
            !encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='),
            "{encoded}"
        );
        // A cursor with no values is the id sort's, and it is the common case.
        let bare = encode_cursor(&Cursor {
            id: EntryId::from_micros(1),
            values: Vec::new(),
        })
        .expect("encodes");
        assert!(!bare.contains('='), "{bare}");
    }

    #[test]
    fn an_entry_id_survives_the_cursor_at_full_precision() {
        // Entry ids are microseconds since the epoch, well past 2^53, so a JSON number would be rounded
        // by any JavaScript client. `EntryId` serializes as a string.
        let c = cursor();
        let encoded = encode_cursor(&c).expect("encodes");
        let decoded = decode_cursor(&encoded).expect("decodes");
        assert_eq!(decoded.id.micros(), 1_700_000_000_123_456);
    }

    #[test]
    fn a_tampered_cursor_is_a_client_error_not_a_crash() {
        for bad in [
            "",
            "   ",
            "not base64 at all!!!",
            // Valid base64url, but not a cursor.
            &CURSOR_ENGINE.encode(b"hello"),
            // A cursor with its values chopped off.
            &CURSOR_ENGINE.encode(br#"{"id":"1","values":[{"x":1}]}"#),
        ] {
            let e = decode_cursor(bad).expect_err("must be rejected");
            assert!(matches!(e, Error::Validation(_)), "{bad:?}: {e}");
            assert!(e.is_client_error(), "{bad:?} should be a 400: {e}");
        }
    }

    #[test]
    fn a_cursor_from_an_older_build_still_decodes() {
        // The values are `#[serde(untagged)]`, so a plain JSON number or string both land.
        let json = br#"{"id":"1700000000123456","values":["News",42]}"#;
        let decoded = decode_cursor(&CURSOR_ENGINE.encode(json)).expect("decodes");
        assert_eq!(decoded, cursor());
    }

    #[test]
    fn unique_preserves_first_seen_order() {
        // `Vec::dedup` only collapses neighbours, and a page interleaves feeds.
        assert_eq!(unique([3, 1, 3, 2, 1].into_iter()), vec![3, 1, 2]);
        assert!(unique(std::iter::empty()).is_empty());
    }
}
