//! SQLite access: the connection pool, per-connection setup, and the single async↔sync boundary.
//!
//! DAOs are ordinary synchronous `fn(&Connection, …) -> Result<T>`. There is exactly one place
//! where async meets them — [`Db::with`] — so DAO unit tests need no runtime and no mocking, and
//! a transaction is just a closure that receives a `&Connection`.
//!
//! Writes that touch `entry`/`entrytmp` go through [`Db::with_writer`], which serializes them on
//! an in-process mutex. SQLite is single-writer; without this the scheduler and user mutations
//! interleave into `SQLITE_BUSY` storms that `busy_timeout` only papers over.

pub mod dao;
pub mod migrate;
pub mod sql;
pub mod udf;

use std::sync::Arc;
use std::time::Duration;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::config::DatabaseConfig;
use crate::error::Result;

pub type Pooled = r2d2::PooledConnection<SqliteConnectionManager>;

/// A SQLite-backed store. Cheap to clone (the pool is `Arc`-shared).
#[derive(Clone)]
pub struct Db {
    pool: Pool<SqliteConnectionManager>,
    writer: Arc<Mutex<()>>,
}

/// Applies PRAGMAs and registers the UDFs on every connection the pool hands out.
fn init_connection(conn: &mut Connection, busy_timeout_ms: u32, wal: bool) -> rusqlite::Result<()> {
    if wal {
        // `:memory:` cannot be WAL; the failure is benign there, so ignore it.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", busy_timeout_ms)?;
    // Negative value = KiB, so this is a 16 MiB page cache.
    conn.pragma_update(None, "cache_size", -16_000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    udf::register(conn)?;
    Ok(())
}

impl Db {
    /// Opens (creating the file and parent directory if needed) and migrates the database.
    pub fn open(cfg: &DatabaseConfig) -> Result<Self> {
        if let Some(parent) = cfg.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|source| crate::error::Error::OpenDb {
                    path: cfg.path.display().to_string(),
                    source,
                })?;
            }
        }

        let busy = cfg.busy_timeout_ms;
        let manager = SqliteConnectionManager::file(&cfg.path)
            .with_init(move |c| init_connection(c, busy, true));
        Self::build(manager, cfg.pool_size)
    }

    /// A private, single-connection `:memory:` database for tests.
    ///
    /// The pool is capped at one connection because each `:memory:` connection is its own
    /// database — a larger pool would hand tests an empty schema at random.
    pub fn in_memory() -> Result<Self> {
        let manager =
            SqliteConnectionManager::memory().with_init(|c| init_connection(c, 5_000, false));
        Self::build(manager, 1)
    }

    fn build(manager: SqliteConnectionManager, pool_size: u32) -> Result<Self> {
        let pool = Pool::builder()
            .max_size(pool_size.max(1))
            .min_idle(Some(1))
            .connection_timeout(Duration::from_secs(10))
            .build(manager)?;

        let db = Self {
            pool,
            writer: Arc::new(Mutex::new(())),
        };
        // Migrate eagerly so a bad schema fails at startup rather than on the first request.
        db.blocking_with(|conn| Ok(migrate::run(conn)?))?;
        Ok(db)
    }

    /// Borrows a connection. Synchronous; prefer [`Db::with`] from async code.
    pub fn get(&self) -> Result<Pooled> {
        Ok(self.pool.get()?)
    }

    /// Runs `f` against a pooled connection on the blocking thread pool.
    pub async fn with<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            f(&conn)
        })
        .await?
    }

    /// Like [`Db::with`], but holds the process-wide writer lock for the duration.
    ///
    /// Use for anything that inserts or updates `entry`/`entrytmp`, or that allocates entry ids.
    pub async fn with_writer<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _guard = self.writer.lock().await;
        self.with(f).await
    }

    /// Synchronous [`Db::with`] for the CLI and the scheduler's blocking paths.
    pub fn blocking_with<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self.pool.get()?;
        f(&conn)
    }

    /// Synchronous [`Db::with_writer`].
    pub fn blocking_with_writer<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let _guard = self.writer.blocking_lock();
        self.blocking_with(f)
    }

    /// Runs `f` inside a deferred transaction, on a pooled connection.
    pub fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        self.blocking_with(|conn| {
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
    }

    /// Runs `PRAGMA optimize`, as SQLite recommends before closing.
    pub fn optimize(&self) -> Result<()> {
        self.blocking_with(|conn| {
            conn.pragma_update(None, "optimize", "")?;
            Ok(())
        })
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // Only when this is the last handle — otherwise a clone going out of scope would
        // optimize a database still in use by the server.
        if Arc::strong_count(&self.writer) == 1 {
            let _ = self.optimize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_opens_and_migrates() {
        let db = Db::in_memory().expect("open");
        let version: i64 = db
            .blocking_with(|c| {
                Ok(c.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })?)
            })
            .expect("query");
        assert_eq!(version, migrate::SCHEMA_VERSION);
    }

    #[test]
    fn in_memory_shares_one_database_across_borrows() {
        let db = Db::in_memory().expect("open");
        db.blocking_with(|c| {
            c.execute(
                "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
                [],
            )?;
            Ok(())
        })
        .expect("insert");

        // A second borrow must see the row; with a pool > 1 on `:memory:` it would not.
        let count: i64 = db
            .blocking_with(|c| Ok(c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn pragmas_are_applied_per_connection() {
        let db = Db::in_memory().expect("open");
        let (fk, busy): (bool, i64) = db
            .blocking_with(|c| {
                let fk: bool = c
                    .pragma_query_value(None, "foreign_keys", |r| r.get(0))
                    .expect("fk");
                let busy: i64 = c
                    .pragma_query_value(None, "busy_timeout", |r| r.get(0))
                    .expect("busy");
                Ok((fk, busy))
            })
            .expect("pragma");
        assert!(fk, "foreign_keys must be ON for the ON DELETE CASCADE schema");
        assert_eq!(busy, 5_000);
    }

    #[test]
    fn file_database_uses_wal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = DatabaseConfig {
            path: dir.path().join("nested/dir/test.sqlite"),
            pool_size: 2,
            busy_timeout_ms: 5_000,
        };
        let db = Db::open(&cfg).expect("open creates parent directories");
        assert!(cfg.path.exists(), "database file must exist");

        let mode: String = db
            .blocking_with(|c| {
                Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?)
            })
            .expect("journal_mode");
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn with_runs_on_the_blocking_pool() {
        let db = Db::in_memory().expect("open");
        let n: i64 = db
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM feed", [], |r| r.get(0))?))
            .await
            .expect("async with");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn with_writer_serializes_mutations() {
        let db = Db::in_memory().expect("open");
        db.with_writer(|c| {
            c.execute(
                "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("insert");

        // Twenty concurrent writers must all succeed and none may see a torn state.
        let mut handles = Vec::new();
        for i in 0..20 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                db.with_writer(move |c| {
                    c.execute(
                        "INSERT INTO category (id_user, name) VALUES (1, ?1)",
                        rusqlite::params![format!("cat{i}")],
                    )?;
                    Ok(())
                })
                .await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("insert");
        }

        let n: i64 = db
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM category", [], |r| r.get(0))?))
            .await
            .expect("count");
        assert_eq!(n, 20, "no write may be lost");
    }

    #[tokio::test]
    async fn transaction_rolls_back_on_error() {
        let db = Db::in_memory().expect("open");
        db.blocking_with(|c| {
            c.execute(
                "INSERT INTO user (id, username, password_hash, created_at) VALUES (1, 'alice', 'x', 0)",
                [],
            )?;
            Ok(())
        })
        .expect("seed");

        let out: Result<()> = db.transaction(|c| {
            c.execute("INSERT INTO category (id_user, name) VALUES (1, 'kept')", [])?;
            Err(crate::error::Error::Internal("boom".into()))
        });
        assert!(out.is_err());

        let n: i64 = db
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM category", [], |r| r.get(0))?))
            .await
            .expect("count");
        assert_eq!(n, 0, "the failed transaction must not have committed");
    }

    #[test]
    fn udfs_are_available_on_pooled_connections() {
        let db = Db::in_memory().expect("open");
        let hit: i64 = db
            .blocking_with(|c| {
                Ok(c.query_row("SELECT frss_ilike('École', '%ecole%')", [], |r| {
                    r.get(0)
                })?)
            })
            .expect("udf");
        assert_eq!(hit, 1);
    }
}
