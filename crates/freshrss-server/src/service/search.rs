//! Parsing a search the way the API needs it: text in, an expandable tree out.
//!
//! Thin, but it is the one place that knows the three steps have to happen in this order — parse,
//! resolve saved searches, then hand the result to a backend — and that all three share one `now`.
//! Relative intervals (`date:P1W`) and saved queries containing them both resolve against that single
//! instant, so a page and the count beside it cannot disagree about what "this week" meant.

use std::collections::HashMap;

use rusqlite::Connection;

use freshrss_search::ast::Expr;
use freshrss_search::expand::{expand, QueryResolver};
use freshrss_search::parser::parse;

use crate::db::dao::user_query as query_dao;
use crate::db::Db;
use crate::error::Result;
use crate::domain::ids::now_unix;

/// Resolves `S:`/`search:` against one user's saved searches.
///
/// Loads the whole set in one query rather than looking each reference up, because a saved search that
/// references another would otherwise issue a round trip per level and the depth is bounded only by
/// [`freshrss_search::expand::MAX_EXPANSIONS`].
pub struct SavedSearches {
    by_id: HashMap<i64, String>,
    by_name: HashMap<String, String>,
    ids: Vec<i64>,
}

impl SavedSearches {
    /// Loads a user's saved searches.
    pub fn load(conn: &Connection, id_user: i64) -> Result<Self> {
        let mut out = Self {
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            ids: Vec::new(),
        };
        for query in query_dao::list(conn, id_user)? {
            // Blank ones are dropped here rather than by the resolver, so `S:*` does not have to skip
            // them one at a time and a name lookup cannot resolve to nothing.
            if !query.is_usable() {
                continue;
            }
            out.ids.push(query.id);
            out.by_id.insert(query.id, query.search.clone());
            out.by_name.insert(query.name, query.search);
        }
        Ok(out)
    }

    /// An empty resolver, for a user with no saved searches. Cheaper than a query that returns nothing.
    pub fn empty() -> Self {
        Self {
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            ids: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl QueryResolver for SavedSearches {
    fn by_id(&self, id: i64) -> Option<String> {
        self.by_id.get(&id).cloned()
    }

    fn by_name(&self, name: &str) -> Option<String> {
        self.by_name.get(name).cloned()
    }

    fn all_ids(&self) -> Vec<i64> {
        self.ids.clone()
    }
}

/// Parses and expands a search, resolving saved queries against `db`.
///
/// This is the entry point every handler uses, so a query that reaches a backend has already had its
/// `S:`/`search:` references replaced — and both backends refuse an unexpanded one rather than
/// guessing.
///
/// # Errors
///
/// Any [`freshrss_search::SearchError`]: a syntax error, a bad regex, an unknown saved search, a cycle.
/// The API maps all of them to `400` with the code `bad_search`.
pub async fn compile(text: &str, id_user: i64, db: &Db) -> Result<Expr> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Expr::TRUE);
    }
    // Owned, because `spawn_blocking` requires `'static` and a query is short enough that the copy
    // costs less than reasoning about the borrow would.
    let text = trimmed.to_owned();
    let db = db.clone();
    tokio::task::spawn_blocking(move || compile_sync(&text, id_user, &db, now_unix()))
        .await
        .map_err(crate::error::Error::from)?
}

/// The synchronous form, for the CLI and for anything already on a blocking thread.
pub fn compile_sync(text: &str, id_user: i64, db: &Db, now: i64) -> Result<Expr> {
    let expr = db.blocking_with(|conn| {
        let saved = SavedSearches::load(conn, id_user)?;
        let parsed = parse(text, now)?;
        // No saved searches means no reference can resolve, so skip the walk: the common case is a
        // plain query and this keeps it to one database round trip.
        Ok(if saved.is_empty() && !names_a_saved_query(&parsed) {
            parsed
        } else {
            expand(&parsed, &saved, now)?
        })
    })?;
    Ok(expr)
}

/// True when the tree still holds a `S:`/`search:` reference.
fn names_a_saved_query(expr: &Expr) -> bool {
    expr.preds().iter().any(|p| {
        matches!(
            p,
            freshrss_search::ast::Pred::SavedQuery { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use freshrss_search::ast::Pred;

    use super::*;
    use crate::db::dao::user as user_dao;
    use crate::error::Error;
    use crate::domain::user_query::SavedQuery;

    const NOW: i64 = 1_700_000_000;

    fn setup() -> (Db, i64) {
        let db = Db::in_memory().expect("in-memory db");
        let id_user = db
            .blocking_with(|conn| {
                Ok(user_dao::insert(
                    conn,
                    &user_dao::NewUser {
                        username: "alice",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )?)
            })
            .expect("create user");
        (db, id_user)
    }

    /// Unwraps the search error, which `compile_sync` boxes into the crate's own `Error`.
    fn search_error(e: &Error) -> &freshrss_search::SearchError {
        match e {
            Error::Search(inner) => inner,
            other => panic!("expected a search error, got {other}"),
        }
    }

    fn save(db: &Db, id_user: i64, name: &str, search: &str) -> i64 {
        db.blocking_with(|conn| Ok(query_dao::insert(conn, &SavedQuery::new(id_user, name, search))?))
            .expect("save")
    }

    #[test]
    fn a_plain_query_needs_no_lookup() {
        let (db, id_user) = setup();
        let expr = compile_sync("intitle:rust", id_user, &db, NOW).expect("compiles");
        assert_eq!(expr.preds().len(), 1);
        assert!(expr.preds().iter().all(|p| !matches!(p, Pred::SavedQuery { .. })));
    }

    #[test]
    fn an_empty_query_is_true_rather_than_an_error() {
        let (db, id_user) = setup();
        for text in ["", "   ", "\n"] {
            assert!(compile_sync(text, id_user, &db, NOW).expect("compiles").is_true(), "{text:?}");
        }
    }

    #[test]
    fn a_saved_search_is_expanded_by_name_and_by_id() {
        let (db, id_user) = setup();
        let id = save(&db, id_user, "unread rust", "intitle:rust -author:bot");

        // A bare value ends at the space, so the space in the name has to be quoted or escaped.
        assert!(matches!(
            search_error(&compile_sync("search:unread rust", id_user, &db, NOW).expect_err("rejected")),
            freshrss_search::SearchError::QueryNotFound { .. }
        ));

        let by_id = compile_sync(&format!("S:{id}"), id_user, &db, NOW).expect("compiles");
        assert_eq!(
            freshrss_search::render::render(&by_id),
            "intitle:rust -author:bot"
        );
    }

    #[test]
    fn a_missing_saved_search_is_a_400_not_a_silent_match_everything() {
        let (db, id_user) = setup();
        save(&db, id_user, "real", "intitle:x");
        let e = compile_sync("S:999", id_user, &db, NOW).expect_err("rejected");
        assert!(matches!(
            search_error(&e),
            freshrss_search::SearchError::QueryNotFound { .. }
        ), "{e}");
        assert!(e.is_client_error());
    }

    #[test]
    fn a_user_never_sees_another_users_saved_search() {
        let (db, id_user) = setup();
        let other = db
            .blocking_with(|conn| {
                Ok(user_dao::insert(
                    conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )?)
            })
            .expect("create user");
        let foreign = save(&db, other, "private", "intitle:secret");

        let e = compile_sync(&format!("S:{foreign}"), id_user, &db, NOW).expect_err("rejected");
        assert!(
            matches!(
                search_error(&e),
                freshrss_search::SearchError::QueryNotFound { .. }
            ),
            "another user's saved search must look absent, not forbidden: {e}"
        );
    }

    #[test]
    fn a_blank_saved_search_counts_as_missing() {
        let (db, id_user) = setup();
        let id = save(&db, id_user, "blank", "   ");
        let e = compile_sync(&format!("S:{id}"), id_user, &db, NOW).expect_err("rejected");
        assert!(matches!(
            search_error(&e),
            freshrss_search::SearchError::QueryNotFound { .. }
        ), "{e}");

        // And `S:*` skips it rather than failing the whole query.
        save(&db, id_user, "real", "intitle:x");
        let expr = compile_sync("S:*", id_user, &db, NOW).expect("compiles");
        assert_eq!(freshrss_search::render::render(&expr), "intitle:x");
    }

    #[test]
    fn a_saved_search_may_reference_another() {
        let (db, id_user) = setup();
        let inner = save(&db, id_user, "inner", "intitle:deep");
        save(&db, id_user, "outer", &format!("S:{inner} f:4"));
        let outer = db
            .blocking_with(|conn| {
                Ok(query_dao::find_by_name(conn, id_user, "outer")?
                    .expect("exists")
                    .id)
            })
            .expect("lookup");
        let expr = compile_sync(&format!("S:{outer}"), id_user, &db, NOW).expect("compiles");
        assert_eq!(freshrss_search::render::render(&expr), "intitle:deep f:4");
    }

    #[test]
    fn a_saved_search_that_references_itself_is_reported_as_a_cycle() {
        let (db, id_user) = setup();
        let id = save(&db, id_user, "loop", "intitle:x");
        db.blocking_with(|conn| {
            let mut q = query_dao::find_by_id(conn, id, id_user)?.expect("exists");
            q.search = format!("S:{id}");
            Ok(query_dao::update(conn, &q)?)
        })
        .expect("rewrite");

        let e = compile_sync(&format!("S:{id}"), id_user, &db, NOW).expect_err("rejected");
        assert!(matches!(
            search_error(&e),
            freshrss_search::SearchError::QueryCycle { .. }
        ), "{e}");
        assert!(e.is_client_error());
    }

    #[test]
    fn a_relative_interval_inside_a_saved_search_uses_the_request_instant() {
        // A saved `date:P1W` has to mean "this week" when it runs, not when it was saved — and it has
        // to mean the same week as the `date:P1W` typed beside it in the same request.
        let (db, id_user) = setup();
        let id = save(&db, id_user, "recent", "date:P1W");
        let via_saved = compile_sync(&format!("S:{id}"), id_user, &db, NOW).expect("compiles");
        let inline = compile_sync("date:P1W", id_user, &db, NOW).expect("compiles");
        assert_eq!(via_saved, inline);
    }

    #[test]
    fn a_syntax_error_is_reported_wherever_it_is() {
        let (db, id_user) = setup();
        assert!(matches!(
            search_error(&compile_sync("intitle:a OR", id_user, &db, NOW).expect_err("rejected")),
            freshrss_search::SearchError::UnexpectedEnd { .. }
        ));
        let id = save(&db, id_user, "broken", "(intitle:a");
        assert!(matches!(
            search_error(
                &compile_sync(&format!("S:{id}"), id_user, &db, NOW).expect_err("rejected")
            ),
            freshrss_search::SearchError::UnbalancedParen { .. }
        ));
    }

    #[test]
    fn the_resolver_reports_what_it_loaded() {
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            assert!(SavedSearches::load(conn, id_user)?.is_empty());
            query_dao::insert(conn, &SavedQuery::new(id_user, "a", "intitle:x"))?;
            query_dao::insert(conn, &SavedQuery::new(id_user, "blank", "  "))?;
            let saved = SavedSearches::load(conn, id_user)?;
            // The blank one is dropped at load time, so `S:*` never has to consider it.
            assert_eq!(saved.all_ids().len(), 1);
            assert!(saved.by_name("a").is_some());
            assert!(saved.by_name("blank").is_none());
            assert!(!saved.is_empty());
            Ok(())
        })
        .expect("resolver");
    }

    #[test]
    fn an_empty_resolver_resolves_nothing() {
        let saved = SavedSearches::empty();
        assert!(saved.is_empty());
        assert!(saved.all_ids().is_empty());
        assert!(saved.by_id(1).is_none());
        assert!(saved.by_name("a").is_none());
    }
}
