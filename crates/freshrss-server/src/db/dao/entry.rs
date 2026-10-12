//! `entry` and `entrytmp` access.
//!
//! This module holds the write side of the refresh pipeline (stage → allocate ids → commit →
//! reconcile) and the point mutations the reader makes (read, star). The read side — scopes, state,
//! search and keyset pagination — is a separate builder module, because it is the most intricate
//! SQL in the crate and deserves to be tested as a pure function.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::db::dao::meta;
use crate::db::dao::row_attributes;
use crate::db::sql::{in_clause, MAX_BIND_PARAMS};
use crate::domain::entry::Entry;
use crate::domain::ids::{now_unix, EntryId};

pub const COLUMNS: &str = "id, id_user, id_feed, guid, title, author, content, link, date, \
     lastSeen, lastModified, lastUserModified, hash, is_read, is_favorite, tags, attributes";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Entry> {
    let id_raw: i64 = row.get(0)?;
    let hash_blob: Option<Vec<u8>> = row.get(12)?;
    Ok(Entry {
        id: EntryId::from_micros(id_raw),
        id_user: row.get(1)?,
        id_feed: row.get(2)?,
        guid: row.get(3)?,
        title: row.get(4)?,
        authors: Entry::authors_from_db(&row.get::<_, String>(5)?),
        content: row.get(6)?,
        link: row.get(7)?,
        date: row.get(8)?,
        last_seen: row.get(9)?,
        last_modified: row.get(10)?,
        last_user_modified: row.get(11)?,
        // FreshRSS's SQLite DAO stores the raw 16-byte MD5 and hex-encodes on read; we keep it raw.
        hash: hash_blob.and_then(|b| <[u8; 16]>::try_from(b).ok()),
        is_read: row.get::<_, i64>(13)? != 0,
        is_favorite: row.get::<_, i64>(14)? != 0,
        tags: Entry::tags_from_db(&row.get::<_, String>(15)?),
        attributes: row_attributes(row, 16)?,
    })
}

// --- reads -----------------------------------------------------------------

pub fn find_by_id(conn: &Connection, id: EntryId, id_user: i64) -> SqlResult<Option<Entry>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM entry WHERE id = ?1 AND id_user = ?2"),
        params![id.micros(), id_user],
        from_row,
    )
    .optional()
}

pub fn find_by_guid(conn: &Connection, id_feed: i64, guid: &str) -> SqlResult<Option<Entry>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM entry WHERE id_feed = ?1 AND guid = ?2"),
        params![id_feed, guid],
        from_row,
    )
    .optional()
}

pub fn list_by_ids(conn: &Connection, id_user: i64, ids: &[EntryId]) -> SqlResult<Vec<Entry>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_BIND_PARAMS) {
        let placeholders = in_clause(chunk.len());
        let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        values.push(&id_user);
        let micros: Vec<i64> = chunk.iter().map(|i| i.micros()).collect();
        for m in &micros {
            values.push(m as &dyn rusqlite::ToSql);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM entry WHERE id_user = ?1 AND id IN {placeholders} ORDER BY id"
        ))?;
        let rows = stmt.query_map(values.as_slice(), from_row)?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// `guid → hash` for the guids a refresh just saw, in one query.
///
/// This is the successor to FreshRSS's `findExistingGuids`: a single lookup that answers both
/// "have I seen this article?" and "has its content changed?", which is what the reconcile loop
/// needs to decide between insert, update and ignore.
pub fn list_hashes_for_guids(
    conn: &Connection,
    id_feed: i64,
    guids: &[String],
) -> SqlResult<HashMap<String, [u8; 16]>> {
    let mut out = HashMap::with_capacity(guids.len());
    for chunk in guids.chunks(MAX_BIND_PARAMS - 1) {
        let placeholders = in_clause(chunk.len());
        let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        values.push(&id_feed);
        for g in chunk {
            values.push(g as &dyn rusqlite::ToSql);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT guid, hash FROM entry WHERE id_feed = ?1 AND guid IN {placeholders}"
        ))?;
        let rows = stmt.query_map(values.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })?;
        for row in rows {
            let (guid, hash) = row?;
            // A NULL or wrong-length hash is treated as "unknown", so the reconcile loop will
            // re-write the article rather than deciding it is unchanged.
            let digest = hash.and_then(|bytes| <[u8; 16]>::try_from(bytes).ok());
            if let Some(digest) = digest {
                out.insert(guid, digest);
            }
        }
    }
    Ok(out)
}

/// The most recent N titles in a feed, for `read_when_same_title_in_feed`.
pub fn list_recent_titles(conn: &Connection, id_feed: i64, limit: usize) -> SqlResult<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT title FROM entry WHERE id_feed = ?1 ORDER BY id DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![id_feed, limit as i64], |r| r.get(0))?;
    rows.collect()
}

/// The most recent N titles across a folder's feeds, for `read_when_same_title_in_category`.
pub fn list_recent_titles_in_category(
    conn: &Connection,
    id_user: i64,
    id_category: i64,
    limit: usize,
) -> SqlResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT e.title FROM entry e
         INNER JOIN feed f ON f.id = e.id_feed
         WHERE e.id_user = ?1 AND f.category = ?2
         ORDER BY e.id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![id_user, id_category, limit as i64], |r| r.get(0))?;
    rows.collect()
}

/// The most recent N guids across a folder's feeds, for `read_when_same_guid_in_category`.
pub fn list_recent_guids_in_category(
    conn: &Connection,
    id_user: i64,
    id_category: i64,
    limit: usize,
) -> SqlResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT e.guid FROM entry e
         INNER JOIN feed f ON f.id = e.id_feed
         WHERE e.id_user = ?1 AND f.category = ?2
         ORDER BY e.id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![id_user, id_category, limit as i64], |r| r.get(0))?;
    rows.collect()
}

// --- staging and commit ----------------------------------------------------

/// Adds a new article to the staging table.
///
/// `INSERT OR IGNORE` because the same guid can appear twice in one refresh (a feed repeating an
/// item, or two feeds in one batch) and `UNIQUE(id_feed, guid)` makes the second a no-op rather
/// than an error.
pub fn stage(conn: &Connection, entry: &Entry) -> SqlResult<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO entrytmp
             (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
              is_read, is_favorite, id_feed, tags, attributes)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            entry.id.micros(),
            entry.id_user,
            entry.guid,
            entry.title,
            entry.author_to_db(),
            entry.content,
            entry.link,
            entry.date,
            entry.last_seen,
            entry.hash.map(|h| h.to_vec()),
            i64::from(entry.is_read),
            i64::from(entry.is_favorite),
            entry.id_feed,
            entry.tags_to_db(),
            entry.attributes.to_db(),
        ],
    )?;
    Ok(inserted > 0)
}

pub fn count_staged(conn: &Connection, id_user: i64) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM entrytmp WHERE id_user = ?1",
        params![id_user],
        |r| r.get(0),
    )
}

/// Moves a user's staged articles into `entry` with ids from the global allocator.
///
/// Ids are assigned in `(date, staged id)` order so they stay chronological within the batch —
/// FreshRSS does the same, and it matters because `ORDER BY id` is the default listing order and a
/// feed that publishes out of order would otherwise show articles shuffled.
///
/// Must be called inside [`crate::db::Db::with_writer`]: it reads and writes the id sequence, and
/// two concurrent callers would hand out the same range.
pub fn commit_staged(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    let staged = count_staged(conn, id_user)?;
    if staged == 0 {
        return Ok(0);
    }
    let first_id = meta::reserve_entry_ids(conn, staged as usize)?;

    // `INSERT OR IGNORE` is the last line of defence against a duplicate `(id_feed, guid)` that
    // slipped past staging — for example an article that arrived via a WebSub push while a pull
    // refresh was in flight.
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO entry
             (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
              is_read, is_favorite, id_feed, tags, attributes)
         SELECT ?2 + (ROW_NUMBER() OVER (ORDER BY t.date, t.id) - 1),
                t.id_user, t.guid, t.title, t.author, t.content, t.link, t.date, t.lastSeen,
                t.hash, t.is_read, t.is_favorite, t.id_feed, t.tags, t.attributes
         FROM entrytmp t
         WHERE t.id_user = ?1",
        params![id_user, first_id],
    )?;

    conn.execute("DELETE FROM entrytmp WHERE id_user = ?1", params![id_user])?;
    Ok(inserted)
}

/// Discards a user's staged rows, for a refresh that aborted before committing.
pub fn discard_staged(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    conn.execute("DELETE FROM entrytmp WHERE id_user = ?1", params![id_user])
}

// --- reconciliation --------------------------------------------------------

/// Updates an article whose content changed upstream.
///
/// Keyed on `(id_feed, guid)`, never on `id`: the id must not change, or the user's read and
/// starred state would be orphaned. `is_read` and `is_favorite` are only written when the caller
/// supplied a value, so an upstream edit does not silently clear either — FreshRSS uses
/// `COALESCE(:x, x)` for exactly this reason.
pub fn update_existing(conn: &Connection, entry: &Entry) -> SqlResult<bool> {
    let updated = conn.execute(
        "UPDATE entry SET
             title = ?3,
             author = ?4,
             content = ?5,
             link = ?6,
             date = ?7,
             lastSeen = ?8,
             lastModified = COALESCE(?9, lastModified),
             lastUserModified = COALESCE(?10, lastUserModified),
             hash = ?11,
             is_read = COALESCE(?12, is_read),
             is_favorite = COALESCE(?13, is_favorite),
             tags = ?14,
             attributes = ?15
         WHERE id_feed = ?1 AND guid = ?2",
        params![
            entry.id_feed,
            entry.guid,
            entry.title,
            entry.author_to_db(),
            entry.content,
            entry.link,
            entry.date,
            entry.last_seen,
            entry.last_modified,
            entry.last_user_modified,
            entry.hash.map(|h| h.to_vec()),
            entry.is_read.then_some(1i64),
            entry.is_favorite.then_some(1i64),
            entry.tags_to_db(),
            entry.attributes.to_db(),
        ],
    )?;
    Ok(updated > 0)
}

/// Stamps `lastSeen` on every guid a refresh saw upstream.
///
/// Retention keys off this column: an article that stops appearing keeps its old `lastSeen` and
/// eventually ages out, while one that is still published keeps sliding forward. Articles are
/// never deleted just because they vanished from the feed.
pub fn update_last_seen(
    conn: &Connection,
    id_feed: i64,
    guids: &[String],
    mtime: i64,
) -> SqlResult<usize> {
    let mut total = 0;
    for chunk in guids.chunks(MAX_BIND_PARAMS - 2) {
        let placeholders = in_clause(chunk.len());
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&mtime, &id_feed];
        for g in chunk {
            values.push(g as &dyn rusqlite::ToSql);
        }
        total += conn.execute(
            &format!(
                "UPDATE entry SET lastSeen = ?1 WHERE id_feed = ?2 AND guid IN {placeholders}"
            ),
            values.as_slice(),
        )?;
    }
    Ok(total)
}

/// Stamps `lastSeen` on a 304 / unchanged feed.
///
/// Nothing was re-parsed, so there is no guid list to work from; instead every article already
/// seen at the previous refresh is advanced. Without this, a feed that rarely changes would have
/// its whole backlog age out on the retention schedule even though it is perfectly healthy.
pub fn update_last_seen_unchanged(conn: &Connection, id_feed: i64, mtime: i64) -> SqlResult<usize> {
    conn.execute(
        "UPDATE entry SET lastSeen = ?2
         WHERE id_feed = ?1
           AND lastSeen >= (SELECT lastUpdate FROM feed f WHERE f.id = ?1)",
        params![id_feed, mtime],
    )
}

// --- point mutations -------------------------------------------------------

/// Marks articles read or unread. Returns how many actually changed.
///
/// Only rows whose flag differs are touched, so `lastUserModified` is not churned on an article
/// that was already read — the column means "when the user last acted", not "when the last request
/// mentioned it".
pub fn mark_read(conn: &Connection, id_user: i64, ids: &[EntryId], read: bool) -> SqlResult<usize> {
    let flag = i64::from(read);
    let now = now_unix();
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS - 3) {
        let placeholders = in_clause(chunk.len());
        let micros: Vec<i64> = chunk.iter().map(|i| i.micros()).collect();
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&flag, &now, &id_user];
        for m in &micros {
            values.push(m as &dyn rusqlite::ToSql);
        }
        total += conn.execute(
            &format!(
                "UPDATE entry SET is_read = ?1, lastUserModified = ?2
                 WHERE id_user = ?3 AND is_read <> ?1 AND id IN {placeholders}"
            ),
            values.as_slice(),
        )?;
    }
    Ok(total)
}

pub fn mark_favorite(
    conn: &Connection,
    id_user: i64,
    ids: &[EntryId],
    favorite: bool,
) -> SqlResult<usize> {
    let flag = i64::from(favorite);
    let now = now_unix();
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS - 3) {
        let placeholders = in_clause(chunk.len());
        let micros: Vec<i64> = chunk.iter().map(|i| i.micros()).collect();
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&flag, &now, &id_user];
        for m in &micros {
            values.push(m as &dyn rusqlite::ToSql);
        }
        total += conn.execute(
            &format!(
                "UPDATE entry SET is_favorite = ?1, lastUserModified = ?2
                 WHERE id_user = ?3 AND is_favorite <> ?1 AND id IN {placeholders}"
            ),
            values.as_slice(),
        )?;
    }
    Ok(total)
}

/// Marks every article in one feed read, up to `id_max`.
///
/// The `id <= ?` bound is what makes "mark all as read" mean "everything I had loaded", not
/// "everything including the articles that arrived while I was reading".
pub fn mark_read_feed(
    conn: &Connection,
    id_feed: i64,
    id_max: EntryId,
    now: i64,
) -> SqlResult<usize> {
    conn.execute(
        "UPDATE entry SET is_read = 1, lastUserModified = ?3
         WHERE id_feed = ?1 AND is_read = 0 AND id <= ?2",
        params![id_feed, id_max.micros(), now],
    )
}

pub fn mark_read_category(
    conn: &Connection,
    id_user: i64,
    id_category: i64,
    id_max: EntryId,
    now: i64,
) -> SqlResult<usize> {
    conn.execute(
        "UPDATE entry SET is_read = 1, lastUserModified = ?4
         WHERE id_user = ?1 AND is_read = 0 AND id <= ?3
           AND id_feed IN (SELECT f.id FROM feed f WHERE f.category = ?2)",
        params![id_user, id_category, id_max.micros(), now],
    )
}

pub fn mark_read_all(
    conn: &Connection,
    id_user: i64,
    id_max: EntryId,
    now: i64,
) -> SqlResult<usize> {
    conn.execute(
        "UPDATE entry SET is_read = 1, lastUserModified = ?3
         WHERE id_user = ?1 AND is_read = 0 AND id <= ?2",
        params![id_user, id_max.micros(), now],
    )
}

// --- aggregates ------------------------------------------------------------

/// Total, unread and starred counts for a user in one pass.
pub fn count_states(conn: &Connection, id_user: i64) -> SqlResult<CountStates> {
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN is_read = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_favorite = 1 THEN 1 ELSE 0 END), 0)
         FROM entry WHERE id_user = ?1",
        params![id_user],
        |row| {
            Ok(CountStates {
                total: row.get(0)?,
                unread: row.get(1)?,
                starred: row.get(2)?,
            })
        },
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CountStates {
    pub total: i64,
    pub unread: i64,
    pub starred: i64,
}

impl CountStates {
    pub fn read(&self) -> i64 {
        self.total - self.unread
    }
}

/// The newest article id for a user, used as the default `id_max` snapshot fence.
pub fn max_id(conn: &Connection, id_user: i64) -> SqlResult<Option<EntryId>> {
    let raw: Option<i64> = conn
        .query_row(
            "SELECT MAX(id) FROM entry WHERE id_user = ?1",
            params![id_user],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw.map(EntryId::from_micros))
}

/// The publication time of the newest article, for the "last updated" display.
pub fn newest_article_date(conn: &Connection, id_user: i64) -> SqlResult<Option<i64>> {
    conn.query_row(
        "SELECT MAX(date) FROM entry WHERE id_user = ?1",
        params![id_user],
        |r| r.get(0),
    )
    .optional()
}

/// Deletes every article a user owns. Used by account deletion and by tests.
pub fn delete_all_for_user(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    conn.execute("DELETE FROM entry WHERE id_user = ?1", params![id_user])
}
