//! `session` table access.
//!
//! The cookie carries only an opaque random id; the session itself lives here so it can be listed,
//! revoked individually, and swept when it expires. A signed-JWT cookie would be stateless but
//! then "sign out everywhere" and the account menu's session list are both impossible.

use sqlx::SqliteConnection;

use crate::db::sql::sql;
use crate::domain::ids::now_unix;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Session {
    pub id: String,
    pub id_user: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        self.expires_at <= now_unix()
    }
}

/// Creates a session with a random id and returns it.
pub async fn create(
    conn: &mut SqliteConnection,
    id_user: i64,
    ttl_secs: i64,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Session> {
    let now = now_unix();
    let session = Session {
        // v4 is 122 bits of entropy, which is plenty for a bearer token that is only ever
        // compared by equality against this table.
        id: uuid::Uuid::new_v4().simple().to_string(),
        id_user,
        created_at: now,
        expires_at: now + ttl_secs.max(1),
        ip: ip.map(str::to_string),
        user_agent: user_agent.map(str::to_string),
    };
    sqlx::query(
        "INSERT INTO session (id, id_user, created_at, expires_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&session.id)
    .bind(session.id_user)
    .bind(session.created_at)
    .bind(session.expires_at)
    .bind(&session.ip)
    .bind(&session.user_agent)
    .execute(&mut *conn)
    .await?;
    Ok(session)
}

const COLUMNS: &str = "id, id_user, created_at, expires_at, ip, user_agent";

/// The same list qualified with the `s.` alias, for the join in [`find_valid`].
const COLUMNS_ALIASED: &str = "s.id, s.id_user, s.created_at, s.expires_at, s.ip, s.user_agent";

/// Looks a session up, returning `None` when it does not exist, has expired, or belongs to a
/// disabled account.
///
/// Expiry is enforced here rather than only by the cookie's `Max-Age`, because a cookie can be
/// replayed after the account was disabled and because the sweep is opportunistic.
pub async fn find_valid(conn: &mut SqliteConnection, id: &str) -> Result<Option<Session>> {
    let now = now_unix();
    Ok(sqlx::query_as(sql(format!(
        "SELECT {COLUMNS_ALIASED} FROM session s
         INNER JOIN user u ON u.id = s.id_user
         WHERE s.id = ?1 AND s.expires_at > ?2 AND u.enabled = 1"
    )))
    .bind(id)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn find(conn: &mut SqliteConnection, id: &str) -> Result<Option<Session>> {
    Ok(
        sqlx::query_as(sql(format!("SELECT {COLUMNS} FROM session WHERE id = ?1")))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

pub async fn list_for_user(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<Session>> {
    Ok(sqlx::query_as(sql(format!(
        "SELECT {COLUMNS} FROM session WHERE id_user = ?1 ORDER BY created_at DESC"
    )))
    .bind(id_user)
    .fetch_all(&mut *conn)
    .await?)
}

pub async fn revoke(conn: &mut SqliteConnection, id: &str) -> Result<bool> {
    Ok(sqlx::query("DELETE FROM session WHERE id = ?1")
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        > 0)
}

pub async fn revoke_for_user(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    Ok(sqlx::query("DELETE FROM session WHERE id_user = ?1")
        .bind(id_user)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}

/// Signs out every session except `keep`, for "sign out other devices" after a password change.
pub async fn revoke_others(conn: &mut SqliteConnection, id_user: i64, keep: &str) -> Result<usize> {
    Ok(
        sqlx::query("DELETE FROM session WHERE id_user = ?1 AND id <> ?2")
            .bind(id_user)
            .bind(keep)
            .execute(&mut *conn)
            .await?
            .rows_affected() as usize,
    )
}

/// Deletes expired sessions. Runs opportunistically from the scheduler rather than on a timer of
/// its own; a stale row is inert, so an occasional skip costs nothing.
pub async fn sweep_expired(conn: &mut SqliteConnection) -> Result<usize> {
    Ok(sqlx::query("DELETE FROM session WHERE expires_at <= ?1")
        .bind(now_unix())
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}
