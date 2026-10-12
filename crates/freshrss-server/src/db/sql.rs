//! Dynamic-SQL helpers.
//!
//! The entry-listing query and the BooleanSearch compiler both build `WHERE` clauses whose shape
//! is not known until runtime, so they need a heterogeneous bind-value list and a way to emit
//! `IN (?,?,…)`. rusqlite's `params![]` macro only takes a fixed argument list; this provides the
//! dynamic equivalent.

use rusqlite::{types::ToSqlOutput, types::ValueRef, Result as SqlResult, ToSql};

/// A bind value whose type is chosen at runtime.
///
/// SQLite is dynamically typed but rusqlite is not: to build one parameter list out of fragments
/// contributed by different parts of a query, every fragment has to agree on a type. This is that
/// agreement.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl ToSql for SqlValue {
    fn to_sql(&self) -> SqlResult<ToSqlOutput<'_>> {
        // `Borrowed` avoids cloning the text and blob on every bind.
        Ok(ToSqlOutput::Borrowed(match self {
            SqlValue::Null => ValueRef::Null,
            SqlValue::Int(v) => ValueRef::Integer(*v),
            SqlValue::Real(v) => ValueRef::Real(*v),
            // `ValueRef::Text` takes UTF-8 bytes, not a `&str`.
            SqlValue::Text(v) => ValueRef::Text(v.as_bytes()),
            SqlValue::Blob(v) => ValueRef::Blob(v.as_slice()),
        }))
    }
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

/// Emits `(?,?,?)` for `count` placeholders, or `(NULL)` for zero.
///
/// The zero case matters: `IN ()` is a syntax error in SQLite, and several call sites build an
/// `IN` list from a search that may name no ids at all. `IN (NULL)` is false for every row, which
/// is the correct answer for an empty id set.
pub fn in_clause(count: usize) -> String {
    if count == 0 {
        return "(NULL)".to_string();
    }
    let mut out = String::with_capacity(count * 2 + 2);
    out.push('(');
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        out.push('?');
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

/// Escapes the `%`, `_` and `\` characters so a user-supplied string can be embedded in a `LIKE`
/// pattern as a literal.
///
/// Only useful with a matching `ESCAPE '\'` clause, or with [`crate::db::udf`]'s `frss_ilike`,
/// which honours backslash escapes the way MySQL's `LIKE` does by default.
pub fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Wraps an escaped literal as a `%…%` substring pattern.
pub fn contains_pattern(text: &str) -> String {
    format!("%{}%", escape_like(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn in_clause_emits_the_right_number_of_placeholders() {
        assert_eq!(in_clause(0), "(NULL)");
        assert_eq!(in_clause(1), "(?)");
        assert_eq!(in_clause(3), "(?,?,?)");
    }

    #[test]
    fn an_empty_in_clause_is_false_for_every_row_rather_than_a_syntax_error() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1),(2);",
        )
        .expect("seed");
        let sql = format!("SELECT COUNT(*) FROM t WHERE id IN {}", in_clause(0));
        let count: i64 = conn.query_row(&sql, [], |r| r.get(0)).expect("query");
        assert_eq!(count, 0);
    }

    #[test]
    fn sql_values_bind_to_the_right_sqlite_types() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n TEXT);")
            .expect("create");

        let values = [
            SqlValue::from(42i64),
            SqlValue::from(1.5f64),
            SqlValue::from("hello"),
            SqlValue::from(vec![1u8, 2, 3]),
            SqlValue::from(None::<i64>),
        ];
        conn.execute(
            "INSERT INTO t VALUES (?,?,?,?,?)",
            rusqlite::params_from_iter(values.iter()),
        )
        .expect("insert");

        let (i, r, s, b, n): (i64, f64, String, Vec<u8>, Option<String>) = conn
            .query_row("SELECT i, r, s, b, n FROM t", [], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("select");
        assert_eq!(i, 42);
        assert_eq!(r, 1.5);
        assert_eq!(s, "hello");
        assert_eq!(b, vec![1, 2, 3]);
        assert_eq!(n, None);
    }

    #[test]
    fn bool_binds_as_an_integer_matching_the_schema() {
        // is_read / is_favorite are INTEGER columns; `true` must land as 1, not as text.
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (flag INTEGER);")
            .expect("create");
        conn.execute(
            "INSERT INTO t VALUES (?)",
            rusqlite::params![SqlValue::from(true)],
        )
        .expect("insert");
        let flag: i64 = conn
            .query_row("SELECT flag FROM t", [], |r| r.get(0))
            .expect("select");
        assert_eq!(flag, 1);
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

    #[test]
    fn escape_like_neutralizes_wildcards() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("100%"), r"100\%");
        assert_eq!(escape_like("a_b"), r"a\_b");
        assert_eq!(escape_like(r"a\b"), r"a\\b");
        assert_eq!(escape_like("%_%"), r"\%\_\%");
    }

    #[test]
    fn contains_pattern_wraps_an_escaped_literal() {
        assert_eq!(contains_pattern("rust"), "%rust%");
        assert_eq!(contains_pattern("50%"), r"%50\%%");
    }

    #[test]
    fn an_escaped_pattern_matches_literally_in_sqlite() {
        let conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("CREATE TABLE t (s TEXT); INSERT INTO t VALUES ('50%'),('500');")
            .expect("seed");
        let pattern = contains_pattern("50%");
        let escaped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM t WHERE s LIKE ? ESCAPE '\\'",
                rusqlite::params![pattern],
                |r| r.get(0),
            )
            .expect("escaped");
        assert_eq!(escaped, 1, "only the literal '50%' row");

        let unescaped: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE s LIKE '%50%%'", [], |r| {
                r.get(0)
            })
            .expect("unescaped");
        assert_eq!(unescaped, 2, "without escaping the % is a wildcard");
    }
}
