//! Data access.
//!
//! Every function here is synchronous and takes a `&Connection`. The only async boundary in the
//! crate is [`crate::db::Db::with`], so these are plain functions: unit-testable with an in-memory
//! database, and free to compose into transactions because a transaction *is* a `&Connection`.
//!
//! Every query that reads owned rows is scoped by `id_user`. Sharing one database between users is
//! a divergence from FreshRSS (which gives each user their own file), so isolation is an invariant
//! this module has to enforce rather than one the filesystem gives us for free.

pub mod category;
pub mod entry;
pub mod entry_query;
pub mod feed;
pub mod meta;
pub mod session;
pub mod tag;
pub mod user;

use rusqlite::Row;

use crate::domain::attributes::Attributes;

/// Reads an `attributes` column, tolerating NULL.
pub fn row_attributes(row: &Row<'_>, index: usize) -> rusqlite::Result<Attributes> {
    let raw: Option<String> = row.get(index)?;
    Ok(Attributes::from_db(raw.as_deref()))
}

/// Reads a nullable INTEGER column as `Option<i64>`, treating SQLite's NULL as `None`.
pub fn row_opt_i64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<i64>> {
    row.get(index)
}
