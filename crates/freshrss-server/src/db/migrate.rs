//! Versioned schema migrations.
//!
//! A ~40-line runner rather than `refinery`: the embedded `.sql` files are applied in version
//! order inside `schema_migrations`, and each applied row records a SHA-1 of its contents so
//! editing an already-applied migration is a hard error instead of silent drift.

use rusqlite::{Connection, Result as SqlResult, params};
use sha1::Sha1;
use sha1::digest::{Digest, FixedOutput};
use thiserror::Error;

/// One migration. `sql` is embedded at compile time so the binary is self-contained.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Append new migrations here; never edit or reorder an entry that has shipped.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("../../migrations/V1__init.sql"),
}];

/// Value written to `meta('schema_version')` once everything is applied.
pub const SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].version;

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "migration V{version} ({name}) has changed since it was applied: \
         recorded checksum {recorded}, file now hashes to {actual}"
    )]
    ChecksumMismatch {
        version: i64,
        name: String,
        recorded: String,
        actual: String,
    },
    #[error("migration V{0} is missing from MIGRATIONS but is recorded as applied")]
    UnknownAppliedVersion(i64),
    #[error("database declares text_encoding = {found:?}, expected {expected:?}")]
    WrongTextEncoding { found: String, expected: String },
}

/// What a migration run did, for logging and the `migrate` CLI subcommand.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub applied: Vec<i64>,
    pub already_current: bool,
    pub schema_version: i64,
}

fn checksum(sql: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(sql.as_bytes());
    hex::encode(hasher.finalize_fixed())
}

/// Creates only the bookkeeping table the runner itself needs.
///
/// `meta` is deliberately *not* created here: V1 owns it, and pre-creating it would make V1
/// fail with "table meta already exists".
fn ensure_bookkeeping(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             name       TEXT    NOT NULL,
             checksum   TEXT    NOT NULL,
             applied_at INTEGER NOT NULL
         );",
    )
}

/// Applies every pending migration. Safe to call on every start.
pub fn run(conn: &Connection) -> Result<Report, MigrateError> {
    ensure_bookkeeping(conn)?;

    let applied: std::collections::HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT version, checksum FROM schema_migrations")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (version, checksum) = row?;
            map.insert(version, checksum);
        }
        map
    };

    let mut report = Report::default();
    for migration in MIGRATIONS {
        let actual = checksum(migration.sql);
        match applied.get(&migration.version) {
            Some(recorded) if recorded == &actual => {
                report.schema_version = migration.version;
            }
            Some(recorded) => {
                return Err(MigrateError::ChecksumMismatch {
                    version: migration.version,
                    name: migration.name.to_string(),
                    recorded: recorded.clone(),
                    actual,
                });
            }
            None => {
                // Each migration is its own transaction: a failure leaves earlier ones applied
                // and this one unrecorded, so a retry resumes rather than half-applies.
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(migration.sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, name, checksum, applied_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        migration.version,
                        migration.name,
                        actual,
                        jiff::Timestamp::now().as_second()
                    ],
                )?;
                tx.commit()?;
                report.applied.push(migration.version);
                report.schema_version = migration.version;
            }
        }
    }

    // Any recorded version we no longer ship means the binary is older than the database.
    for version in applied.keys() {
        if !MIGRATIONS.iter().any(|m| m.version == *version) {
            return Err(MigrateError::UnknownAppliedVersion(*version));
        }
    }

    // Refuse to serve a database written by a FreshRSS-compatible (HTML-encoded) schema.
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('text_encoding', 'utf8-plain')
         ON CONFLICT (key) DO NOTHING",
        [],
    )?;
    let encoding: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'text_encoding'", [], |r| {
            r.get(0)
        })
        .unwrap_or_else(|_| "utf8-plain".into());
    if encoding != "utf8-plain" {
        return Err(MigrateError::WrongTextEncoding {
            found: encoding,
            expected: "utf8-plain".into(),
        });
    }

    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT (key) DO UPDATE SET value = ?1",
        params![SCHEMA_VERSION.to_string()],
    )?;

    report.already_current = report.applied.is_empty();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection::open_in_memory().expect("in-memory db")
    }

    #[test]
    fn applies_every_migration_and_records_it() {
        let c = conn();
        let report = run(&c).expect("first run applies");
        assert_eq!(report.applied, vec![1]);
        assert!(!report.already_current);
        assert_eq!(report.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn second_run_is_a_noop() {
        let c = conn();
        run(&c).expect("first");
        let report = run(&c).expect("second");
        assert!(report.applied.is_empty());
        assert!(report.already_current);
        assert_eq!(report.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn creates_every_table_and_index() {
        let c = conn();
        run(&c).expect("migrate");
        let tables: Vec<String> = {
            let mut stmt = c
                .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
                .expect("prepare");
            stmt.query_map([], |r| r.get(0))
                .expect("query")
                .map(|r| r.expect("row"))
                .collect()
        };
        for expected in [
            "category",
            "entry",
            "entrytag",
            "entrytmp",
            "feed",
            "meta",
            "schema_migrations",
            "session",
            "tag",
            "user",
            "user_query",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; have {tables:?}"
            );
        }
    }

    #[test]
    fn records_the_text_encoding_marker() {
        let c = conn();
        run(&c).expect("migrate");
        let v: String = c
            .query_row("SELECT value FROM meta WHERE key='text_encoding'", [], |r| r.get(0))
            .expect("marker");
        assert_eq!(v, "utf8-plain");
    }

    #[test]
    fn refuses_a_database_marked_as_html_encoded() {
        let c = conn();
        run(&c).expect("migrate");
        c.execute(
            "UPDATE meta SET value='html-entities' WHERE key='text_encoding'",
            [],
        )
        .expect("mark");
        let err = run(&c).expect_err("must refuse");
        assert!(
            matches!(err, MigrateError::WrongTextEncoding { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn detects_an_edited_applied_migration() {
        let c = conn();
        run(&c).expect("migrate");
        c.execute(
            "UPDATE schema_migrations SET checksum='deadbeef' WHERE version=1",
            [],
        )
        .expect("tamper");
        let err = run(&c).expect_err("must refuse");
        assert!(
            matches!(err, MigrateError::ChecksumMismatch { version: 1, .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn detects_a_database_newer_than_the_binary() {
        let c = conn();
        run(&c).expect("migrate");
        c.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (999, 'future', 'x', 0)",
            [],
        )
        .expect("insert");
        let err = run(&c).expect_err("must refuse");
        assert!(matches!(err, MigrateError::UnknownAppliedVersion(999)));
    }

    #[test]
    fn foreign_keys_cascade_from_user() {
        let c = conn();
        c.pragma_update(None, "foreign_keys", true).expect("fk on");
        run(&c).expect("migrate");
        c.execute(
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
            [],
        )
        .expect("user");
        c.execute(
            "INSERT INTO category (id, id_user, name) VALUES (1, 1, 'News')",
            [],
        )
        .expect("category");
        c.execute(
            "INSERT INTO feed (id, id_user, url, name, category) VALUES (1, 1, 'https://e.com/f', 'E', 1)",
            [],
        )
        .expect("feed");
        c.execute(
            "INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (1, 1, 'g', 'T', 'https://e.com/a', 1)",
            [],
        )
        .expect("entry");

        c.execute("DELETE FROM user WHERE id = 1", []).expect("delete user");
        let left: i64 = c
            .query_row(
                "SELECT (SELECT COUNT(*) FROM category) + (SELECT COUNT(*) FROM feed) + (SELECT COUNT(*) FROM entry)",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(left, 0, "deleting a user must cascade to categories, feeds and entries");
    }

    #[test]
    fn deleting_a_feed_orphans_its_category_reference_not_the_reverse() {
        let c = conn();
        c.pragma_update(None, "foreign_keys", true).expect("fk on");
        run(&c).expect("migrate");
        c.execute(
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'a', 'x', 0)",
            [],
        )
        .expect("user");
        c.execute("INSERT INTO category (id, id_user, name) VALUES (1, 1, 'News')", [])
            .expect("category");
        c.execute(
            "INSERT INTO feed (id, id_user, url, name, category) VALUES (1, 1, 'u', 'n', 1)",
            [],
        )
        .expect("feed");
        // ON DELETE SET NULL, matching FreshRSS.
        c.execute("DELETE FROM category WHERE id = 1", []).expect("del cat");
        let cat: Option<i64> = c
            .query_row("SELECT category FROM feed WHERE id = 1", [], |r| r.get(0))
            .expect("feed survives");
        assert_eq!(cat, None);
    }

    #[test]
    fn entry_guid_is_unique_per_feed_but_reusable_across_feeds() {
        let c = conn();
        c.pragma_update(None, "foreign_keys", true).expect("fk on");
        run(&c).expect("migrate");
        c.execute(
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'a', 'x', 0)",
            [],
        )
        .expect("user");
        for id in [1, 2] {
            c.execute(
                "INSERT INTO feed (id, id_user, url, name) VALUES (?1, 1, ?1, ?1)",
                params![id],
            )
            .expect("feed");
        }
        for (entry_id, feed_id) in [(10, 1), (11, 2)] {
            c.execute(
                "INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (?1, 1, 'same', 't', 'l', ?2)",
                params![entry_id, feed_id],
            )
            .expect("same guid in a different feed is fine");
        }
        let dup = c.execute(
            "INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (12, 1, 'same', 't', 'l', 1)",
            [],
        );
        assert!(dup.is_err(), "duplicate (id_feed, guid) must be rejected");
    }

    #[test]
    fn category_names_are_unique_per_user_not_globally() {
        let c = conn();
        c.pragma_update(None, "foreign_keys", true).expect("fk on");
        run(&c).expect("migrate");
        for id in [1, 2] {
            c.execute(
                "INSERT INTO user (id, username, password_hash, created_at) VALUES (?1, ?1, 'x', 0)",
                params![id],
            )
            .expect("user");
            c.execute(
                "INSERT INTO category (id_user, name) VALUES (?1, 'Uncategorized')",
                params![id],
            )
            .expect("both users may have the same category name");
        }
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM category WHERE name='Uncategorized'", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(n, 2);
        let dup = c.execute(
            "INSERT INTO category (id_user, name) VALUES (1, 'Uncategorized')",
            [],
        );
        assert!(dup.is_err(), "same user, same name must be rejected");
    }
}
