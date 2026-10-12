//! `category` table access — the reader's folders.
//!
//! Every query is scoped by `id_user`. That is not defensive redundancy: `category.name` is only
//! unique per user, so an unscoped lookup by name would return another user's folder.

use sqlx::sqlite::SqliteRow;
use sqlx::{Connection as _, Row as _, SqliteConnection};

use crate::db::dao::row_attributes;
use crate::db::sql::sql;
use crate::domain::attributes::Attributes;
use crate::domain::category::{Category, CategoryKind};
use crate::error::Result;

pub const COLUMNS: &str = "id, id_user, name, kind, lastUpdate, error, position, attributes";

fn from_row(row: SqliteRow) -> std::result::Result<Category, sqlx::Error> {
    let kind_raw: i64 = row.try_get("kind")?;
    Ok(Category {
        id: row.try_get("id")?,
        id_user: row.try_get("id_user")?,
        name: row.try_get("name")?,
        // An unrecognised kind degrades to a normal folder rather than failing the listing; the
        // value is preserved on the next write because `update_*` never touches `kind`.
        kind: CategoryKind::from_i16(kind_raw as i16).unwrap_or(CategoryKind::Normal),
        last_update: row.try_get("lastUpdate")?,
        error: row.try_get("error")?,
        position: row.try_get("position")?,
        attributes: row_attributes(&row, "attributes")?,
    })
}

pub struct NewCategory<'a> {
    pub id_user: i64,
    pub name: &'a str,
    pub kind: CategoryKind,
    pub position: i64,
    pub attributes: Attributes,
}

pub async fn insert(conn: &mut SqliteConnection, new: &NewCategory<'_>) -> Result<i64> {
    let id = sqlx::query(
        "INSERT INTO category (id_user, name, kind, position, attributes)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(new.id_user)
    .bind(new.name)
    .bind(i64::from(new.kind.as_i16()))
    .bind(new.position)
    .bind(new.attributes.to_db())
    .execute(&mut *conn)
    .await?
    .last_insert_rowid();
    Ok(id)
}

pub async fn find_by_id(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
) -> Result<Option<Category>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM category WHERE id = ?1 AND id_user = ?2"
    )))
    .bind(id)
    .bind(id_user)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Looks a folder up by name, for "add this feed to the folder called X" during OPML import.
pub async fn find_by_name(
    conn: &mut SqliteConnection,
    id_user: i64,
    name: &str,
) -> Result<Option<Category>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM category WHERE id_user = ?1 AND name = ?2"
    )))
    .bind(id_user)
    .bind(name)
    .try_map(from_row)
    .fetch_optional(&mut *conn)
    .await?)
}

/// All folders for a user, in sidebar order.
pub async fn list(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<Category>> {
    Ok(sqlx::query(sql(format!(
        "SELECT {COLUMNS} FROM category WHERE id_user = ?1 ORDER BY position, name COLLATE NOCASE"
    )))
    .bind(id_user)
    .try_map(from_row)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn count(conn: &mut SqliteConnection, id_user: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM category WHERE id_user = ?1")
            .bind(id_user)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// The next free sidebar position.
pub async fn next_position(conn: &mut SqliteConnection, id_user: i64) -> Result<i64> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(position) FROM category WHERE id_user = ?1")
            .bind(id_user)
            .fetch_one(&mut *conn)
            .await?;
    Ok(max.unwrap_or(-1) + 1)
}

pub async fn update_name(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    name: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE category SET name = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(name)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_position(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    position: i64,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE category SET position = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(position)
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
        sqlx::query("UPDATE category SET attributes = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(attributes.to_db())
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Records a refresh failure. `error` is a timestamp, not a boolean, so the sidebar can say how
/// stale the problem is.
pub async fn update_error(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    error: i64,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE category SET error = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .bind(error)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_last_update(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
    mtime: i64,
) -> Result<bool> {
    Ok(
        sqlx::query(
            "UPDATE category SET lastUpdate = ?3, error = 0 WHERE id = ?1 AND id_user = ?2",
        )
        .bind(id)
        .bind(id_user)
        .bind(mtime)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            > 0,
    )
}

/// Deletes a folder. Feeds inside it are **not** deleted — `feed.category` is
/// `ON DELETE SET NULL`, so they become orphans, which the reader lists separately. That matches
/// FreshRSS and avoids a folder rename going wrong destroying someone's subscriptions.
pub async fn delete(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM category WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Applies a new sidebar ordering. Positions are assigned from the slice index, so the caller
/// supplies the complete desired order.
pub async fn reorder(
    conn: &mut SqliteConnection,
    id_user: i64,
    ordered_ids: &[i64],
) -> Result<usize> {
    let mut tx = conn.begin().await?;
    let mut updated = 0;
    for (position, id) in ordered_ids.iter().enumerate() {
        updated += sqlx::query("UPDATE category SET position = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(*id)
            .bind(id_user)
            .bind(position as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(updated)
}
