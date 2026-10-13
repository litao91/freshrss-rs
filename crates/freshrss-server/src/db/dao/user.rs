//! `user` table access.

use sqlx::sqlite::SqliteRow;
use sqlx::{Connection as _, QueryBuilder, Row as _, Sqlite, SqliteConnection};

use crate::db::dao::row_attributes;
use crate::domain::attributes::Attributes;
use crate::domain::ids::now_unix;
use crate::domain::user::{User, UserConfig};
use crate::error::Result;

/// Column list used by every `SELECT` in this module, so the shape of a row lives in one place.
pub const COLUMNS: &str = "id, username, email, password_hash, is_admin, enabled, \
                           created_at, last_seen_at, config, attributes";

fn from_row(row: SqliteRow) -> std::result::Result<User, sqlx::Error> {
    let config_raw: Option<String> = row.try_get("config")?;
    let is_admin: i64 = row.try_get("is_admin")?;
    let enabled: i64 = row.try_get("enabled")?;
    // A corrupt config must not lock a user out of their own account; fall back to defaults and
    // let the next write repair it.
    let config = config_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<UserConfig>(raw).ok())
        .unwrap_or_default();

    Ok(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        email: row.try_get("email")?,
        password_hash: row.try_get("password_hash")?,
        is_admin: is_admin != 0,
        enabled: enabled != 0,
        created_at: row.try_get("created_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        config,
        attributes: row_attributes(&row, "attributes")?,
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
pub async fn insert(conn: &mut SqliteConnection, new: &NewUser<'_>) -> Result<i64> {
    let mut tx = conn.begin().await?;
    let config_json = serde_json::to_string(&UserConfig::default())?;

    let id = sqlx::query(
        "INSERT INTO user (username, email, password_hash, is_admin, enabled, created_at, config)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
    )
    .bind(new.username)
    .bind(new.email)
    .bind(new.password_hash)
    .bind(i64::from(new.is_admin))
    .bind(now_unix())
    .bind(config_json)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    sqlx::query("INSERT INTO category (id_user, name, position) VALUES (?1, ?2, 0)")
        .bind(id)
        .bind(crate::domain::category::DEFAULT_CATEGORY_NAME)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn find_by_id(conn: &mut SqliteConnection, id: i64) -> Result<Option<User>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user WHERE id = ")
        .push_bind(id);
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Looks a user up by username. The column is `COLLATE NOCASE`, so this matches regardless of the
/// case the visitor typed at the login form.
pub async fn find_by_username(conn: &mut SqliteConnection, username: &str) -> Result<Option<User>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user WHERE username = ")
        .push_bind(username);
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn find_by_email(conn: &mut SqliteConnection, email: &str) -> Result<Option<User>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user WHERE email = ")
        .push_bind(email);
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn list(conn: &mut SqliteConnection) -> Result<Vec<User>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder.push(COLUMNS).push(" FROM user ORDER BY username");
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_all(&mut *conn)
        .await?)
}

pub async fn count(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM user")
        .fetch_one(&mut *conn)
        .await?)
}

pub async fn update_password(
    conn: &mut SqliteConnection,
    id: i64,
    password_hash: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE user SET password_hash = ?2 WHERE id = ?1")
            .bind(id)
            .bind(password_hash)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn update_email(
    conn: &mut SqliteConnection,
    id: i64,
    email: Option<&str>,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE user SET email = ?2 WHERE id = ?1")
        .bind(id)
        .bind(email)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

pub async fn update_config(
    conn: &mut SqliteConnection,
    id: i64,
    config: &UserConfig,
) -> Result<bool> {
    let json = serde_json::to_string(config)?;
    Ok(sqlx::query("UPDATE user SET config = ?2 WHERE id = ?1")
        .bind(id)
        .bind(json)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

pub async fn update_attributes(
    conn: &mut SqliteConnection,
    id: i64,
    attributes: &Attributes,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE user SET attributes = ?2 WHERE id = ?1")
        .bind(id)
        .bind(attributes.to_db())
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

pub async fn set_enabled(conn: &mut SqliteConnection, id: i64, enabled: bool) -> Result<bool> {
    Ok(sqlx::query("UPDATE user SET enabled = ?2 WHERE id = ?1")
        .bind(id)
        .bind(i64::from(enabled))
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

/// Records activity. Called on authenticated requests, throttled by the caller so it is not a
/// write on every single hit.
pub async fn touch_last_seen(conn: &mut SqliteConnection, id: i64) -> Result<()> {
    sqlx::query("UPDATE user SET last_seen_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(now_unix())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Deletes a user. `ON DELETE CASCADE` removes their categories, feeds, entries, labels and
/// sessions; the schema tests in `crate::db` assert that.
pub async fn delete(conn: &mut SqliteConnection, id: i64) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM user WHERE id = ?1")
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

/// The id of the folder a new feed lands in when the caller does not name one.
pub async fn default_category_id(conn: &mut SqliteConnection, id_user: i64) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM category
         WHERE id_user = ?1 AND name = ?2
         ORDER BY position, id LIMIT 1",
    )
    .bind(id_user)
    .bind(crate::domain::category::DEFAULT_CATEGORY_NAME)
    .fetch_optional(&mut *conn)
    .await?)
}
