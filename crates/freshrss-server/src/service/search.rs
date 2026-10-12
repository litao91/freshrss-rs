//! Parsing a search the way the API needs it: text in, an expandable tree out.
//!
//! Thin, but it is the one place that knows the three steps have to happen in this order — parse,
//! resolve saved searches, then hand the result to a backend — and that all three share one `now`.
//! Relative intervals (`date:P1W`) and saved queries containing them both resolve against that single
//! instant, so a page and the count beside it cannot disagree about what "this week" meant.

use std::collections::HashMap;

use sqlx::SqliteConnection;

use freshrss_search::ast::Expr;
use freshrss_search::expand::{expand, QueryResolver};
use freshrss_search::parser::parse;

use crate::db::dao::user_query as query_dao;
use crate::db::Db;
use crate::domain::ids::now_unix;
use crate::error::Result;

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
    pub async fn load(conn: &mut SqliteConnection, id_user: i64) -> Result<Self> {
        let mut out = Self {
            by_id: HashMap::new(),
            by_name: HashMap::new(),
            ids: Vec::new(),
        };
        for query in query_dao::list(conn, id_user).await? {
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
    compile_at(text, id_user, db, now_unix()).await
}

/// [`compile`] against a caller-supplied instant.
///
/// Tests need one fixed `now`, or a relative interval would resolve against a wall clock that moves
/// between the assertion and the value it is compared with.
pub async fn compile_at(text: &str, id_user: i64, db: &Db, now: i64) -> Result<Expr> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Expr::TRUE);
    }
    db.with(|mut conn| async move {
        let saved = SavedSearches::load(&mut conn, id_user).await?;
        let parsed = parse(trimmed, now)?;
        // No saved searches means no reference can resolve, so skip the walk: the common case is a
        // plain query and this keeps it to one database round trip.
        Ok(if saved.is_empty() && !names_a_saved_query(&parsed) {
            parsed
        } else {
            expand(&parsed, &saved, now)?
        })
    })
    .await
}

/// True when the tree still holds a `S:`/`search:` reference.
fn names_a_saved_query(expr: &Expr) -> bool {
    expr.preds()
        .iter()
        .any(|p| matches!(p, freshrss_search::ast::Pred::SavedQuery { .. }))
}

#[cfg(test)]
mod tests {
    use freshrss_search::ast::Pred;

    use super::*;
    use crate::db::dao::user as user_dao;
    use crate::db::support::db as test_db;
    use crate::domain::user_query::SavedQuery;
    use crate::error::Error;

    const NOW: i64 = 1_700_000_000;

    async fn setup() -> (Db, i64) {
        let store = test_db().await;
        let id_user = user(&store, "alice").await;
        (store, id_user)
    }

    async fn user(db: &Db, username: &str) -> i64 {
        let username = username.to_owned();
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: &username,
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await
        })
        .await
        .expect("create user")
    }

    /// Unwraps the search error, which `compile_at` boxes into the crate's own `Error`.
    fn search_error(e: &Error) -> &freshrss_search::SearchError {
        match e {
            Error::Search(inner) => inner,
            other => panic!("expected a search error, got {other}"),
        }
    }

    async fn save(db: &Db, id_user: i64, name: &str, search: &str) -> i64 {
        let query = SavedQuery::new(id_user, name, search);
        db.with(move |mut conn| async move { query_dao::insert(&mut conn, &query).await })
            .await
            .expect("save")
    }

    #[tokio::test]
    async fn a_plain_query_needs_no_lookup() {
        let (store, id_user) = setup().await;
        let expr = compile_at("intitle:rust", id_user, &store, NOW)
            .await
            .expect("compiles");
        assert_eq!(expr.preds().len(), 1);
        assert!(expr
            .preds()
            .iter()
            .all(|p| !matches!(p, Pred::SavedQuery { .. })));
    }

    #[tokio::test]
    async fn an_empty_query_is_true_rather_than_an_error() {
        let (store, id_user) = setup().await;
        for text in ["", "   ", "\n"] {
            assert!(
                compile_at(text, id_user, &store, NOW)
                    .await
                    .expect("compiles")
                    .is_true(),
                "{text:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_saved_search_is_expanded_by_name_and_by_id() {
        let (store, id_user) = setup().await;
        let id = save(&store, id_user, "unread rust", "intitle:rust -author:bot").await;

        // A bare value ends at the space, so the space in the name has to be quoted or escaped.
        assert!(matches!(
            search_error(
                &compile_at("search:unread rust", id_user, &store, NOW)
                    .await
                    .expect_err("rejected")
            ),
            freshrss_search::SearchError::QueryNotFound { .. }
        ));

        let by_id = compile_at(&format!("S:{id}"), id_user, &store, NOW)
            .await
            .expect("compiles");
        assert_eq!(
            freshrss_search::render::render(&by_id),
            "intitle:rust -author:bot"
        );
    }

    #[tokio::test]
    async fn a_missing_saved_search_is_a_400_not_a_silent_match_everything() {
        let (store, id_user) = setup().await;
        save(&store, id_user, "real", "intitle:x").await;
        let e = compile_at("S:999", id_user, &store, NOW)
            .await
            .expect_err("rejected");
        assert!(
            matches!(
                search_error(&e),
                freshrss_search::SearchError::QueryNotFound { .. }
            ),
            "{e}"
        );
        assert!(e.is_client_error());
    }

    #[tokio::test]
    async fn a_user_never_sees_another_users_saved_search() {
        let (store, id_user) = setup().await;
        let other = user(&store, "bob").await;
        let foreign = save(&store, other, "private", "intitle:secret").await;

        let e = compile_at(&format!("S:{foreign}"), id_user, &store, NOW)
            .await
            .expect_err("rejected");
        assert!(
            matches!(
                search_error(&e),
                freshrss_search::SearchError::QueryNotFound { .. }
            ),
            "another user's saved search must look absent, not forbidden: {e}"
        );
    }

    #[tokio::test]
    async fn a_blank_saved_search_counts_as_missing() {
        let (store, id_user) = setup().await;
        let id = save(&store, id_user, "blank", "   ").await;
        let e = compile_at(&format!("S:{id}"), id_user, &store, NOW)
            .await
            .expect_err("rejected");
        assert!(
            matches!(
                search_error(&e),
                freshrss_search::SearchError::QueryNotFound { .. }
            ),
            "{e}"
        );

        // And `S:*` skips it rather than failing the whole query.
        save(&store, id_user, "real", "intitle:x").await;
        let expr = compile_at("S:*", id_user, &store, NOW)
            .await
            .expect("compiles");
        assert_eq!(freshrss_search::render::render(&expr), "intitle:x");
    }

    #[tokio::test]
    async fn a_saved_search_may_reference_another() {
        let (store, id_user) = setup().await;
        let inner = save(&store, id_user, "inner", "intitle:deep").await;
        save(&store, id_user, "outer", &format!("S:{inner} f:4")).await;
        let outer = store
            .with(move |mut conn| async move {
                Ok(query_dao::find_by_name(&mut conn, id_user, "outer")
                    .await?
                    .expect("exists")
                    .id)
            })
            .await
            .expect("lookup");
        let expr = compile_at(&format!("S:{outer}"), id_user, &store, NOW)
            .await
            .expect("compiles");
        assert_eq!(freshrss_search::render::render(&expr), "intitle:deep f:4");
    }

    #[tokio::test]
    async fn a_saved_search_that_references_itself_is_reported_as_a_cycle() {
        let (store, id_user) = setup().await;
        let id = save(&store, id_user, "loop", "intitle:x").await;
        store
            .with(move |mut conn| async move {
                let mut q = query_dao::find_by_id(&mut conn, id, id_user)
                    .await?
                    .expect("exists");
                q.search = format!("S:{id}");
                query_dao::update(&mut conn, &q).await
            })
            .await
            .expect("rewrite");

        let e = compile_at(&format!("S:{id}"), id_user, &store, NOW)
            .await
            .expect_err("rejected");
        assert!(
            matches!(
                search_error(&e),
                freshrss_search::SearchError::QueryCycle { .. }
            ),
            "{e}"
        );
        assert!(e.is_client_error());
    }

    #[tokio::test]
    async fn a_relative_interval_inside_a_saved_search_uses_the_request_instant() {
        // A saved `date:P1W` has to mean "this week" when it runs, not when it was saved — and it has
        // to mean the same week as the `date:P1W` typed beside it in the same request.
        let (store, id_user) = setup().await;
        let id = save(&store, id_user, "recent", "date:P1W").await;
        let via_saved = compile_at(&format!("S:{id}"), id_user, &store, NOW)
            .await
            .expect("compiles");
        let inline = compile_at("date:P1W", id_user, &store, NOW)
            .await
            .expect("compiles");
        assert_eq!(via_saved, inline);
    }

    #[tokio::test]
    async fn a_syntax_error_is_reported_wherever_it_is() {
        let (store, id_user) = setup().await;
        assert!(matches!(
            search_error(
                &compile_at("intitle:a OR", id_user, &store, NOW)
                    .await
                    .expect_err("rejected")
            ),
            freshrss_search::SearchError::UnexpectedEnd { .. }
        ));
        let id = save(&store, id_user, "broken", "(intitle:a").await;
        assert!(matches!(
            search_error(
                &compile_at(&format!("S:{id}"), id_user, &store, NOW)
                    .await
                    .expect_err("rejected")
            ),
            freshrss_search::SearchError::UnbalancedParen { .. }
        ));
    }

    #[tokio::test]
    async fn the_resolver_reports_what_it_loaded() {
        let (store, id_user) = setup().await;
        store
            .with(move |mut conn| async move {
                assert!(SavedSearches::load(&mut conn, id_user).await?.is_empty());
                query_dao::insert(&mut conn, &SavedQuery::new(id_user, "a", "intitle:x")).await?;
                query_dao::insert(&mut conn, &SavedQuery::new(id_user, "blank", "  ")).await?;
                let saved = SavedSearches::load(&mut conn, id_user).await?;
                // The blank one is dropped at load time, so `S:*` never has to consider it.
                assert_eq!(saved.all_ids().len(), 1);
                assert!(saved.by_name("a").is_some());
                assert!(saved.by_name("blank").is_none());
                assert!(!saved.is_empty());
                Ok(())
            })
            .await
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
