//! SQLite scalar functions registered on every pooled connection.
//!
//! These exist so the SQL emitted by [`freshrss_search`] and its in-memory matcher share one
//! definition of "these two strings match". SQLite's built-in `LIKE` is case-insensitive for
//! ASCII only and not accent-insensitive at all, which is why FreshRSS's SQL path and its
//! `DatabaseDAO::strilike` matcher disagree.
//!
//! Argument order is `(subject, pattern)` so the SQL reads naturally:
//! `WHERE frss_ilike(e.title, ?)`.
//!
//! Three functions, one per comparison the search grammar needs:
//!
//! | function | meaning | used by |
//! |---|---|---|
//! | `frss_ilike(a, b)` | `b` occurs inside `a`, ignoring case and accents | `intitle:` `intext:` `author:` `inurl:` free text `#tag` |
//! | `frss_ieq(a, b)` | `a` and `b` are equal, ignoring case and accents | `labels:` |
//! | `frss_regexp(a, b)` | `b` matches inside `a`, as written | every operator's `/pattern/im` form |
//!
//! `frss_ilike`'s pattern is a **literal**: `%` and `_` are not wildcards. A user typing `100%` into
//! the search box means those three characters, and the way to express a pattern is a regex.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use fancy_regex::Regex;
use rusqlite::{functions::FunctionFlags, Connection, Result};

/// Compiled-pattern cache ceiling, per thread.
const REGEX_CACHE_LIMIT: usize = 256;

thread_local! {
    static REGEX_CACHE: RefCell<Lru> = RefCell::new(Lru::default());
}

#[derive(Default)]
struct Lru {
    map: HashMap<String, Regex>,
    order: VecDeque<String>,
}

impl Lru {
    fn get_or_compile(&mut self, pattern: &str) -> Option<&Regex> {
        if !self.map.contains_key(pattern) {
            let compiled = Regex::new(pattern).ok()?;
            if self.map.len() >= REGEX_CACHE_LIMIT {
                if let Some(evicted) = self.order.pop_front() {
                    self.map.remove(&evicted);
                }
            }
            self.order.push_back(pattern.to_string());
            self.map.insert(pattern.to_string(), compiled);
        } else if let Some(pos) = self.order.iter().position(|p| p == pattern) {
            // Refresh recency so hot patterns survive eviction.
            let p = self.order.remove(pos).expect("position came from order");
            self.order.push_back(p);
        }
        self.map.get(pattern)
    }
}

/// Matches `text` against the regular expression `pattern`.
///
/// `pattern` is expected to already be translated out of FreshRSS's `/body/flags` form: the
/// SQL compiler strips the delimiters and inlines `i`/`m` as `(?i)`/`(?m)` before binding, so
/// this function stays dumb. Total by design — an uncompilable pattern yields `false` rather
/// than aborting the whole query, because a `SQLITE_ERROR` from a UDF kills the statement.
fn regexp_match(text: &str, pattern: &str) -> bool {
    REGEX_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .get_or_compile(pattern)
            .is_some_and(|re| re.is_match(text).unwrap_or(false))
    })
}

/// Registers every function on `conn`. Called from the pool's `with_init` hook, so a
/// connection handed out by [`crate::db::Db`] always has them.
pub fn register(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    conn.create_scalar_function("frss_ilike", 2, flags, |ctx| {
        let haystack: Option<String> = ctx.get(0)?;
        let needle: Option<String> = ctx.get(1)?;
        Ok(match (haystack, needle) {
            (Some(h), Some(n)) => freshrss_search::ilike(&h, &n),
            // SQL NULL semantics: a comparison against NULL is not true.
            _ => false,
        })
    })?;

    conn.create_scalar_function("frss_ieq", 2, flags, |ctx| {
        let left: Option<String> = ctx.get(0)?;
        let right: Option<String> = ctx.get(1)?;
        Ok(match (left, right) {
            (Some(a), Some(b)) => freshrss_search::ieq(&a, &b),
            _ => false,
        })
    })?;

    conn.create_scalar_function("frss_regexp", 2, flags, |ctx| {
        let text: Option<String> = ctx.get(0)?;
        let pattern: Option<String> = ctx.get(1)?;
        Ok(match (text, pattern) {
            (Some(t), Some(p)) => regexp_match(&t, &p),
            _ => false,
        })
    })?;

    conn.create_scalar_function("frss_norm", 1, flags, |ctx| {
        let text: Option<String> = ctx.get(0)?;
        Ok(text.map(|t| freshrss_search::fold(&t)).unwrap_or_default())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("in-memory db");
        register(&c).expect("udfs register");
        c
    }

    fn scalar_i64(c: &Connection, sql: &str) -> i64 {
        c.query_row(sql, [], |r| r.get::<_, i64>(0)).expect("query")
    }

    #[test]
    fn ilike_is_accent_and_case_insensitive() {
        let c = conn();
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('École Normale', 'ecole')"), 1);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('CAFE', 'café')"), 1);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('lycée', 'college')"), 0);
    }

    #[test]
    fn ilike_treats_its_needle_as_a_literal() {
        let c = conn();
        // The two characters a user is most likely to type without meaning them as wildcards. SQLite's
        // own LIKE would answer 1 to both.
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('axb', 'a_b')"), 0);
        assert_eq!(scalar_i64(&c, "SELECT 'axb' LIKE 'a_b'"), 1);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('100 things', '100%')"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('100% sure', '100%')"), 1);
    }

    #[test]
    fn ieq_compares_whole_values() {
        let c = conn();
        assert_eq!(scalar_i64(&c, "SELECT frss_ieq('Étiquette', 'etiquette')"), 1);
        // The reason labels do not use frss_ilike: `labels:todo` must not match a label "todos".
        assert_eq!(scalar_i64(&c, "SELECT frss_ieq('todos', 'todo')"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('todos', 'todo')"), 1);
    }

    #[test]
    fn sqlite_like_is_not_accent_insensitive_but_frss_ilike_is() {
        let c = conn();
        // The exact divergence this module exists to remove.
        assert_eq!(scalar_i64(&c, "SELECT 'École' LIKE '%ecole%'"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('École', 'ecole')"), 1);
    }

    #[test]
    fn null_arguments_are_not_true() {
        let c = conn();
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike(NULL, 'x')"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ilike('x', NULL)"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ieq(NULL, 'x')"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_ieq('x', NULL)"), 0);
        assert_eq!(scalar_i64(&c, "SELECT frss_regexp(NULL, 'x')"), 0);
    }

    #[test]
    fn regexp_matches_translated_patterns() {
        let c = conn();
        // What `Pattern::inline()` produces for `/rus|rust/i`.
        assert_eq!(
            scalar_i64(&c, "SELECT frss_regexp('Rustlang', '(?i)rus|rust')"),
            1
        );
        assert_eq!(
            scalar_i64(&c, "SELECT frss_regexp('python', '(?i)rus|rust')"),
            0
        );
        assert_eq!(scalar_i64(&c, "SELECT frss_regexp('aXb', 'a.b')"), 1);
    }

    #[test]
    fn regexp_sees_the_stored_text_with_its_accents_intact() {
        let c = conn();
        // A regex is not folded on either side, so the pattern spells its own accents and `/i` is
        // what makes them case-insensitive.
        assert_eq!(
            scalar_i64(&c, "SELECT frss_regexp('l''École', '(?i)école')"),
            1
        );
        assert_eq!(
            scalar_i64(&c, "SELECT frss_regexp('l''École', 'école')"),
            0,
            "without /i the uppercase É does not match"
        );
        assert_eq!(
            scalar_i64(&c, "SELECT frss_regexp('l''École', '(?i)ecole')"),
            0,
            "…and a folded spelling never matches an accented article"
        );
    }

    #[test]
    fn multiline_flags_let_anchors_work_per_tag() {
        let c = conn();
        // The tag column rewritten the way `tag_condition` rewrites it: `#rust #rss` becomes one tag
        // per line, so `^` and `$` bound a single tag.
        let rewritten = "REPLACE(REPLACE('#rust #rss', ' #', '#'), '#', CHAR(10))";
        assert_eq!(
            scalar_i64(&c, &format!("SELECT frss_regexp({rewritten}, '(?m)^rss$')")),
            1
        );
        assert_eq!(
            scalar_i64(&c, &format!("SELECT frss_regexp({rewritten}, '(?m)^rs$')")),
            0
        );
        assert_eq!(
            scalar_i64(&c, &format!("SELECT frss_regexp({rewritten}, '^rss$')")),
            0,
            "without (?m) the anchors mean the whole rewritten string"
        );
    }

    #[test]
    fn regexp_supports_lookahead_which_the_regex_crate_does_not() {
        let c = conn();
        assert_eq!(
            scalar_i64(&c, r"SELECT frss_regexp('rustlang', 'rust(?=lang)')"),
            1
        );
        assert_eq!(
            scalar_i64(&c, r"SELECT frss_regexp('rustc', 'rust(?=lang)')"),
            0
        );
    }

    #[test]
    fn invalid_pattern_is_false_not_an_error() {
        let c = conn();
        assert_eq!(scalar_i64(&c, "SELECT frss_regexp('abc', '([')"), 0);
    }

    #[test]
    fn norm_exposes_the_fold() {
        let c = conn();
        let out: String = c
            .query_row("SELECT frss_norm('Ångström Café')", [], |r| r.get(0))
            .expect("query");
        assert_eq!(out, "angstrom cafe");
    }

    #[test]
    fn works_inside_a_where_clause_over_a_table() {
        let c = conn();
        c.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO t (title) VALUES ('École'), ('ecole'), ('lycee'), ('Café');",
        )
        .expect("seed");
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM t WHERE frss_ilike(title, 'ecole')",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(n, 2, "'École' and 'ecole' must both match 'ecole'");
    }

    #[test]
    fn regex_cache_evicts_without_leaking() {
        let mut lru = Lru::default();
        for i in 0..(REGEX_CACHE_LIMIT + 50) {
            let pattern = format!("pat{i}");
            assert!(lru.get_or_compile(&pattern).is_some());
        }
        assert!(lru.map.len() <= REGEX_CACHE_LIMIT);
    }

    #[test]
    fn regex_cache_rejects_invalid_patterns_without_inserting() {
        let mut lru = Lru::default();
        assert!(lru.get_or_compile("([").is_none());
        assert!(lru.map.is_empty());
    }
}
