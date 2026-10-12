//! `category` table access — the reader's folders.
//!
//! Every query is scoped by `id_user`. That is not defensive redundancy: `category.name` is only
//! unique per user, so an unscoped lookup by name would return another user's folder.

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::db::dao::row_attributes;
use crate::domain::attributes::Attributes;
use crate::domain::category::{Category, CategoryKind};

pub const COLUMNS: &str = "id, id_user, name, kind, lastUpdate, error, position, attributes";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Category> {
    let kind_raw: i64 = row.get(3)?;
    Ok(Category {
        id: row.get(0)?,
        id_user: row.get(1)?,
        name: row.get(2)?,
        // An unrecognised kind degrades to a normal folder rather than failing the listing; the
        // value is preserved on the next write because `update_*` never touches `kind`.
        kind: CategoryKind::from_i16(kind_raw as i16).unwrap_or(CategoryKind::Normal),
        last_update: row.get(4)?,
        error: row.get(5)?,
        position: row.get(6)?,
        attributes: row_attributes(row, 7)?,
    })
}

pub struct NewCategory<'a> {
    pub id_user: i64,
    pub name: &'a str,
    pub kind: CategoryKind,
    pub position: i64,
    pub attributes: Attributes,
}

pub fn insert(conn: &Connection, new: &NewCategory<'_>) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO category (id_user, name, kind, position, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            new.id_user,
            new.name,
            i64::from(new.kind.as_i16()),
            new.position,
            new.attributes.to_db(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn find_by_id(conn: &Connection, id: i64, id_user: i64) -> SqlResult<Option<Category>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM category WHERE id = ?1 AND id_user = ?2"),
        params![id, id_user],
        from_row,
    )
    .optional()
}

/// Looks a folder up by name, for "add this feed to the folder called X" during OPML import.
pub fn find_by_name(conn: &Connection, id_user: i64, name: &str) -> SqlResult<Option<Category>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM category WHERE id_user = ?1 AND name = ?2"),
        params![id_user, name],
        from_row,
    )
    .optional()
}

/// All folders for a user, in sidebar order.
pub fn list(conn: &Connection, id_user: i64) -> SqlResult<Vec<Category>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM category WHERE id_user = ?1 ORDER BY position, name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_user], from_row)?;
    rows.collect()
}

pub fn count(conn: &Connection, id_user: i64) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM category WHERE id_user = ?1",
        params![id_user],
        |r| r.get(0),
    )
}

/// The next free sidebar position.
pub fn next_position(conn: &Connection, id_user: i64) -> SqlResult<i64> {
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(position) FROM category WHERE id_user = ?1",
            params![id_user],
            |r| r.get(0),
        )
        .optional()?;
    Ok(max.unwrap_or(-1) + 1)
}

pub fn update_name(conn: &Connection, id: i64, id_user: i64, name: &str) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE category SET name = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, name],
    )? > 0)
}

pub fn update_position(conn: &Connection, id: i64, id_user: i64, position: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE category SET position = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, position],
    )? > 0)
}

pub fn update_attributes(
    conn: &Connection,
    id: i64,
    id_user: i64,
    attributes: &Attributes,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE category SET attributes = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, attributes.to_db()],
    )? > 0)
}

/// Records a refresh failure. `error` is a timestamp, not a boolean, so the sidebar can say how
/// stale the problem is.
pub fn update_error(conn: &Connection, id: i64, id_user: i64, error: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE category SET error = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, error],
    )? > 0)
}

pub fn update_last_update(conn: &Connection, id: i64, id_user: i64, mtime: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE category SET lastUpdate = ?3, error = 0 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, mtime],
    )? > 0)
}

/// Deletes a folder. Feeds inside it are **not** deleted — `feed.category` is
/// `ON DELETE SET NULL`, so they become orphans, which the reader lists separately. That matches
/// FreshRSS and avoids a folder rename going wrong destroying someone's subscriptions.
pub fn delete(conn: &Connection, id: i64, id_user: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "DELETE FROM category WHERE id = ?1 AND id_user = ?2",
        params![id, id_user],
    )? > 0)
}

/// Applies a new sidebar ordering. Positions are assigned from the slice index, so the caller
/// supplies the complete desired order.
pub fn reorder(conn: &Connection, id_user: i64, ordered_ids: &[i64]) -> SqlResult<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut updated = 0;
    for (position, id) in ordered_ids.iter().enumerate() {
        updated += tx.execute(
            "UPDATE category SET position = ?3 WHERE id = ?1 AND id_user = ?2",
            params![id, id_user, position as i64],
        )?;
    }
    tx.commit()?;
    Ok(updated)
}
