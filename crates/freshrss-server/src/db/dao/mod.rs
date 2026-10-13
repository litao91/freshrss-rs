//! Data access.
//!
//! Every function here is `async` and takes `&mut SqliteConnection`. The only place a connection is
//! acquired from the pool is [`crate::db::Db::with`], so these otherwise read like ordinary
//! functions: unit-testable against an in-memory database, and free to compose into a transaction
//! because `conn.begin().await` yields something that derefs to the same `&mut SqliteConnection`.
//!
//! Rows are read by column name, never by position. The wide `SELECT` lists these modules assemble
//! have been reordered before and will be again; a positional read turns that into a silently
//! wrong value rather than a decode error naming the column.
//!
//! Every query that reads owned rows is scoped by `id_user`. Sharing one database between users is
//! a divergence from FreshRSS (which gives each user their own file), so isolation is an invariant
//! this module has to enforce rather than one the filesystem gives us for free.

pub mod category;
pub mod entry;
pub mod entry_query;
pub mod entry_retention;
pub mod feed;
pub mod meta;
pub mod session;
pub mod tag;
pub mod user;
pub mod user_query;

use sqlx::sqlite::SqliteRow;
use sqlx::Row as _;

use crate::domain::attributes::Attributes;

/// Reads an `attributes` column, tolerating NULL.
///
/// # Errors
///
/// Propagates a missing or wrongly-typed column rather than panicking on it, and returns `sqlx`'s
/// error type because every row mapper it is called from already does. Every caller selects the
/// column, so this is a programming error when it happens — but one that has to arrive as a 500 in
/// the JSON envelope, not as a panic that takes the connection task down with it.
pub fn row_attributes(row: &SqliteRow, column: &str) -> Result<Attributes, sqlx::Error> {
    Ok(Attributes::from_db(
        row.try_get::<Option<String>, _>(column)?.as_deref(),
    ))
}
