//! Expanding `S:` / `search:` saved-query references into subtrees.
//!
//! [`crate::parser`] leaves a [`Pred::SavedQuery`] in the tree rather than resolving it, because
//! resolving needs the user's saved searches and this crate has no database. Neither backend can
//! evaluate one — both report [`SearchError::Unsupported`] — so [`expand`] runs between parsing and
//! compiling.
//!
//! FreshRSS expands by *string substitution* before parsing: `search:foo` is textually replaced by
//! `(the text of foo)` and the result is re-lexed. That has one consequence worth keeping and one
//! worth dropping. Worth keeping: a saved query is stored as text and re-read with whatever grammar
//! is current, so editing the grammar does not invalidate old queries. Worth dropping: because the
//! substitution only runs at recursion level 0, a saved query that references another saved query is
//! never resolved — the inner `S:3` survives as literal search text and silently matches articles
//! containing the characters "S:3". Expanding into subtrees instead makes nesting work, which is why
//! the cycle detection and the budget below exist at all.

use crate::ast::{Expr, IdSet, Pred, QueryRef};
use crate::error::SearchError;
use crate::parser::parse;

/// How many saved queries one expansion may open.
///
/// A cycle is caught by name, so this is not the cycle guard — it is the blow-up guard. A diamond
/// (`a` references `b` and `c`, both of which reference `d` and `e`, …) is acyclic and still doubles
/// the tree at every level. Sixty-four is far more nesting than anyone writes by hand.
pub const MAX_EXPANSIONS: usize = 64;

/// Where [`expand`] looks up a saved search.
///
/// Returns **text**, not an [`Expr`]: that is what the `user_query` table stores, and parsing it here
/// means a query saved under an older grammar is read with the current one.
///
/// `None` and an empty/whitespace string are the same answer. A blank saved search is a mistake, and
/// treating it as "no constraint" — what FreshRSS's substitution does — would make a `read` filter
/// action keyed on it mark every article in the feed read.
pub trait QueryResolver {
    /// The text of the saved search with this id.
    fn by_id(&self, id: i64) -> Option<String>;
    /// The text of the saved search with this exact name. Names are matched case-sensitively, the way
    /// the unique index on `user_query` stores them.
    fn by_name(&self, name: &str) -> Option<String>;
    /// The ids of every saved search, for `S:*`.
    fn all_ids(&self) -> Vec<i64>;
}

/// Replaces every [`Pred::SavedQuery`] in `expr` with the referenced query's own subtree.
///
/// `now` is the same instant the outer query was parsed with, so a relative interval inside a saved
/// query (`date:P1W`) means the same thing whether it was typed inline or reached through `S:`.
///
/// # Errors
///
/// - [`SearchError::QueryNotFound`] when a reference names a saved search that does not exist or is
///   blank. `S:*` does not raise this: a wildcard asks for whatever is there.
/// - [`SearchError::QueryCycle`] when a saved search references itself, directly or through others.
/// - [`SearchError::BudgetExceeded`] after [`MAX_EXPANSIONS`] opens.
/// - Any parse error from a saved search's own text, which surfaces as the error it is rather than as
///   a broken tree.
pub fn expand(expr: &Expr, resolver: &dyn QueryResolver, now: i64) -> Result<Expr, SearchError> {
    let mut state = State {
        resolver,
        now,
        stack: Vec::new(),
        budget: MAX_EXPANSIONS,
    };
    state.expr(expr)
}

struct State<'a> {
    resolver: &'a dyn QueryResolver,
    now: i64,
    /// The references currently being expanded, innermost last. A repeat is a cycle.
    stack: Vec<String>,
    budget: usize,
}

impl State<'_> {
    fn expr(&mut self, expr: &Expr) -> Result<Expr, SearchError> {
        Ok(match expr {
            Expr::Leaf(preds) => self.leaf(preds)?,
            Expr::Node { op, children } => {
                let expanded = children
                    .iter()
                    .map(|c| self.expr(c))
                    .collect::<Result<Vec<_>, _>>()?;
                if op.is_and() {
                    Expr::and(expanded)
                } else {
                    Expr::or(expanded)
                }
            }
            Expr::Not(inner) => Expr::negate(self.expr(inner)?),
        })
    }

    /// A leaf is an implicit AND, so one saved query inside it turns the leaf into a node: the
    /// predicates around it stay leaves and the expansion becomes a sibling. Order is preserved, so
    /// the canonical text of `intext:x search:a #t` still reads in the order it was written.
    fn leaf(&mut self, preds: &[Pred]) -> Result<Expr, SearchError> {
        if !preds.iter().any(|p| matches!(p, Pred::SavedQuery { .. })) {
            return Ok(Expr::Leaf(preds.to_vec()));
        }

        let mut parts: Vec<Expr> = Vec::new();
        let mut plain: Vec<Pred> = Vec::new();
        for pred in preds {
            match pred {
                Pred::SavedQuery { target, negated } => {
                    if !plain.is_empty() {
                        parts.push(Expr::Leaf(std::mem::take(&mut plain)));
                    }
                    let expanded = self.reference(target)?;
                    parts.push(if *negated {
                        Expr::negate(expanded)
                    } else {
                        expanded
                    });
                }
                other => plain.push(other.clone()),
            }
        }
        if !plain.is_empty() {
            parts.push(Expr::Leaf(plain));
        }
        Ok(Expr::and(parts))
    }

    /// Resolves one reference to a subtree, guarding against cycles and blow-up.
    fn reference(&mut self, target: &QueryRef) -> Result<Expr, SearchError> {
        let resolved: Vec<(String, Option<String>)> = match target {
            QueryRef::Ids(IdSet::List(ids)) => ids
                .iter()
                .map(|id| {
                    let key = format!("S:{id}");
                    let text = self.resolver.by_id(*id).filter(|t| usable(t));
                    (key, text)
                })
                .collect(),
            // A wildcard is a request for whatever exists, so an individual miss is not an error —
            // but with nothing at all it has to be FALSE, not TRUE, or `S:*` would match every
            // article for a user with no saved searches.
            QueryRef::Ids(IdSet::Any) => self
                .resolver
                .all_ids()
                .into_iter()
                .map(|id| {
                    let key = format!("S:{id}");
                    let text = self.resolver.by_id(id).filter(|t| usable(t));
                    (key, text)
                })
                .collect(),
            QueryRef::Names(names) => names
                .iter()
                .map(|name| {
                    let key = format!("search:{name}");
                    let text = self.resolver.by_name(name).filter(|t| usable(t));
                    (key, text)
                })
                .collect(),
        };

        // Several names or ids in one reference are ORed, matching `S:1,2` in FreshRSS and every
        // other list-valued operator here.
        let mut branches = Vec::with_capacity(resolved.len());
        for (key, text) in resolved {
            let Some(text) = text else {
                // `S:*` collects whatever is there, so a miss is expected; an explicit reference is
                // the user asking for something specific that is not there.
                if matches!(target, QueryRef::Ids(IdSet::Any)) {
                    continue;
                }
                return Err(SearchError::QueryNotFound { name: key });
            };
            branches.push(self.open(&key, &text)?);
        }
        Ok(Expr::or(branches))
    }

    /// Parses and recursively expands one saved query's text.
    fn open(&mut self, key: &str, text: &str) -> Result<Expr, SearchError> {
        if self.stack.iter().any(|open| open == key) {
            return Err(SearchError::QueryCycle {
                name: key.to_owned(),
            });
        }
        if self.budget == 0 {
            return Err(SearchError::BudgetExceeded {
                max: MAX_EXPANSIONS,
            });
        }
        self.budget -= 1;

        let parsed = parse(text, self.now)?;
        self.stack.push(key.to_owned());
        let expanded = self.expr(&parsed);
        self.stack.pop();
        expanded
    }
}

/// True for stored text worth expanding.
fn usable(text: &str) -> bool {
    !text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const NOW: i64 = 1_136_214_245;

    /// A resolver over two maps, which is all the trait asks for.
    #[derive(Default)]
    struct Saved {
        by_id: HashMap<i64, String>,
        by_name: HashMap<String, String>,
    }

    impl Saved {
        fn new(entries: &[(i64, &str, &str)]) -> Self {
            let mut saved = Self::default();
            for (id, name, search) in entries {
                saved.by_id.insert(*id, (*search).to_owned());
                saved
                    .by_name
                    .insert((*name).to_owned(), (*search).to_owned());
            }
            saved
        }
    }

    impl QueryResolver for Saved {
        fn by_id(&self, id: i64) -> Option<String> {
            self.by_id.get(&id).cloned()
        }
        fn by_name(&self, name: &str) -> Option<String> {
            self.by_name.get(name).cloned()
        }
        fn all_ids(&self) -> Vec<i64> {
            let mut ids: Vec<i64> = self.by_id.keys().copied().collect();
            ids.sort_unstable();
            ids
        }
    }

    fn expand_with(q: &str, saved: &Saved) -> Result<Expr, SearchError> {
        expand(&parse(q, NOW).unwrap(), saved, NOW)
    }

    /// Expands and renders, so a test can assert on readable text rather than on a tree.
    fn text(q: &str, saved: &Saved) -> String {
        crate::render::render(&expand_with(q, saved).unwrap())
    }

    // --- substitution ------------------------------------------------------

    #[test]
    fn a_query_with_no_reference_is_returned_unchanged() {
        let saved = Saved::default();
        for q in ["rust", "intitle:a f:1", "-(a OR b)", "#tag labels:todo"] {
            assert_eq!(
                text(q, &saved),
                crate::render::render(&parse(q, NOW).unwrap())
            );
        }
    }

    #[test]
    fn a_reference_by_name_becomes_the_saved_subtree() {
        let saved = Saved::new(&[(1, "unread-rust", "intitle:rust -author:bot")]);
        assert_eq!(
            text("search:unread-rust", &saved),
            "intitle:rust -author:bot"
        );
    }

    #[test]
    fn a_reference_by_id_becomes_the_saved_subtree() {
        let saved = Saved::new(&[(7, "mine", "f:3")]);
        assert_eq!(text("S:7", &saved), "f:3");
    }

    #[test]
    fn several_ids_are_ored() {
        let saved = Saved::new(&[(1, "a", "intitle:one"), (2, "b", "intitle:two")]);
        assert_eq!(text("S:1,2", &saved), "intitle:one OR intitle:two");
    }

    #[test]
    fn the_wildcard_means_every_saved_search() {
        let saved = Saved::new(&[(1, "a", "intitle:one"), (2, "b", "intitle:two")]);
        assert_eq!(text("S:*", &saved), "intitle:one OR intitle:two");
    }

    #[test]
    fn the_wildcard_matches_nothing_when_there_is_nothing() {
        // Not TRUE: a user with no saved searches asking for "any saved search" should get an empty
        // list, not every article they have.
        let saved = Saved::default();
        assert!(expand_with("S:*", &saved).unwrap().is_false());
    }

    #[test]
    fn an_empty_id_list_matches_nothing() {
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        assert!(expand_with("S:", &saved).unwrap().is_false());
    }

    // --- composition with the rest of the grammar --------------------------

    #[test]
    fn a_reference_keeps_the_grouping_of_the_query_around_it() {
        let saved = Saved::new(&[(1, "a", "intitle:one OR intitle:two")]);
        // The expansion is a subtree, so the saved query's own OR cannot leak out and re-associate
        // with the surrounding AND — which is exactly what FreshRSS's string substitution gets wrong
        // unless the stored text happens to be parenthesized.
        assert_eq!(
            text("f:9 search:a", &saved),
            "f:9 (intitle:one OR intitle:two)"
        );
    }

    #[test]
    fn a_reference_shares_a_leaf_with_other_predicates() {
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        assert_eq!(
            text("intext:body search:a #tag", &saved),
            "intext:body intitle:one #tag",
        );
    }

    #[test]
    fn a_negated_reference_inverts_the_whole_saved_query() {
        let saved = Saved::new(&[(1, "a", "intitle:one OR intitle:two")]);
        assert_eq!(text("-search:a", &saved), "-(intitle:one OR intitle:two)");
    }

    #[test]
    fn a_reference_nests_inside_parentheses_and_ors() {
        let saved = Saved::new(&[(1, "a", "intitle:one"), (2, "b", "f:5")]);
        assert_eq!(
            text("(search:a OR search:b) intext:x", &saved),
            "(intitle:one OR f:5) intext:x"
        );
    }

    // --- nesting -----------------------------------------------------------

    #[test]
    fn a_saved_query_may_reference_another() {
        // The case FreshRSS cannot do: its substitution runs once, so the inner reference survives as
        // literal search text.
        let saved = Saved::new(&[(1, "inner", "intitle:deep"), (2, "outer", "S:1 f:4")]);
        assert_eq!(text("search:outer", &saved), "intitle:deep f:4");
    }

    #[test]
    fn nesting_by_name_works_too() {
        let saved = Saved::new(&[
            (1, "inner", "intitle:deep"),
            (2, "middle", "search:inner"),
            (3, "outer", "search:middle f:1"),
        ]);
        assert_eq!(text("S:3", &saved), "intitle:deep f:1");
    }

    // --- cycles and budgets ------------------------------------------------

    #[test]
    fn a_saved_query_that_references_itself_is_a_cycle() {
        let saved = Saved::new(&[(1, "loop", "S:1")]);
        let e = expand_with("S:1", &saved).unwrap_err();
        assert!(
            matches!(&e, SearchError::QueryCycle { name } if name == "S:1"),
            "{e}"
        );
    }

    #[test]
    fn a_cycle_through_several_queries_is_still_a_cycle() {
        let saved = Saved::new(&[(1, "a", "S:2"), (2, "b", "S:3"), (3, "c", "search:a")]);
        let e = expand_with("S:1", &saved).unwrap_err();
        // The name reported is the one that was already open, not the one that closed the loop.
        assert!(
            matches!(&e, SearchError::QueryCycle { name } if name == "S:2"),
            "{e}"
        );
    }

    #[test]
    fn the_same_query_twice_in_one_branch_is_not_a_cycle() {
        // A diamond is not a cycle: `S:1,1` and `search:a search:a` both legitimately reuse a
        // subtree. Only a reference that is *still open* when it is reached again is a cycle.
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        assert_eq!(text("S:1,1", &saved), "intitle:one OR intitle:one");
        assert_eq!(text("search:a search:a", &saved), "intitle:one intitle:one");
    }

    #[test]
    fn a_wildcard_that_reaches_itself_is_a_cycle() {
        let saved = Saved::new(&[(1, "a", "S:*")]);
        let e = expand_with("S:*", &saved).unwrap_err();
        assert!(matches!(&e, SearchError::QueryCycle { .. }), "{e}");
    }

    #[test]
    fn an_acyclic_blowup_hits_the_budget() {
        // Each level references the next one twice, so the tree doubles without ever repeating a
        // reference on one path. The stack cannot catch this; the budget has to.
        let mut saved = Saved::default();
        let levels = MAX_EXPANSIONS;
        saved
            .by_id
            .insert(levels as i64 + 1, "intitle:leaf".to_owned());
        for level in (1..=levels).rev() {
            let next = level as i64 + 1;
            saved.by_id.insert(level as i64, format!("S:{next},{next}"));
        }
        let e = expand_with("S:1", &saved).unwrap_err();
        assert!(
            matches!(&e, SearchError::BudgetExceeded { max } if *max == MAX_EXPANSIONS),
            "{e}"
        );
    }

    // --- missing and blank references --------------------------------------

    #[test]
    fn a_missing_name_is_an_error_not_a_silent_match_everything() {
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        let e = expand_with("search:nope", &saved).unwrap_err();
        assert!(
            matches!(&e, SearchError::QueryNotFound { name } if name == "search:nope"),
            "{e}"
        );
    }

    #[test]
    fn a_missing_id_is_an_error() {
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        let e = expand_with("S:99", &saved).unwrap_err();
        assert!(
            matches!(&e, SearchError::QueryNotFound { name } if name == "S:99"),
            "{e}"
        );
    }

    #[test]
    fn one_missing_id_fails_the_whole_reference() {
        // FreshRSS drops the unresolvable id and keeps the rest, so `S:1,99` quietly searches for
        // less than the user asked for.
        let saved = Saved::new(&[(1, "a", "intitle:one")]);
        let e = expand_with("S:1,99", &saved).unwrap_err();
        assert!(matches!(&e, SearchError::QueryNotFound { .. }), "{e}");
    }

    #[test]
    fn a_blank_saved_search_counts_as_missing() {
        // Substituting nothing — what FreshRSS does — turns the reference into "no constraint", so a
        // `read` filter action keyed on it would mark the whole feed read.
        let saved = Saved::new(&[(1, "blank", "   ")]);
        assert!(matches!(
            expand_with("search:blank", &saved).unwrap_err(),
            SearchError::QueryNotFound { .. }
        ));
    }

    #[test]
    fn the_wildcard_skips_a_blank_saved_search_rather_than_failing() {
        let saved = Saved::new(&[(1, "blank", ""), (2, "real", "intitle:one")]);
        assert_eq!(text("S:*", &saved), "intitle:one");
    }

    // --- the saved text is parsed, not trusted -----------------------------

    #[test]
    fn a_syntax_error_inside_a_saved_query_surfaces() {
        let saved = Saved::new(&[(1, "broken", "intitle:a OR")]);
        let e = expand_with("S:1", &saved).unwrap_err();
        assert!(matches!(e, SearchError::UnexpectedEnd { .. }), "{e}");
    }

    #[test]
    fn a_saved_query_uses_the_same_now_as_the_outer_query() {
        // `date:P1W` is relative, so the instant has to be the outer one — a saved search must not
        // silently mean something different depending on when it is expanded.
        let saved = Saved::new(&[(1, "recent", "date:P1W")]);
        let expanded = expand_with("search:recent", &saved).unwrap();
        let inline = parse("date:P1W", NOW).unwrap();
        assert_eq!(expanded, inline);
    }

    #[test]
    fn nothing_left_behind() {
        let saved = Saved::new(&[
            (1, "a", "intitle:one"),
            (2, "b", "S:1 OR search:a"),
            (3, "c", "-(S:2 f:1)"),
        ]);
        let expanded = expand_with("S:3 intext:x", &saved).unwrap();
        assert!(
            expanded
                .preds()
                .iter()
                .all(|p| !matches!(p, Pred::SavedQuery { .. })),
            "an unexpanded reference survived: {expanded:?}"
        );
        // And both backends can now evaluate it rather than refusing.
        assert!(crate::sql::compile(&expanded, &crate::sql::SqlCtx::default()).is_ok());
    }
}
