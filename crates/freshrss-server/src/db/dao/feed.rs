//! `feed` table access, including the due-feed selection query that drives the refresh scheduler.

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::db::dao::row_attributes;
use crate::db::sql::{in_clause, MAX_BIND_PARAMS};
use crate::domain::attributes::Attributes;
use crate::domain::feed::Feed;
use crate::domain::kind::FeedKind;
use crate::domain::priority::Priority;
use crate::domain::ttl::Ttl;

pub const COLUMNS: &str = "id, id_user, url, kind, category, name, website, description, \
     lastUpdate, priority, pathEntries, pathEntriesFilter, httpAuth, error, ttl, attributes, \
     cache_nbEntries, cache_nbUnreads";

/// The same list qualified for the join in [`list_due`].
const COLUMNS_ALIASED: &str = "f.id, f.id_user, f.url, f.kind, f.category, f.name, f.website, \
     f.description, f.lastUpdate, f.priority, f.pathEntries, f.pathEntriesFilter, f.httpAuth, \
     f.error, f.ttl, f.attributes, f.cache_nbEntries, f.cache_nbUnreads";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Feed> {
    let kind_raw: i64 = row.get(3)?;
    let priority_raw: i64 = row.get(9)?;
    let ttl_raw: i64 = row.get(14)?;
    Ok(Feed {
        id: row.get(0)?,
        id_user: row.get(1)?,
        url: row.get(2)?,
        kind: FeedKind::from_i16(kind_raw as i16).unwrap_or(FeedKind::Rss),
        category: row.get(4)?,
        name: row.get(5)?,
        website: row.get(6)?,
        description: row.get(7)?,
        last_update: row.get(8)?,
        priority: Priority::new(priority_raw as i8),
        path_entries: row.get(10)?,
        path_entries_filter: row.get(11)?,
        http_auth: row.get(12)?,
        error: row.get(13)?,
        ttl: Ttl::from_raw(ttl_raw as i32),
        attributes: row_attributes(row, 15)?,
        cache_nb_entries: row.get(16)?,
        cache_nb_unreads: row.get(17)?,
    })
}

pub fn insert(conn: &Connection, feed: &Feed) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO feed (id_user, url, kind, category, name, website, description, lastUpdate,
                            priority, pathEntries, pathEntriesFilter, httpAuth, error, ttl,
                            attributes, cache_nbEntries, cache_nbUnreads)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            feed.id_user,
            feed.url,
            i64::from(feed.kind.as_i16()),
            feed.category,
            feed.name,
            feed.website,
            feed.description,
            feed.last_update,
            i64::from(feed.priority.get()),
            feed.path_entries,
            feed.path_entries_filter,
            feed.http_auth,
            feed.error,
            feed.ttl.raw(),
            feed.attributes.to_db(),
            feed.cache_nb_entries,
            feed.cache_nb_unreads,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn find_by_id(conn: &Connection, id: i64, id_user: i64) -> SqlResult<Option<Feed>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM feed WHERE id = ?1 AND id_user = ?2"),
        params![id, id_user],
        from_row,
    )
    .optional()
}

/// Duplicate check before subscribing. Scoped by user, so two people may follow the same feed.
pub fn find_by_url(conn: &Connection, id_user: i64, url: &str) -> SqlResult<Option<Feed>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM feed WHERE id_user = ?1 AND url = ?2 LIMIT 1"),
        params![id_user, url],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection, id_user: i64) -> SqlResult<Vec<Feed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM feed WHERE id_user = ?1 ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_user], from_row)?;
    rows.collect()
}

pub fn list_by_category(conn: &Connection, id_user: i64, category: i64) -> SqlResult<Vec<Feed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM feed WHERE id_user = ?1 AND category = ?2
         ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_user, category], from_row)?;
    rows.collect()
}

pub fn count(conn: &Connection, id_user: i64) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM feed WHERE id_user = ?1",
        params![id_user],
        |r| r.get(0),
    )
}

/// Feeds whose refresh is due, oldest first.
///
/// Ports `FreshRSS_FeedDAO::listFeedsOrderUpdate`, with one change forced by the shared database:
/// `ttl = 0` means "use the owner's default", and that default lives in *that user's* config, so
/// the query has to join `user` and read it per row. FreshRSS takes the value as a scalar because
/// its database only ever has one user in it; passing a global 3600 here would silently ignore
/// per-user configuration.
///
/// `ttl >= 0` is the mute filter — a negative ttl means muted, not "very often".
///
/// The `CASE WHEN error > lastUpdate` term means a feed that keeps failing is retried on its own
/// error timestamp rather than being hammered every tick.
pub fn list_due(conn: &Connection, now: i64, limit: usize) -> SqlResult<Vec<Feed>> {
    // `json_extract` reads `ttl_default` out of the user's config blob. It stays there rather than
    // being promoted to a column so there is exactly one copy of the value; the query runs once per
    // scheduler tick over an indexed candidate set, so the extraction is not on a hot path.
    let sql = format!(
        "SELECT {COLUMNS_ALIASED} FROM feed f
         INNER JOIN user u ON u.id = f.id_user
         WHERE f.ttl >= 0 AND u.enabled = 1
           AND (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END)
               < (?1 + 60 - COALESCE(
                     NULLIF(f.ttl, 0),
                     CAST(json_extract(u.config, '$.ttl_default') AS INTEGER),
                     3600))
         ORDER BY (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END) ASC
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now, limit as i64], from_row)?;
    rows.collect()
}

/// Feeds belonging to one user whose refresh is due. Same selection as [`list_due`], scoped.
pub fn list_due_for_user(
    conn: &Connection,
    id_user: i64,
    now: i64,
    limit: usize,
) -> SqlResult<Vec<Feed>> {
    let sql = format!(
        "SELECT {COLUMNS_ALIASED} FROM feed f
         INNER JOIN user u ON u.id = f.id_user
         WHERE f.id_user = ?3 AND f.ttl >= 0
           AND (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END)
               < (?1 + 60 - COALESCE(
                     NULLIF(f.ttl, 0),
                     CAST(json_extract(u.config, '$.ttl_default') AS INTEGER),
                     3600))
         ORDER BY (CASE WHEN f.error > f.lastUpdate THEN f.error ELSE f.lastUpdate END) ASC
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now, limit as i64, id_user], from_row)?;
    rows.collect()
}

/// Feeds with a recorded failure or muted, for the reader's "Feeds with problems" dialog.
pub fn list_with_problems(conn: &Connection, id_user: i64) -> SqlResult<Vec<Feed>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM feed
         WHERE id_user = ?1 AND (error > 0 OR ttl < 0)
         ORDER BY error DESC, name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_user], from_row)?;
    rows.collect()
}

// --- updates ---------------------------------------------------------------

pub fn update_url(conn: &Connection, id: i64, id_user: i64, url: &str) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET url = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, url],
    )? > 0)
}

pub fn update_name(conn: &Connection, id: i64, id_user: i64, name: &str) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET name = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, name],
    )? > 0)
}

pub fn update_category(
    conn: &Connection,
    id: i64,
    id_user: i64,
    category: Option<i64>,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET category = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, category],
    )? > 0)
}

pub fn update_priority(
    conn: &Connection,
    id: i64,
    id_user: i64,
    priority: Priority,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET priority = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, i64::from(priority.get())],
    )? > 0)
}

pub fn update_ttl(conn: &Connection, id: i64, id_user: i64, ttl: Ttl) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET ttl = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, ttl.raw()],
    )? > 0)
}

pub fn update_kind(conn: &Connection, id: i64, id_user: i64, kind: FeedKind) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET kind = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, i64::from(kind.as_i16())],
    )? > 0)
}

pub fn update_scraping(
    conn: &Connection,
    id: i64,
    id_user: i64,
    path_entries: Option<&str>,
    path_entries_filter: Option<&str>,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET pathEntries = ?3, pathEntriesFilter = ?4 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, path_entries, path_entries_filter],
    )? > 0)
}

pub fn update_http_auth(
    conn: &Connection,
    id: i64,
    id_user: i64,
    http_auth: Option<&str>,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET httpAuth = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, http_auth],
    )? > 0)
}

pub fn update_attributes(
    conn: &Connection,
    id: i64,
    id_user: i64,
    attributes: &Attributes,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET attributes = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, attributes.to_db()],
    )? > 0)
}

pub fn update_website_description(
    conn: &Connection,
    id: i64,
    id_user: i64,
    website: &str,
    description: &str,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET website = ?3, description = ?4 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, website, description],
    )? > 0)
}

/// Records a successful refresh. Clears `error` unconditionally — FreshRSS does the same, so a
/// feed that recovers stops showing in the problems dialog without a separate step.
pub fn update_last_update(conn: &Connection, id: i64, mtime: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET lastUpdate = ?2, error = 0 WHERE id = ?1",
        params![id, mtime],
    )? > 0)
}

/// Records a failed refresh as a timestamp.
pub fn update_last_error(conn: &Connection, id: i64, error: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET error = ?2 WHERE id = ?1",
        params![id, error],
    )? > 0)
}

/// Recomputes the denormalized entry counters.
///
/// FreshRSS keeps these because its sidebar is rendered server-side and cannot afford a count per
/// feed; we keep them for the same reason — `GET /tree` builds the whole sidebar from one query.
/// The correlated subqueries replace the `UPDATE … LEFT JOIN` form, which SQLite does not support.
pub fn update_cached_values(conn: &Connection, ids: &[i64]) -> SqlResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut total = 0;
    for chunk in ids.chunks(MAX_BIND_PARAMS) {
        let placeholders = in_clause(chunk.len());
        let values: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        total += conn.execute(
            &format!(
                "UPDATE feed SET
                     cache_nbEntries = (SELECT COUNT(*) FROM entry e WHERE e.id_feed = feed.id),
                     cache_nbUnreads = (SELECT COUNT(*) FROM entry e
                                        WHERE e.id_feed = feed.id AND e.is_read = 0)
                 WHERE id IN {placeholders}"
            ),
            values.as_slice(),
        )?;
    }
    Ok(total)
}

/// Recomputes the counters for every feed a user owns.
pub fn update_all_cached_values(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    conn.execute(
        "UPDATE feed SET
             cache_nbEntries = (SELECT COUNT(*) FROM entry e WHERE e.id_feed = feed.id),
             cache_nbUnreads = (SELECT COUNT(*) FROM entry e
                                WHERE e.id_feed = feed.id AND e.is_read = 0)
         WHERE id_user = ?1",
        params![id_user],
    )
}

/// Adjusts one feed's unread counter by a delta, clamped at zero.
///
/// Used by the single-entry read/star paths, where recomputing from `entry` would be a full scan
/// for a change of one.
pub fn bump_cache_unreads(conn: &Connection, id_feed: i64, delta: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE feed SET cache_nbUnreads = MAX(0, cache_nbUnreads + ?2) WHERE id = ?1",
        params![id_feed, delta],
    )? > 0)
}

/// Deletes a feed and, via `ON DELETE CASCADE`, its entries and their label links.
pub fn delete(conn: &Connection, id: i64, id_user: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "DELETE FROM feed WHERE id = ?1 AND id_user = ?2",
        params![id, id_user],
    )? > 0)
}

/// Empties a feed's articles but keeps the subscription. Resets `lastUpdate` so the next refresh
/// treats the feed as new and re-fetches everything, matching FreshRSS's `truncate`.
pub fn truncate_entries(conn: &Connection, id: i64, id_user: i64) -> SqlResult<usize> {
    let tx = conn.unchecked_transaction()?;
    let deleted = tx.execute(
        "DELETE FROM entry WHERE id_feed = ?1 AND id_user = ?2",
        params![id, id_user],
    )?;
    tx.execute(
        "UPDATE feed SET cache_nbEntries = 0, cache_nbUnreads = 0, lastUpdate = 0
         WHERE id = ?1 AND id_user = ?2",
        params![id, id_user],
    )?;
    tx.commit()?;
    Ok(deleted)
}
