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
//!
//! Statements whose shape depends on a runtime value — an `IN` list, the entry listing — are
//! assembled with [`sqlx::QueryBuilder`] rather than by formatting a string, so a value can only
//! ever reach the database as a bound parameter. [`push_in_list`] carries the one rule that has to
//! hold across such a statement on SQLite.

pub mod category;
pub mod entry;
pub mod entry_query;
pub mod entry_retention;
pub mod favicon;
pub mod feed;
pub mod meta;
pub mod session;
pub mod tag;
pub mod user;
pub mod user_query;

use sqlx::sqlite::SqliteRow;
use sqlx::{Encode, QueryBuilder, Row as _, Sqlite, Type};

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

/// SQLite's `SQLITE_MAX_VARIABLE_NUMBER` defaults to 32766 on modern builds, but FreshRSS chunks at
/// 998 to stay safe on older ones. The same conservative bound is kept here so a statement never
/// fails on a deployment nobody tested.
pub(super) const MAX_BIND_PARAMS: usize = 998;

/// Splits `items` into chunks small enough to bind in one statement, leaving room for the `extra`
/// parameters the rest of that statement binds.
pub(super) fn chunks<T>(items: &[T], extra: usize) -> impl Iterator<Item = &[T]> {
    items.chunks(MAX_BIND_PARAMS.saturating_sub(extra).max(1))
}

/// Appends `IN (?,?,…)` to `builder`, binding one value per element.
///
/// Every placeholder [`QueryBuilder`] emits is anonymous, and sqlx's SQLite driver binds an
/// anonymous `?` by counting the anonymous placeholders before it. A statement must therefore be
/// *all* anonymous: numbering one parameter `?1` while leaving the rest `?` restarts that count at
/// the first bound value and silently binds the wrong ones. Building the list here, one `push_bind`
/// per element, is what keeps text order and value order the same order.
///
/// An empty iterator emits `IN (NULL)`, which is false for every row: `IN ()` is a syntax error in
/// SQLite, and an empty id set quietly becoming "every id" would be worse.
pub(super) fn push_in_list<'q, T, I>(builder: &mut QueryBuilder<Sqlite>, values: I)
where
    T: Encode<'q, Sqlite> + Type<Sqlite>,
    I: IntoIterator<Item = T>,
{
    let mut values = values.into_iter().peekable();
    builder.push("IN (");
    if values.peek().is_none() {
        builder.push("NULL");
    } else {
        let mut list = builder.separated(",");
        for value in values {
            list.push_bind(value);
        }
    }
    builder.push(")");
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    /// A bare one-connection `:memory:` database. Deliberately not [`crate::db::Db`]: these tests
    /// are about the helpers, and a migrated schema would only get in the way.
    async fn db() -> SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory db")
    }

    #[test]
    fn chunks_respects_the_bind_limit() {
        let items: Vec<i64> = (0..(MAX_BIND_PARAMS * 2 + 5) as i64).collect();
        let parts: Vec<usize> = chunks(&items, 1).map(<[i64]>::len).collect();
        // Derived rather than written out: a literal here rots the moment the bound moves, which is
        // exactly how it came to disagree with the slice it was checking.
        let per_chunk = MAX_BIND_PARAMS - 1;
        let mut expected = vec![per_chunk; items.len() / per_chunk];
        if items.len() % per_chunk > 0 {
            expected.push(items.len() % per_chunk);
        }
        assert_eq!(parts, expected);
        // The chunk plus the parameters beside it stay under the limit.
        assert!(parts.iter().all(|n| *n < MAX_BIND_PARAMS));
        assert_eq!(parts.iter().sum::<usize>(), items.len());
    }

    #[test]
    fn chunks_of_an_empty_slice_yields_nothing() {
        assert_eq!(chunks::<i64>(&[], 0).count(), 0);
    }

    #[tokio::test]
    async fn an_empty_list_is_false_for_every_row_rather_than_a_syntax_error() {
        let pool = db().await;
        sqlx::raw_sql("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1),(2);")
            .execute(&pool)
            .await
            .expect("seed");
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM t WHERE id ");
        push_in_list(&mut builder, Vec::<i64>::new());
        let count: i64 = builder
            .build_query_scalar()
            .fetch_one(&pool)
            .await
            .expect("query");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn a_list_binds_the_values_that_follow_it() {
        // The reason a statement is all-anonymous or all-numbered and never mixed: a bare `?` after a
        // `?1` is bound from the start of the value list again, and the row would quietly not match.
        let pool = db().await;
        sqlx::raw_sql(
            "CREATE TABLE t (a INTEGER, b INTEGER); INSERT INTO t VALUES (10,20),(11,21);",
        )
        .execute(&pool)
        .await
        .expect("seed");
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM t WHERE a = ");
        builder.push_bind(10i64).push(" AND b ");
        push_in_list(&mut builder, [99i64, 20]);
        let count: i64 = builder
            .build_query_scalar()
            .fetch_one(&pool)
            .await
            .expect("query");
        assert_eq!(count, 1, "the 99 matches nothing and the 20 does");
    }
}
