//! `tag` and `entrytag` access — user labels.
//!
//! Labels are per-user (`UNIQUE(id_user, name)`), so two people can both have a label called
//! "todo" without colliding. Feed-provided keywords are a different thing entirely: those live in
//! `entry.tags` and are matched by the `#foo` search operator rather than by `labels:`.

use std::collections::HashMap;

use sqlx::sqlite::SqliteRow;
use sqlx::{Connection as _, QueryBuilder, Row as _, Sqlite, SqliteConnection};

use crate::db::dao::row_attributes;
use crate::db::dao::{chunks, push_in_list, MAX_BIND_PARAMS};
use crate::domain::attributes::Attributes;
use crate::domain::tag::Tag;
use crate::error::Result;

pub const COLUMNS: &str = "id, id_user, name, attributes, name_folded";

/// The same list qualified with the `t.` alias, for joins.
const COLUMNS_ALIASED: &str = "t.id, t.id_user, t.name, t.attributes, t.name_folded";

fn tag_of(row: &SqliteRow) -> std::result::Result<Tag, sqlx::Error> {
    Ok(Tag {
        id: row.try_get("id")?,
        id_user: row.try_get("id_user")?,
        name: row.try_get("name")?,
        attributes: row_attributes(row, "attributes")?,
    })
}

/// One row of the `entrytag` join: the article a label is on, and the label.
fn entry_label_of(row: SqliteRow) -> std::result::Result<(i64, Tag), sqlx::Error> {
    Ok((row.try_get("id_entry")?, tag_of(&row)?))
}

pub async fn insert(conn: &mut SqliteConnection, id_user: i64, name: &str) -> Result<i64> {
    let id = sqlx::query("INSERT INTO tag (id_user, name, name_folded) VALUES (?1, ?2, ?3)")
        .bind(id_user)
        .bind(name)
        .bind(freshrss_search::fold(name))
        .execute(&mut *conn)
        .await?
        .last_insert_rowid();
    Ok(id)
}

/// Returns the existing label's id or creates it.
///
/// The `INSERT … ON CONFLICT DO NOTHING` then re-select shape avoids a read-then-write race: two
/// concurrent requests labelling an article "todo" must not both insert, and `UNIQUE(id_user,name)`
/// alone would turn the loser into an error the user sees.
pub async fn find_or_create(conn: &mut SqliteConnection, id_user: i64, name: &str) -> Result<i64> {
    sqlx::query(
        "INSERT INTO tag (id_user, name, name_folded) VALUES (?1, ?2, ?3)
         ON CONFLICT (id_user, name) DO NOTHING",
    )
    .bind(id_user)
    .bind(name)
    .bind(freshrss_search::fold(name))
    .execute(&mut *conn)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM tag WHERE id_user = ?1 AND name = ?2")
            .bind(id_user)
            .bind(name)
            .fetch_one(&mut *conn)
            .await?,
    )
}

pub async fn find_by_id(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<Option<Tag>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM tag WHERE id = ")
        .push_bind(id)
        .push(" AND id_user = ")
        .push_bind(id_user);
    Ok(builder
        .build()
        .try_map(|row: SqliteRow| tag_of(&row))
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn find_by_name(
    conn: &mut SqliteConnection,
    id_user: i64,
    name: &str,
) -> Result<Option<Tag>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM tag WHERE id_user = ")
        .push_bind(id_user)
        .push(" AND name = ")
        .push_bind(name);
    Ok(builder
        .build()
        .try_map(|row: SqliteRow| tag_of(&row))
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn list(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<Tag>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM tag WHERE id_user = ")
        .push_bind(id_user)
        .push(" ORDER BY name COLLATE NOCASE");
    Ok(builder
        .build()
        .try_map(|row: SqliteRow| tag_of(&row))
        .fetch_all(&mut *conn)
        .await?)
}

/// Labels together with how many articles each is on, for the sidebar. One query rather than a
/// count per label.
pub async fn list_with_counts(
    conn: &mut SqliteConnection,
    id_user: i64,
) -> Result<Vec<(Tag, i64, i64)>> {
    Ok(sqlx::query(
        "SELECT t.id, t.id_user, t.name, t.attributes,
                COUNT(et.id_entry) AS total,
                COALESCE(SUM(CASE WHEN e.is_read = 0 THEN 1 ELSE 0 END), 0) AS unread
         FROM tag t
         LEFT JOIN entrytag et ON et.id_tag = t.id
         LEFT JOIN entry e ON e.id = et.id_entry
         WHERE t.id_user = ?1
         GROUP BY t.id
         ORDER BY t.name COLLATE NOCASE",
    )
    .bind(id_user)
    .try_map(
        |row: SqliteRow| -> std::result::Result<(Tag, i64, i64), sqlx::Error> {
            Ok((tag_of(&row)?, row.try_get("total")?, row.try_get("unread")?))
        },
    )
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn rename(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    name: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE tag SET name = ?3, name_folded = ?4 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(name)
            .bind(freshrss_search::fold(name))
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_attributes(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    attributes: &Attributes,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE tag SET attributes = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(attributes.to_db())
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Deletes a label and, via `ON DELETE CASCADE`, its `entrytag` links. Articles are untouched.
pub async fn delete(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM tag WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

// --- entrytag --------------------------------------------------------------

/// Attaches or detaches a label, matching `FreshRSS_TagDAO::tagEntry`.
pub async fn tag_entry(
    conn: &mut SqliteConnection,
    id_tag: i64,
    id_entry: i64,
    tagged: bool,
) -> Result<()> {
    if tagged {
        sqlx::query("INSERT OR IGNORE INTO entrytag (id_tag, id_entry) VALUES (?1, ?2)")
            .bind(id_tag)
            .bind(id_entry)
            .execute(&mut *conn)
            .await?;
    } else {
        sqlx::query("DELETE FROM entrytag WHERE id_tag = ?1 AND id_entry = ?2")
            .bind(id_tag)
            .bind(id_entry)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Attaches one label to many articles at once.
///
/// Used after a refresh commits, because filter actions that add a label need real entry ids and
/// so can only run once the staged rows exist.
pub async fn tag_entries(
    conn: &mut SqliteConnection,
    id_tag: i64,
    id_entries: &[i64],
) -> Result<usize> {
    if id_entries.is_empty() {
        return Ok(0);
    }
    let mut added = 0;
    // Two parameters per row, because an anonymous placeholder is bound once per occurrence: the tag
    // id cannot be written once and referenced by every row. The chunk is sized for that.
    for chunk in id_entries.chunks(MAX_BIND_PARAMS / 2) {
        // One statement per chunk rather than per row: a refresh can commit thousands of articles
        // and a label rule matching most of them would otherwise issue thousands of inserts.
        let mut builder =
            QueryBuilder::<Sqlite>::new("INSERT OR IGNORE INTO entrytag (id_tag, id_entry)");
        builder.push_values(chunk.iter().copied(), |mut row, id_entry| {
            row.push_bind(id_tag).push_bind(id_entry);
        });
        added += builder.build().execute(&mut *conn).await?.rows_affected() as usize;
    }
    Ok(added)
}

pub async fn untag_entry(conn: &mut SqliteConnection, id_tag: i64, id_entry: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM entrytag WHERE id_tag = ?1 AND id_entry = ?2")
            .bind(id_tag)
            .bind(id_entry)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Replaces an article's labels with exactly `id_tags`.
pub async fn set_entry_tags(
    conn: &mut SqliteConnection,
    id_entry: i64,
    id_tags: &[i64],
) -> Result<()> {
    let mut tx = conn.begin().await?;
    sqlx::query("DELETE FROM entrytag WHERE id_entry = ?1")
        .bind(id_entry)
        .execute(&mut *tx)
        .await?;
    for id_tag in id_tags {
        sqlx::query("INSERT OR IGNORE INTO entrytag (id_tag, id_entry) VALUES (?1, ?2)")
            .bind(*id_tag)
            .bind(id_entry)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn labels_for_entry(conn: &mut SqliteConnection, id_entry: i64) -> Result<Vec<Tag>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS_ALIASED)
        .push(
            " FROM tag t
         INNER JOIN entrytag et ON et.id_tag = t.id
         WHERE et.id_entry = ",
        )
        .push_bind(id_entry)
        .push(
            "
         ORDER BY t.name COLLATE NOCASE",
        );
    Ok(builder
        .build()
        .try_map(|row: SqliteRow| tag_of(&row))
        .fetch_all(&mut *conn)
        .await?)
}

/// Labels for many articles in one query, keyed by entry id.
///
/// The list endpoint needs this for every row on the page; fetching per entry would be a classic
/// N+1 — 50 extra queries per page of 50 articles.
pub async fn labels_for_entries(
    conn: &mut SqliteConnection,
    id_entries: &[i64],
) -> Result<HashMap<i64, Vec<Tag>>> {
    let mut out: HashMap<i64, Vec<Tag>> = HashMap::new();
    for chunk in chunks(id_entries, 0) {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT et.id_entry, t.id, t.id_user, t.name, t.attributes, t.name_folded
             FROM entrytag et
             INNER JOIN tag t ON t.id = et.id_tag
             WHERE et.id_entry ",
        );
        push_in_list(&mut builder, chunk.iter().copied());
        builder.push(
            "
             ORDER BY t.name COLLATE NOCASE",
        );
        let rows = builder
            .build()
            .try_map(entry_label_of)
            .fetch_all(&mut *conn)
            .await?;
        for (entry_id, tag) in rows {
            out.entry(entry_id).or_default().push(tag);
        }
    }
    Ok(out)
}
