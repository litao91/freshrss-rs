//! `user` table access.

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::db::dao::row_attributes;
use crate::domain::attributes::Attributes;
use crate::domain::ids::now_unix;
use crate::domain::user::{User, UserConfig};

/// Column order used by every `SELECT` in this module, so row indexes stay in one place.
pub const COLUMNS: &str = "id, username, email, password_hash, is_admin, enabled, \
                           created_at, last_seen_at, config, attributes";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<User> {
    let config_raw: Option<String> = row.get(8)?;
    // A corrupt config must not lock a user out of their own account; fall back to defaults and
    // let the next write repair it.
    let config = config_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<UserConfig>(raw).ok())
        .unwrap_or_default();

    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        email: row.get(2)?,
        password_hash: row.get(3)?,
        is_admin: row.get::<_, i64>(4)? != 0,
        enabled: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        config,
        attributes: row_attributes(row, 9)?,
    })
}

/// Everything needed to create an account. The hash is computed by
/// [`crate::service::auth`], never here.
pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub password_hash: &'a str,
    pub is_admin: bool,
}

/// Inserts a user and seeds their `Uncategorized` folder.
///
/// FreshRSS inserts one global `category(id=1,'Uncategorized')` at install; with a shared database
/// the folder has to be per-user, and creating it here means no other code path can forget.
pub fn insert(conn: &Connection, new: &NewUser<'_>) -> SqlResult<i64> {
    let tx = conn.unchecked_transaction()?;
    let config_json = serde_json::to_string(&UserConfig::default())
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    tx.execute(
        "INSERT INTO user (username, email, password_hash, is_admin, enabled, created_at, config)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
        params![
            new.username,
            new.email,
            new.password_hash,
            i64::from(new.is_admin),
            now_unix(),
            config_json,
        ],
    )?;
    let id = tx.last_insert_rowid();

    tx.execute(
        "INSERT INTO category (id_user, name, position) VALUES (?1, ?2, 0)",
        params![id, crate::domain::category::DEFAULT_CATEGORY_NAME],
    )?;

    tx.commit()?;
    Ok(id)
}

pub fn find_by_id(conn: &Connection, id: i64) -> SqlResult<Option<User>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM user WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
}

/// Looks a user up by username. The column is `COLLATE NOCASE`, so this matches regardless of the
/// case the visitor typed at the login form.
pub fn find_by_username(conn: &Connection, username: &str) -> SqlResult<Option<User>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM user WHERE username = ?1"),
        params![username],
        from_row,
    )
    .optional()
}

pub fn find_by_email(conn: &Connection, email: &str) -> SqlResult<Option<User>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM user WHERE email = ?1"),
        params![email],
        from_row,
    )
    .optional()
}

pub fn list(conn: &Connection) -> SqlResult<Vec<User>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM user ORDER BY username"))?;
    let rows = stmt.query_map([], from_row)?;
    rows.collect()
}

pub fn count(conn: &Connection) -> SqlResult<i64> {
    conn.query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))
}

pub fn update_password(conn: &Connection, id: i64, password_hash: &str) -> SqlResult<bool> {
    let n = conn.execute(
        "UPDATE user SET password_hash = ?2 WHERE id = ?1",
        params![id, password_hash],
    )?;
    Ok(n > 0)
}

pub fn update_email(conn: &Connection, id: i64, email: Option<&str>) -> SqlResult<bool> {
    let n = conn.execute(
        "UPDATE user SET email = ?2 WHERE id = ?1",
        params![id, email],
    )?;
    Ok(n > 0)
}

pub fn update_config(conn: &Connection, id: i64, config: &UserConfig) -> SqlResult<bool> {
    let json = serde_json::to_string(config)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let n = conn.execute(
        "UPDATE user SET config = ?2 WHERE id = ?1",
        params![id, json],
    )?;
    Ok(n > 0)
}

pub fn update_attributes(conn: &Connection, id: i64, attributes: &Attributes) -> SqlResult<bool> {
    let n = conn.execute(
        "UPDATE user SET attributes = ?2 WHERE id = ?1",
        params![id, attributes.to_db()],
    )?;
    Ok(n > 0)
}

pub fn set_enabled(conn: &Connection, id: i64, enabled: bool) -> SqlResult<bool> {
    let n = conn.execute(
        "UPDATE user SET enabled = ?2 WHERE id = ?1",
        params![id, i64::from(enabled)],
    )?;
    Ok(n > 0)
}

/// Records activity. Called on authenticated requests, throttled by the caller so it is not a
/// write on every single hit.
pub fn touch_last_seen(conn: &Connection, id: i64) -> SqlResult<()> {
    conn.execute(
        "UPDATE user SET last_seen_at = ?2 WHERE id = ?1",
        params![id, now_unix()],
    )?;
    Ok(())
}

/// Deletes a user. `ON DELETE CASCADE` removes their categories, feeds, entries, labels and
/// sessions; the schema test in `db::migrate` asserts that.
pub fn delete(conn: &Connection, id: i64) -> SqlResult<bool> {
    let n = conn.execute("DELETE FROM user WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// The id of the folder a new feed lands in when the caller does not name one.
pub fn default_category_id(conn: &Connection, id_user: i64) -> SqlResult<Option<i64>> {
    conn.query_row(
        "SELECT id FROM category
         WHERE id_user = ?1 AND name = ?2
         ORDER BY position, id LIMIT 1",
        params![id_user, crate::domain::category::DEFAULT_CATEGORY_NAME],
        |r| r.get(0),
    )
    .optional()
}
