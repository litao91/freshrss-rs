//! `session` table access.
//!
//! The cookie carries only an opaque random id; the session itself lives here so it can be listed,
//! revoked individually, and swept when it expires. A signed-JWT cookie would be stateless but
//! then "sign out everywhere" and the account menu's session list are both impossible.

use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params};

use crate::domain::ids::now_unix;

#[derive(Debug, Clone, PartialEq)]
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
pub fn create(
    conn: &Connection,
    id_user: i64,
    ttl_secs: i64,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> SqlResult<Session> {
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
    conn.execute(
        "INSERT INTO session (id, id_user, created_at, expires_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            session.id,
            session.id_user,
            session.created_at,
            session.expires_at,
            session.ip,
            session.user_agent,
        ],
    )?;
    Ok(session)
}

const COLUMNS: &str = "id, id_user, created_at, expires_at, ip, user_agent";

/// The same list qualified with the `s.` alias, for the join in [`find_valid`].
const COLUMNS_ALIASED: &str =
    "s.id, s.id_user, s.created_at, s.expires_at, s.ip, s.user_agent";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Session> {
    Ok(Session {
        id: row.get(0)?,
        id_user: row.get(1)?,
        created_at: row.get(2)?,
        expires_at: row.get(3)?,
        ip: row.get(4)?,
        user_agent: row.get(5)?,
    })
}

/// Looks a session up, returning `None` when it does not exist, has expired, or belongs to a
/// disabled account.
///
/// Expiry is enforced here rather than only by the cookie's `Max-Age`, because a cookie can be
/// replayed after the account was disabled and because the sweep is opportunistic.
pub fn find_valid(conn: &Connection, id: &str) -> SqlResult<Option<Session>> {
    let now = now_unix();
    conn.query_row(
        &format!(
            "SELECT {COLUMNS_ALIASED} FROM session s
             INNER JOIN user u ON u.id = s.id_user
             WHERE s.id = ?1 AND s.expires_at > ?2 AND u.enabled = 1"
        ),
        params![id, now],
        from_row,
    )
    .optional()
}

pub fn find(conn: &Connection, id: &str) -> SqlResult<Option<Session>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM session WHERE id = ?1"),
        params![id],
        from_row,
    )
    .optional()
}

pub fn list_for_user(conn: &Connection, id_user: i64) -> SqlResult<Vec<Session>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM session WHERE id_user = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map(params![id_user], from_row)?;
    rows.collect()
}

pub fn revoke(conn: &Connection, id: &str) -> SqlResult<bool> {
    Ok(conn.execute("DELETE FROM session WHERE id = ?1", params![id])? > 0)
}

pub fn revoke_for_user(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    Ok(conn.execute(
        "DELETE FROM session WHERE id_user = ?1",
        params![id_user],
    )?)
}

/// Signs out every session except `keep`, for "sign out other devices" after a password change.
pub fn revoke_others(conn: &Connection, id_user: i64, keep: &str) -> SqlResult<usize> {
    Ok(conn.execute(
        "DELETE FROM session WHERE id_user = ?1 AND id <> ?2",
        params![id_user, keep],
    )?)
}

/// Deletes expired sessions. Runs opportunistically from the scheduler rather than on a timer of
/// its own; a stale row is inert, so an occasional skip costs nothing.
pub fn sweep_expired(conn: &Connection) -> SqlResult<usize> {
    Ok(conn.execute(
        "DELETE FROM session WHERE expires_at <= ?1",
        params![now_unix()],
    )?)
}
