//! SQLite access: the connection pool, per-connection setup, and the one place work borrows a
//! connection.
//!
//! DAOs are `async fn(&mut SqliteConnection, …) -> Result<T>` and otherwise read like synchronous
//! code: [`Db::with`] is the only place a connection is acquired, so DAO unit tests need no
//! mocking and a transaction is just `conn.begin().await`.
//!
//! Writes that touch `entry`/`entrytmp` go through [`Db::with_writer`], which serializes them on an
//! in-process mutex. SQLite is single-writer; without this the scheduler and user mutations
//! interleave into `SQLITE_BUSY` storms that `busy_timeout` only papers over.
//!
//! The mutex is a contention measure, not a correctness one, and nothing load-bearing may depend on
//! it: it exists per process, so two server instances sharing one database file have no mutex
//! between them at all. Anything that must not interleave — the entry-id allocator above all — is
//! atomic in its own SQL instead.

pub mod dao;
pub mod fold;
pub mod sql;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqliteConnection, SqlitePool};
use tokio::sync::Mutex;

use crate::config::DatabaseConfig;
use crate::error::{Error, Result};

/// A connection borrowed from the pool for the duration of one [`Db::with`] call.
pub type Pooled = PoolConnection<Sqlite>;

/// The schema this binary knows how to build, embedded at compile time.
///
/// `sqlx::migrate!` lives behind sqlx's `macros` feature, which this crate does not enable: there
/// is no compile-time database to check against, and turning it on would only add the `query!`
/// family that every statement in [`dao`] deliberately avoids. This is the same proc macro that
/// path expands to.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::sqlx_macros::migrate!("./migrations");

/// The marker this port writes into `meta` to say that text columns hold clean UTF-8 rather than
/// FreshRSS's HTML-encoded form. See [`Db::refuse_html_encoded`].
pub const TEXT_ENCODING: &str = "utf8-plain";

/// A SQLite-backed store. Cheap to clone (the pool is `Arc`-shared).
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    writer: Arc<Mutex<()>>,
}

/// The connection setup every pool entry gets, as sqlx applies it on connect.
fn connect_options(
    filename: impl AsRef<std::path::Path>,
    busy_timeout: Duration,
) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(filename)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(busy_timeout)
        // Negative value = KiB, so this is a 16 MiB page cache.
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY")
}

impl Db {
    /// Opens (creating the file and parent directory if needed) and migrates the database.
    pub async fn open(cfg: &DatabaseConfig) -> Result<Self> {
        if let Some(parent) = cfg.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|source| Error::OpenDb {
                    path: cfg.path.display().to_string(),
                    source,
                })?;
            }
        }

        let options = connect_options(
            &cfg.path,
            Duration::from_millis(u64::from(cfg.busy_timeout_ms)),
        )
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);

        Self::build(options, cfg.pool_size.max(1)).await
    }

    /// A private, single-connection `:memory:` database for tests.
    ///
    /// The pool is capped at one connection because each `:memory:` connection is its own
    /// database — a larger pool would hand tests an empty schema at random. Recycling is off for
    /// the same reason: a connection retired by the idle reaper would come back empty.
    pub async fn in_memory() -> Result<Self> {
        let options = connect_options(":memory:", Duration::from_secs(5));
        Self::build(options, 1).await
    }

    async fn build(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await?;

        let db = Self {
            pool,
            writer: Arc::new(Mutex::new(())),
        };
        // Migrate eagerly so a bad schema fails at startup rather than on the first request.
        MIGRATOR.run(&db.pool).await?;
        db.refuse_html_encoded().await?;
        // Rows written before the folded search columns existed carry the empty default, which
        // would make every search over them match nothing.
        let folded = db
            .with(|mut conn| async move { fold::ensure(&mut conn).await })
            .await?;
        if folded > 0 {
            tracing::info!(rows = folded, "backfilled folded search columns");
        }
        Ok(db)
    }

    /// Runs `f` with a connection borrowed from the pool.
    ///
    /// The closure takes the connection by value rather than by `&mut`. A future that *borrowed* it
    /// would carry that borrow in its type, and `FnOnce(&mut SqliteConnection) -> Fut` cannot name
    /// one `Fut` for every borrow lifetime the caller might choose. Owning it keeps the call sites
    /// plain `async move` blocks; the DAOs still take `&mut SqliteConnection`, so they accept
    /// `&mut conn` and `&mut *tx` alike.
    pub async fn with<T, F, Fut>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: FnOnce(Pooled) -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
    {
        let conn = self.pool.acquire().await?;
        f(conn).await
    }

    /// Like [`Db::with`], but holds the process-wide writer lock for the duration.
    ///
    /// Use for anything that inserts or updates `entry`/`entrytmp`, or that allocates entry ids.
    pub async fn with_writer<T, F, Fut>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: FnOnce(Pooled) -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
    {
        let _guard = self.writer.lock().await;
        self.with(f).await
    }

    /// The highest schema version applied to this database.
    pub async fn schema_version(&self) -> Result<i64> {
        self.with(|mut conn| async move { applied_schema_version(&mut conn).await })
            .await
    }

    /// Runs `PRAGMA optimize`, as SQLite recommends before closing.
    pub async fn optimize(&self) -> Result<()> {
        self.with(|mut conn| async move {
            sqlx::query("PRAGMA optimize").execute(&mut *conn).await?;
            Ok(())
        })
        .await
    }

    /// Refuses to serve a database written by a FreshRSS-compatible (HTML-encoded) schema.
    ///
    /// FreshRSS stores title, author and body HTML-encoded at rest; this port stores clean UTF-8
    /// and says so in `meta`. There is no conversion to attempt — reading an encoded database as
    /// plain text would serve every article double-escaped — so it is a startup failure rather
    /// than a warning.
    async fn refuse_html_encoded(&self) -> Result<()> {
        self.with(|mut conn| async move {
            sqlx::query(
                "INSERT INTO meta (key, value) VALUES ('text_encoding', ?1)
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind(TEXT_ENCODING)
            .execute(&mut *conn)
            .await?;
            let found: String =
                sqlx::query_scalar("SELECT value FROM meta WHERE key = 'text_encoding'")
                    .fetch_one(&mut *conn)
                    .await?;
            if found != TEXT_ENCODING {
                return Err(Error::WrongTextEncoding {
                    found,
                    expected: TEXT_ENCODING.to_owned(),
                });
            }
            Ok(())
        })
        .await
    }
}

/// The highest schema version applied to a database, read from sqlx's own bookkeeping table.
///
/// sqlx owns `_sqlx_migrations` and records a checksum per applied file, so there is no second
/// copy of the version to keep in `meta` — and nothing that could disagree with it.
pub async fn applied_schema_version(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// The highest schema version this binary carries.
pub fn bundled_schema_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Shared scaffolding for the crate's database tests.
///
/// Every one of them starts from a migrated `:memory:` database and most of them poke it with raw
/// SQL to set up a state no DAO writes; keeping that in one place is what stops each test module
/// growing its own slightly different copy.
#[cfg(test)]
pub(crate) mod support {
    use super::*;

    /// A migrated in-memory database.
    pub(crate) async fn db() -> Db {
        Db::in_memory().await.expect("in-memory db")
    }

    /// Runs one statement that returns a single column, and unwraps it.
    pub(crate) async fn scalar<T>(db: &Db, sql_text: &'static str) -> T
    where
        T: for<'r> sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite> + Send + Sync + Unpin,
    {
        db.with(
            |mut conn| async move { Ok(sqlx::query_scalar(sql_text).fetch_one(&mut *conn).await?) },
        )
        .await
        .expect("query")
    }

    /// Runs a batch of statements, and panics if any of them fails.
    pub(crate) async fn exec(db: &Db, sql_text: &'static str) {
        db.with(|mut conn| async move { Ok(sqlx::raw_sql(sql_text).execute(&mut *conn).await?) })
            .await
            .expect("execute");
    }

    /// A file-backed database in a fresh temporary directory, with the config that opened it.
    pub(crate) async fn file_db(dir: &std::path::Path) -> (DatabaseConfig, Db) {
        let cfg = DatabaseConfig {
            path: dir.join("nested/dir/test.sqlite"),
            pool_size: 2,
            busy_timeout_ms: 5_000,
        };
        let db = Db::open(&cfg)
            .await
            .expect("open creates parent directories");
        (cfg, db)
    }
}

#[cfg(test)]
mod tests {
    use super::support::{db, exec, file_db, scalar};
    use super::*;

    #[tokio::test]
    async fn in_memory_opens_and_migrates() {
        let db = db().await;
        let version: i64 = scalar(&db, "SELECT MAX(version) FROM _sqlx_migrations").await;
        assert_eq!(version, bundled_schema_version());
        assert!(version >= 2, "the folded columns migration must be applied");
    }

    #[tokio::test]
    async fn in_memory_shares_one_database_across_borrows() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
        )
        .await;

        // A second borrow must see the row; with a pool > 1 on `:memory:` it would not.
        let count: i64 = scalar(&db, "SELECT COUNT(*) FROM user").await;
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn pragmas_are_applied_per_connection() {
        let db = db().await;
        let (fk, busy): (i64, i64) = db
            .with(|mut conn| async move {
                let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                    .fetch_one(&mut *conn)
                    .await?;
                let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
                    .fetch_one(&mut *conn)
                    .await?;
                Ok((fk, busy))
            })
            .await
            .expect("pragma");
        assert_eq!(
            fk, 1,
            "foreign_keys must be ON for the ON DELETE CASCADE schema"
        );
        assert_eq!(busy, 5_000);
    }

    #[tokio::test]
    async fn file_database_uses_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, db) = file_db(dir.path()).await;
        assert!(cfg.path.exists(), "database file must exist");

        let mode: String = scalar(&db, "PRAGMA journal_mode").await;
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn opening_the_same_file_twice_is_a_noop() {
        // The migrate subcommand and the server both open the database; the second open must not
        // try to re-apply anything.
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, db) = file_db(dir.path()).await;
        drop(db);
        let again = Db::open(&cfg).await.expect("reopen");
        assert_eq!(again.schema_version().await.expect("version"), 2);
    }

    #[tokio::test]
    async fn creates_every_table_and_index() {
        let db = db().await;
        let tables: Vec<String> = db
            .with(|mut conn| async move {
                Ok(sqlx::query_scalar(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .fetch_all(&mut *conn)
                .await?)
            })
            .await
            .expect("tables");
        for expected in [
            "_sqlx_migrations",
            "category",
            "entry",
            "entrytag",
            "entrytmp",
            "feed",
            "meta",
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

    #[tokio::test]
    async fn records_the_text_encoding_marker() {
        let db = db().await;
        let value: String = scalar(&db, "SELECT value FROM meta WHERE key='text_encoding'").await;
        assert_eq!(value, TEXT_ENCODING);
    }

    #[tokio::test]
    async fn refuses_a_database_marked_as_html_encoded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, db) = file_db(dir.path()).await;
        exec(
            &db,
            "UPDATE meta SET value='html-entities' WHERE key='text_encoding'",
        )
        .await;
        drop(db);

        let err = match Db::open(&cfg).await {
            Ok(_) => panic!("an HTML-encoded database must be refused, not opened"),
            Err(err) => err,
        };
        assert!(
            matches!(&err, Error::WrongTextEncoding { found, .. } if found == "html-entities"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn foreign_keys_cascade_from_user() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0);
             INSERT INTO category (id, id_user, name) VALUES (1, 1, 'News');
             INSERT INTO feed (id, id_user, url, name, category) VALUES (1, 1, 'https://e.com/f', 'E', 1);
             INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (1, 1, 'g', 'T', 'https://e.com/a', 1);
             DELETE FROM user WHERE id = 1;",
        )
        .await;
        let left: i64 = scalar(
            &db,
            "SELECT (SELECT COUNT(*) FROM category) + (SELECT COUNT(*) FROM feed) + (SELECT COUNT(*) FROM entry)",
        )
        .await;
        assert_eq!(
            left, 0,
            "deleting a user must cascade to categories, feeds and entries"
        );
    }

    #[tokio::test]
    async fn deleting_a_feed_orphans_its_category_reference_not_the_reverse() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'a', 'x', 0);
             INSERT INTO category (id, id_user, name) VALUES (1, 1, 'News');
             INSERT INTO feed (id, id_user, url, name, category) VALUES (1, 1, 'u', 'n', 1);
             DELETE FROM category WHERE id = 1;",
        )
        .await;
        // ON DELETE SET NULL, matching FreshRSS.
        let cat: Option<i64> = scalar(&db, "SELECT category FROM feed WHERE id = 1").await;
        assert_eq!(cat, None);
    }

    #[tokio::test]
    async fn entry_guid_is_unique_per_feed_but_reusable_across_feeds() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'a', 'x', 0);
             INSERT INTO feed (id, id_user, url, name) VALUES (1, 1, '1', '1'), (2, 1, '2', '2');
             INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (10, 1, 'same', 't', 'l', 1);
             INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (11, 1, 'same', 't', 'l', 2);",
        )
        .await;
        let dup = db
            .with(|mut conn| async move {
                Ok(sqlx::query(
                    "INSERT INTO entry (id, id_user, guid, title, link, id_feed) VALUES (12, 1, 'same', 't', 'l', 1)",
                )
                .execute(&mut *conn)
                .await)
            })
            .await
            .expect("insert");
        assert!(dup.is_err(), "duplicate (id_feed, guid) must be rejected");
    }

    #[tokio::test]
    async fn category_names_are_unique_per_user_not_globally() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, '1', 'x', 0), (2, '2', 'x', 0);
             INSERT INTO category (id_user, name) VALUES (1, 'Uncategorized'), (2, 'Uncategorized');",
        )
        .await;
        let n: i64 = scalar(
            &db,
            "SELECT COUNT(*) FROM category WHERE name='Uncategorized'",
        )
        .await;
        assert_eq!(n, 2);
        let dup = db
            .with(|mut conn| async move {
                Ok(
                    sqlx::query("INSERT INTO category (id_user, name) VALUES (1, 'Uncategorized')")
                        .execute(&mut *conn)
                        .await,
                )
            })
            .await
            .expect("insert");
        assert!(dup.is_err(), "same user, same name must be rejected");
    }

    #[tokio::test]
    async fn with_borrows_a_connection_from_the_pool() {
        let db = db().await;
        let n: i64 = scalar(&db, "SELECT COUNT(*) FROM feed").await;
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn with_writer_serializes_mutations() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
        )
        .await;

        // Twenty concurrent writers must all succeed and none may see a torn state.
        let mut handles = Vec::new();
        for i in 0..20 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                db.with_writer(move |mut conn| async move {
                    sqlx::query("INSERT INTO category (id_user, name) VALUES (1, ?1)")
                        .bind(format!("cat{i}"))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
                .await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("insert");
        }

        let n: i64 = scalar(&db, "SELECT COUNT(*) FROM category").await;
        assert_eq!(n, 20, "no write may be lost");
    }

    #[tokio::test]
    async fn transaction_rolls_back_on_error() {
        let db = db().await;
        exec(
            &db,
            "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
        )
        .await;

        let out: Result<()> = db
            .with(|mut conn| async move {
                use sqlx::Connection as _;
                let mut tx = conn.begin().await?;
                sqlx::query("INSERT INTO category (id_user, name) VALUES (1, 'kept')")
                    .execute(&mut *tx)
                    .await?;
                Err(Error::Internal("boom".into()))
            })
            .await;
        assert!(out.is_err());

        let n: i64 = scalar(&db, "SELECT COUNT(*) FROM category").await;
        assert_eq!(n, 0, "the failed transaction must not have committed");
    }

    #[tokio::test]
    async fn every_pooled_connection_sees_the_same_schema() {
        // The pool hands out several connections; a migration or pragma applied to only the first
        // would show up as a missing table on the second request.
        let db = db().await;
        exec(&db, "INSERT INTO meta (key, value) VALUES ('probe', '1')").await;
        for _ in 0..4 {
            let v: String = scalar(&db, "SELECT value FROM meta WHERE key = 'probe'").await;
            assert_eq!(v, "1");
        }
    }
}
