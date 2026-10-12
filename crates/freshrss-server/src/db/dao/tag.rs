//! `tag` and `entrytag` access — user labels.
//!
//! Labels are per-user (`UNIQUE(id_user, name)`), so two people can both have a label called
//! "todo" without colliding. Feed-provided keywords are a different thing entirely: those live in
//! `entry.tags` and are matched by the `#foo` search operator rather than by `labels:`.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params};

use crate::db::dao::row_attributes;
use crate::db::sql::{MAX_BIND_PARAMS, in_clause};
use crate::domain::attributes::Attributes;
use crate::domain::tag::Tag;

pub const COLUMNS: &str = "id, id_user, name, attributes";

/// The same list qualified with the `t.` alias, for joins.
const COLUMNS_ALIASED: &str = "t.id, t.id_user, t.name, t.attributes";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Tag> {
    Ok(Tag {
        id: row.get(0)?,
        id_user: row.get(1)?,
        name: row.get(2)?,
        attributes: row_attributes(row, 3)?,
    })
}

pub fn insert(conn: &Connection, id_user: i64, name: &str) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO tag (id_user, name) VALUES (?1, ?2)",
        params![id_user, name],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns the existing label's id or creates it.
///
/// The `INSERT … ON CONFLICT DO NOTHING` then re-select shape avoids a read-then-write race: two
/// concurrent requests labelling an article "todo" must not both insert, and `UNIQUE(id_user,name)`
/// alone would turn the loser into an error the user sees.
pub fn find_or_create(conn: &Connection, id_user: i64, name: &str) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO tag (id_user, name) VALUES (?1, ?2)
         ON CONFLICT (id_user, name) DO NOTHING",
        params![id_user, name],
    )?;
    conn.query_row(
        "SELECT id FROM tag WHERE id_user = ?1 AND name = ?2",
        params![id_user, name],
        |r| r.get(0),
    )
}

pub fn find_by_id(conn: &Connection, id: i64, id_user: i64) -> SqlResult<Option<Tag>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM tag WHERE id = ?1 AND id_user = ?2"),
        params![id, id_user],
        from_row,
    )
    .optional()
}

pub fn find_by_name(conn: &Connection, id_user: i64, name: &str) -> SqlResult<Option<Tag>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM tag WHERE id_user = ?1 AND name = ?2"),
        params![id_user, name],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection, id_user: i64) -> SqlResult<Vec<Tag>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM tag WHERE id_user = ?1 ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_user], from_row)?;
    rows.collect()
}

/// Labels together with how many articles each is on, for the sidebar. One query rather than a
/// count per label.
pub fn list_with_counts(conn: &Connection, id_user: i64) -> SqlResult<Vec<(Tag, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.id_user, t.name, t.attributes,
                COUNT(et.id_entry) AS total,
                COALESCE(SUM(CASE WHEN e.is_read = 0 THEN 1 ELSE 0 END), 0) AS unread
         FROM tag t
         LEFT JOIN entrytag et ON et.id_tag = t.id
         LEFT JOIN entry e ON e.id = et.id_entry
         WHERE t.id_user = ?1
         GROUP BY t.id
         ORDER BY t.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map(params![id_user], |row| {
        Ok((
            Tag {
                id: row.get(0)?,
                id_user: row.get(1)?,
                name: row.get(2)?,
                attributes: row_attributes(row, 3)?,
            },
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    rows.collect()
}

pub fn rename(conn: &Connection, id: i64, id_user: i64, name: &str) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE tag SET name = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, name],
    )? > 0)
}

pub fn update_attributes(
    conn: &Connection,
    id: i64,
    id_user: i64,
    attributes: &Attributes,
) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE tag SET attributes = ?3 WHERE id = ?1 AND id_user = ?2",
        params![id, id_user, attributes.to_db()],
    )? > 0)
}

/// Deletes a label and, via `ON DELETE CASCADE`, its `entrytag` links. Articles are untouched.
pub fn delete(conn: &Connection, id: i64, id_user: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "DELETE FROM tag WHERE id = ?1 AND id_user = ?2",
        params![id, id_user],
    )? > 0)
}

// --- entrytag --------------------------------------------------------------

/// Attaches or detaches a label, matching `FreshRSS_TagDAO::tagEntry`.
pub fn tag_entry(conn: &Connection, id_tag: i64, id_entry: i64, tagged: bool) -> SqlResult<()> {
    if tagged {
        conn.execute(
            "INSERT OR IGNORE INTO entrytag (id_tag, id_entry) VALUES (?1, ?2)",
            params![id_tag, id_entry],
        )?;
    } else {
        conn.execute(
            "DELETE FROM entrytag WHERE id_tag = ?1 AND id_entry = ?2",
            params![id_tag, id_entry],
        )?;
    }
    Ok(())
}

/// Attaches one label to many articles at once.
///
/// Used after a refresh commits, because filter actions that add a label need real entry ids and
/// so can only run once the staged rows exist.
pub fn tag_entries(conn: &Connection, id_tag: i64, id_entries: &[i64]) -> SqlResult<usize> {
    if id_entries.is_empty() {
        return Ok(0);
    }
    let mut added = 0;
    // One parameter is taken by the tag id, so the entry chunk must leave room for it.
    for chunk in id_entries.chunks(MAX_BIND_PARAMS - 1) {
        // One statement per chunk rather than per row: a refresh can commit thousands of articles
        // and a label rule matching most of them would otherwise issue thousands of inserts.
        let mut sql = String::from("INSERT OR IGNORE INTO entrytag (id_tag, id_entry) VALUES ");
        let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len());
        for (i, entry_id) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            // ?1 is the tag; the entry ids follow.
            sql.push_str(&format!("(?1,?{})", i + 2));
            values.push(entry_id as &dyn rusqlite::ToSql);
        }
        let mut all: Vec<&dyn rusqlite::ToSql> = vec![&id_tag];
        all.extend(values);
        added += conn.execute(&sql, all.as_slice())?;
    }
    Ok(added)
}

pub fn untag_entry(conn: &Connection, id_tag: i64, id_entry: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "DELETE FROM entrytag WHERE id_tag = ?1 AND id_entry = ?2",
        params![id_tag, id_entry],
    )? > 0)
}

/// Replaces an article's labels with exactly `id_tags`.
pub fn set_entry_tags(conn: &Connection, id_entry: i64, id_tags: &[i64]) -> SqlResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM entrytag WHERE id_entry = ?1",
        params![id_entry],
    )?;
    for id_tag in id_tags {
        tx.execute(
            "INSERT OR IGNORE INTO entrytag (id_tag, id_entry) VALUES (?1, ?2)",
            params![id_tag, id_entry],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn labels_for_entry(conn: &Connection, id_entry: i64) -> SqlResult<Vec<Tag>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS_ALIASED} FROM tag t
         INNER JOIN entrytag et ON et.id_tag = t.id
         WHERE et.id_entry = ?1
         ORDER BY t.name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map(params![id_entry], from_row)?;
    rows.collect()
}

/// Labels for many articles in one query, keyed by entry id.
///
/// The list endpoint needs this for every row on the page; fetching per entry would be a classic
/// N+1 — 50 extra queries per page of 50 articles.
pub fn labels_for_entries(
    conn: &Connection,
    id_entries: &[i64],
) -> SqlResult<HashMap<i64, Vec<Tag>>> {
    let mut out: HashMap<i64, Vec<Tag>> = HashMap::new();
    for chunk in id_entries.chunks(MAX_BIND_PARAMS) {
        let placeholders = in_clause(chunk.len());
        let sql = format!(
            "SELECT et.id_entry, t.id, t.id_user, t.name, t.attributes
             FROM entrytag et
             INNER JOIN tag t ON t.id = et.id_tag
             WHERE et.id_entry IN {placeholders}
             ORDER BY t.name COLLATE NOCASE"
        );
        let mut stmt = conn.prepare(&sql)?;
        let values: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(values.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Tag {
                    id: row.get(1)?,
                    id_user: row.get(2)?,
                    name: row.get(3)?,
                    attributes: row_attributes(row, 4)?,
                },
            ))
        })?;
        for row in rows {
            let (entry_id, tag) = row?;
            out.entry(entry_id).or_default().push(tag);
        }
    }
    Ok(out)
}
