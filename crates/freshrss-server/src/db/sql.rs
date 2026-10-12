//! Dynamic-SQL helpers.
//!
//! The entry-listing query and the BooleanSearch compiler both build `WHERE` clauses whose shape
//! is not known until runtime, so they need a heterogeneous bind-value list and a way to emit
//! `IN (?,?,…)`. sqlx's `.bind()` takes one statically-typed value at a time; this is the dynamic
//! equivalent.

use sqlx::sqlite::SqliteArguments;
use sqlx::{AssertSqlSafe, Sqlite};

/// A statement assembled at runtime, with its parameters not yet bound.
pub type Statement<'q> = sqlx::query::Query<'q, Sqlite, SqliteArguments>;

/// A bind value whose type is chosen at runtime.
///
/// SQLite is dynamically typed but a `.bind()` call is not: to build one parameter list out of
/// fragments contributed by different parts of a query, every fragment has to agree on a type.
/// This is that agreement.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// Appends this value to a statement's parameter list.
    ///
    /// A `match` rather than a `sqlx::Encode` impl: sqlx keeps its SQLite argument buffer private
    /// to the driver crate, so encoding cannot be implemented from outside it.
    fn bind<'q>(self, statement: Statement<'q>) -> Statement<'q> {
        match self {
            // `NULL` on its own has no type, so it is bound as an absent integer; SQLite stores
            // and compares it the same way whichever typed NULL produced it.
            SqlValue::Null => statement.bind(None::<i64>),
            SqlValue::Int(v) => statement.bind(v),
            SqlValue::Real(v) => statement.bind(v),
            SqlValue::Text(v) => statement.bind(v),
            SqlValue::Blob(v) => statement.bind(v),
        }
    }
}

/// Binds `params` onto `statement`, in order.
pub fn bind_all<'q>(statement: Statement<'q>, params: Vec<SqlValue>) -> Statement<'q> {
    params
        .into_iter()
        .fold(statement, |statement, value| value.bind(statement))
}

/// Declares a dynamically assembled statement safe to run.
///
/// sqlx refuses a `String` as SQL text unless the caller asserts it interpolates nothing
/// untrusted. Every statement built here is assembled from `const` column lists and fixed clause
/// fragments, with user-supplied values bound as parameters, so the assertion is honest.
pub fn sql(text: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(text)
}

impl From<i64> for SqlValue {
    fn from(v: i64) -> Self {
        SqlValue::Int(v)
    }
}

impl From<i32> for SqlValue {
    fn from(v: i32) -> Self {
        SqlValue::Int(i64::from(v))
    }
}

impl From<f64> for SqlValue {
    fn from(v: f64) -> Self {
        SqlValue::Real(v)
    }
}

impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        SqlValue::Text(v)
    }
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        SqlValue::Text(v.to_string())
    }
}

impl From<bool> for SqlValue {
    fn from(v: bool) -> Self {
        SqlValue::Int(i64::from(v))
    }
}

impl From<Vec<u8>> for SqlValue {
    fn from(v: Vec<u8>) -> Self {
        SqlValue::Blob(v)
    }
}

impl<T: Into<SqlValue>> From<Option<T>> for SqlValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => SqlValue::Null,
        }
    }
}

impl From<&freshrss_search::sql::Param> for SqlValue {
    fn from(param: &freshrss_search::sql::Param) -> Self {
        use freshrss_search::sql::Param;
        match param {
            Param::Text(t) => SqlValue::Text(t.clone()),
            Param::Int(i) => SqlValue::Int(*i),
            Param::Null => SqlValue::Null,
        }
    }
}

/// Emits `(?first,…,?first+count-1)` for `count` placeholders, or `(NULL)` for zero.
///
/// Numbered rather than bare `?`: sqlx resolves a bare placeholder by counting the anonymous ones
/// it has seen, so a statement that names its leading parameters `?1`…`?3` and then leaves an `IN`
/// list anonymous restarts that count at the first bound value and silently binds the wrong ones.
/// SQLite assigns bare placeholders exactly the numbers this emits, so the statements say what they
/// already meant.
///
/// The zero case matters: `IN ()` is a syntax error in SQLite, and several call sites build an
/// `IN` list from a search that may name no ids at all. `IN (NULL)` is false for every row, which
/// is the correct answer for an empty id set.
pub fn in_clause(first: usize, count: usize) -> String {
    if count == 0 {
        return "(NULL)".to_string();
    }
    let mut out = String::with_capacity(count * 5 + 2);
    out.push('(');
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        out.push('?');
        out.push_str(&(first + i).to_string());
    }
    out.push(')');
    out
}

/// SQLite's `SQLITE_MAX_VARIABLE_NUMBER` defaults to 32766 on modern builds, but FreshRSS chunks
/// at 998 to stay safe on older ones. We keep the same conservative bound so a query never fails
/// on a deployment we did not test.
pub const MAX_BIND_PARAMS: usize = 998;

/// Splits `items` into chunks small enough to bind as one `IN` list.
pub fn chunks<T>(items: &[T]) -> impl Iterator<Item = &[T]> {
    items.chunks(MAX_BIND_PARAMS.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Row, SqlitePool};

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
    fn in_clause_emits_the_right_number_of_placeholders() {
        assert_eq!(in_clause(1, 0), "(NULL)");
        assert_eq!(in_clause(1, 1), "(?1)");
        assert_eq!(in_clause(2, 3), "(?2,?3,?4)");
    }

    #[tokio::test]
    async fn an_empty_in_clause_is_false_for_every_row_rather_than_a_syntax_error() {
        let pool = db().await;
        sqlx::raw_sql("CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1),(2);")
            .execute(&pool)
            .await
            .expect("seed");
        let count: i64 = sqlx::query_scalar(sql(format!(
            "SELECT COUNT(*) FROM t WHERE id IN {}",
            in_clause(1, 0)
        )))
        .fetch_one(&pool)
        .await
        .expect("query");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn a_numbered_in_clause_binds_the_values_it_follows() {
        // The reason `in_clause` numbers its placeholders. A bare `?` after a `?1` would be bound
        // from the start of the value list again, and the row would quietly not match.
        let pool = db().await;
        sqlx::raw_sql(
            "CREATE TABLE t (a INTEGER, b INTEGER); INSERT INTO t VALUES (10,20),(11,21);",
        )
        .execute(&pool)
        .await
        .expect("seed");
        let statement = sql(format!(
            "SELECT COUNT(*) FROM t WHERE a = ?1 AND b IN {}",
            in_clause(2, 2)
        ));
        let count: i64 = sqlx::query_scalar(statement)
            .bind(10i64)
            .bind(99i64)
            .bind(20i64)
            .fetch_one(&pool)
            .await
            .expect("query");
        assert_eq!(count, 1, "the 99 matches nothing and the 20 does");
    }

    #[tokio::test]
    async fn sql_values_bind_to_the_right_sqlite_types() {
        let pool = db().await;
        sqlx::raw_sql("CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n TEXT);")
            .execute(&pool)
            .await
            .expect("create");

        let values = vec![
            SqlValue::from(42i64),
            SqlValue::from(1.5f64),
            SqlValue::from("hello"),
            SqlValue::from(vec![1u8, 2, 3]),
            SqlValue::from(None::<i64>),
        ];
        bind_all(sqlx::query("INSERT INTO t VALUES (?,?,?,?,?)"), values)
            .execute(&pool)
            .await
            .expect("insert");

        let row = sqlx::query("SELECT i, r, s, b, n FROM t")
            .fetch_one(&pool)
            .await
            .expect("select");
        assert_eq!(row.get::<i64, _>("i"), 42);
        assert_eq!(row.get::<f64, _>("r"), 1.5);
        assert_eq!(row.get::<String, _>("s"), "hello");
        assert_eq!(row.get::<Vec<u8>, _>("b"), vec![1, 2, 3]);
        assert_eq!(row.get::<Option<String>, _>("n"), None);
    }

    #[tokio::test]
    async fn bool_binds_as_an_integer_matching_the_schema() {
        // is_read / is_favorite are INTEGER columns; `true` must land as 1, not as text.
        let pool = db().await;
        sqlx::raw_sql("CREATE TABLE t (flag INTEGER);")
            .execute(&pool)
            .await
            .expect("create");
        bind_all(
            sqlx::query("INSERT INTO t VALUES (?)"),
            vec![SqlValue::from(true)],
        )
        .execute(&pool)
        .await
        .expect("insert");
        let flag: i64 = sqlx::query_scalar("SELECT flag FROM t")
            .fetch_one(&pool)
            .await
            .expect("select");
        assert_eq!(flag, 1);
    }

    #[tokio::test]
    async fn a_null_value_binds_as_null_not_as_a_typed_default() {
        // The search compiler emits `Param::Null` for an id set that resolved to nothing; binding it
        // as `0` or `''` would silently match a real row.
        let pool = db().await;
        sqlx::raw_sql("CREATE TABLE t (s TEXT); INSERT INTO t VALUES (NULL), ('x');")
            .execute(&pool)
            .await
            .expect("create");
        let count: i64 = bind_all(
            sqlx::query("SELECT COUNT(*) FROM t WHERE s IS ?"),
            vec![SqlValue::Null],
        )
        .fetch_one(&pool)
        .await
        .expect("count")
        .get(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn chunks_respects_the_bind_limit() {
        let items: Vec<i64> = (0..(MAX_BIND_PARAMS * 2 + 5) as i64).collect();
        let parts: Vec<usize> = chunks(&items).map(<[i64]>::len).collect();
        assert_eq!(parts, vec![998, 998, 5]);
        assert!(parts.iter().all(|n| *n <= MAX_BIND_PARAMS));
        assert_eq!(parts.iter().sum::<usize>(), items.len());
    }

    #[test]
    fn chunks_of_an_empty_slice_yields_nothing() {
        assert_eq!(chunks::<i64>(&[]).count(), 0);
    }
}
