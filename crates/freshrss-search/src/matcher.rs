//! The in-memory evaluator: the same AST, evaluated without a database.
//!
//! This exists for two reasons, and the second is the one that pays for itself.
//!
//! **Filter actions run before an article has an id.** A `read`/`star`/`label` filter is applied to
//! items straight off the wire, during the refresh, when the entry row does not exist yet. There is
//! nothing to run SQL against.
//!
//! **It is the other half of the equivalence test.** [`crate::sql`] compiles the tree to SQL; this
//! module evaluates it directly. Both call [`crate::normalize::fold`] on both sides of every text
//! comparison, so they agree *by construction*. A property test can then generate random trees,
//! seed a database, and assert that the SQL id-set and the matcher id-set are identical — which is
//! the one test that permanently prevents the class of bug FreshRSS actually has.

use crate::ast::{DateField, DateRange, Expr, IdSet, Op, Pred, QueryRef, TextPat};
use crate::error::SearchError;
use crate::normalize::fold;

/// The fields a query can look at, as seen by the matcher.
///
/// Implemented by the server's `Entry` row and by `freshrss_feed::RawItem`, which is what makes the
/// same filter rules work during refresh and when re-running a saved search over stored articles.
///
/// Text accessors return the **stored** form, unfolded. The matcher folds each one exactly once per
/// evaluation (see [`Folded`]); folding inside every comparison would make a twenty-predicate query
/// over a thousand articles do twenty thousand normalizations instead of one thousand.
pub trait SearchTarget {
    /// The entry id, or `None` for an item that has not been stored yet.
    ///
    /// `None` is not the same as `0`: a query that reads the id (`e:`, `date:`) cannot be answered
    /// for an unstored item, and the matcher reports that rather than guessing.
    fn id(&self) -> Option<i64>;
    fn feed_id(&self) -> i64;
    fn category_id(&self) -> Option<i64>;
    fn title(&self) -> &str;
    fn content(&self) -> &str;
    fn author(&self) -> &str;
    fn link(&self) -> &str;
    /// The `tags` column form, `"#a #b"`.
    fn tags(&self) -> &str;
    fn label_ids(&self) -> &[i64];
    fn label_names(&self) -> &[String];
    /// Publication time in unix seconds.
    fn date(&self) -> i64;
    fn last_modified(&self) -> Option<i64>;
    fn last_user_modified(&self) -> Option<i64>;
}

/// Every text field of one target, accent-folded once.
struct Folded {
    title: String,
    content: String,
    author: String,
    link: String,
    /// The tag list as individual folded names, so a whole-tag comparison is an equality check
    /// rather than a substring search — the in-memory equivalent of the SQL `' #'` terminator trick.
    tags: Vec<String>,
    labels: Vec<String>,
}

impl Folded {
    fn of(target: &dyn SearchTarget) -> Self {
        Self {
            title: fold(target.title()),
            content: fold(target.content()),
            author: fold(target.author()),
            link: fold(target.link()),
            tags: split_folded_tags(target.tags()),
            labels: target.label_names().iter().map(|n| fold(n)).collect(),
        }
    }
}

/// Splits the `"#a #b"` column form into folded individual tags.
///
/// Comma is accepted as a separator too, because some feeds put every keyword in one category and
/// the stored form preserves whatever the feed sent.
fn split_folded_tags(tags: &str) -> Vec<String> {
    tags.split(['#', ','])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(fold)
        .collect()
}

/// Evaluates an expression against one target.
///
/// # Errors
///
/// [`SearchError::Unsupported`] when the tree still holds an unexpanded [`Pred::SavedQuery`], or
/// when the query reads a field the target cannot supply — an unstored item has no id, so `e:` and
/// `date:` have no meaning for it. Reporting that is better than answering `false`, which would
/// silently make every filter action that mentions a date a no-op.
pub fn matches(expr: &Expr, target: &dyn SearchTarget) -> Result<bool, SearchError> {
    evaluate(expr, target, &Folded::of(target))
}

fn evaluate(expr: &Expr, target: &dyn SearchTarget, folded: &Folded) -> Result<bool, SearchError> {
    match expr {
        Expr::Leaf(preds) => {
            for pred in preds {
                if !eval_pred(pred, target, folded)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Expr::Node { op, children } => {
            let want_all = matches!(op, Op::And);
            for child in children {
                let got = evaluate(child, target, folded)?;
                if got != want_all {
                    // Short-circuit: `And` stops at the first false, `Or` at the first true.
                    return Ok(got);
                }
            }
            // An empty `And` is true and an empty `Or` is false, which is exactly `want_all`.
            Ok(want_all)
        }
        Expr::Not(inner) => Ok(!evaluate(inner, target, folded)?),
    }
}

fn eval_pred(pred: &Pred, target: &dyn SearchTarget, folded: &Folded) -> Result<bool, SearchError> {
    let result = match pred {
        Pred::EntryIds { ids, negated } => {
            let id = require_id(target, "e:")?;
            xor(ids.contains(&id), *negated)
        }
        Pred::FeedIds { ids, negated } => xor(ids.contains(&target.feed_id()), *negated),
        Pred::CategoryIds { ids, negated } => xor(
            target.category_id().is_some_and(|c| ids.contains(&c)),
            *negated,
        ),
        Pred::Title { pat, negated } => xor(pat.matches_folded(&folded.title), *negated),
        Pred::Content { pat, negated } => xor(pat.matches_folded(&folded.content), *negated),
        Pred::FreeText { pat, negated } => xor(
            pat.matches_folded(&folded.title) || pat.matches_folded(&folded.content),
            *negated,
        ),
        Pred::Author { pat, negated } => xor(pat.matches_folded(&folded.author), *negated),
        Pred::Url { pat, negated } => xor(pat.matches_folded(&folded.link), *negated),
        Pred::Tag { pat, negated } => xor(any_tag_matches(pat, &folded.tags), *negated),
        Pred::LabelIds { ids, negated } => xor(label_ids_match(ids, target.label_ids()), *negated),
        Pred::LabelNames { names, negated } => {
            xor(any_label_name_matches(names, &folded.labels), *negated)
        }
        Pred::Date {
            field,
            range,
            negated,
        } => {
            let value = date_value(target, *field)?;
            let inside = match value {
                Some(v) => range_contains(*range, v),
                // A NULL column cannot satisfy a range. `NOT (NULL BETWEEN …)` is NULL in SQL, which
                // a WHERE clause reads as false, so both polarities answer "no match" — matching what
                // the SQL backend does rather than what boolean logic would suggest.
                None => false,
            };
            xor(inside, *negated)
        }
        Pred::SavedQuery {
            target: reference, ..
        } => {
            return Err(SearchError::Unsupported(match reference {
                QueryRef::Ids(_) => "S:".to_owned(),
                QueryRef::Names(_) => "search:".to_owned(),
            }))
        }
    };
    Ok(result)
}

/// Applies a negation. Kept as a function because `found ^ negated` reads as a bit trick at a glance
/// and this is the single place where getting it backwards would invert a whole class of query.
const fn xor(found: bool, negated: bool) -> bool {
    found != negated
}

fn require_id(target: &dyn SearchTarget, operator: &str) -> Result<i64, SearchError> {
    target
        .id()
        .ok_or_else(|| SearchError::Unsupported(operator.to_owned()))
}

/// Reads the timestamp a date predicate compares against, in unix **seconds**.
///
/// [`DateField::Ingestion`] is the entry id, which is a microsecond wall clock. `crate::sql` scales
/// the range bounds up by a million to meet it; here the id is scaled down instead, which cannot
/// overflow. Either direction is correct as long as only one side moves — getting that wrong makes
/// the two backends disagree by six orders of magnitude, which is precisely what the equivalence
/// test exists to catch.
fn date_value(target: &dyn SearchTarget, field: DateField) -> Result<Option<i64>, SearchError> {
    Ok(match field {
        DateField::Ingestion => Some(require_id(target, "date:")? / MICROSECONDS_PER_SECOND),
        DateField::Publication => Some(target.date()),
        DateField::Modified => target.last_modified(),
        DateField::UserModified => target.last_user_modified(),
    })
}

const MICROSECONDS_PER_SECOND: i64 = 1_000_000;

/// Inclusive range test, honouring the complemented form the parser produces for a negated range.
fn range_contains(range: DateRange, value: i64) -> bool {
    match (range.min, range.max) {
        // `min > max` is the complement of a two-sided range: everything outside the original band.
        (Some(lo), Some(hi)) if lo > hi => value < hi || value > lo,
        (Some(lo), Some(hi)) => value >= lo && value <= hi,
        (Some(lo), None) => value >= lo,
        (None, Some(hi)) => value <= hi,
        (None, None) => true,
    }
}

/// Whole-tag matching: the needle must equal one of the tags, not merely occur inside one.
///
/// This is the in-memory counterpart of the SQL `TRIM(tags) || ' #' LIKE '%needle #%'` trick. Both
/// exist to stop `#ru` from matching the tag `rust`, and the equivalence test compares them.
fn any_tag_matches(pat: &TextPat, folded_tags: &[String]) -> bool {
    match pat {
        // A `Contains` needle over a tag is an exact, folded comparison — the same semantics the SQL
        // side gets from the `' #'` terminator.
        TextPat::Contains(needle) => folded_tags.iter().any(|tag| tag == needle),
        TextPat::Regex(re) => folded_tags.iter().any(|tag| re.is_match_folded(tag)),
    }
}

fn label_ids_match(wanted: &IdSet, have: &[i64]) -> bool {
    match wanted {
        IdSet::Any => !have.is_empty(),
        IdSet::List(list) => list.iter().any(|id| have.contains(id)),
    }
}

fn any_label_name_matches(names: &[TextPat], folded_labels: &[String]) -> bool {
    folded_labels.iter().any(|label| {
        names.iter().any(|pat| match pat {
            TextPat::Contains(needle) => label == needle,
            TextPat::Regex(re) => re.is_match_folded(label),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    const NOW: i64 = 1_136_214_245;

    /// A minimal target with every field settable.
    #[derive(Debug, Default, Clone)]
    struct Article {
        id: Option<i64>,
        feed_id: i64,
        category_id: Option<i64>,
        title: String,
        content: String,
        author: String,
        link: String,
        tags: String,
        label_ids: Vec<i64>,
        label_names: Vec<String>,
        date: i64,
        last_modified: Option<i64>,
        last_user_modified: Option<i64>,
    }

    impl Article {
        fn stored() -> Self {
            Self {
                id: Some(1_136_214_245_000_000),
                feed_id: 7,
                category_id: Some(3),
                title: "A Rust article about École".into(),
                content: "<p>Body text mentioning axum</p>".into(),
                author: "Ada Lovelace; Grace Hopper".into(),
                link: "https://example.com/a-rust-article".into(),
                tags: "#rust #rss".into(),
                label_ids: vec![11],
                label_names: vec!["todo".into()],
                date: NOW,
                last_modified: Some(NOW + 100),
                last_user_modified: Some(NOW + 200),
            }
        }

        /// An item straight off the wire: no id, no labels, no user timestamps.
        fn unstored() -> Self {
            Self {
                id: None,
                label_ids: Vec::new(),
                label_names: Vec::new(),
                last_modified: None,
                last_user_modified: None,
                ..Self::stored()
            }
        }
    }

    impl SearchTarget for Article {
        fn id(&self) -> Option<i64> {
            self.id
        }
        fn feed_id(&self) -> i64 {
            self.feed_id
        }
        fn category_id(&self) -> Option<i64> {
            self.category_id
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn content(&self) -> &str {
            &self.content
        }
        fn author(&self) -> &str {
            &self.author
        }
        fn link(&self) -> &str {
            &self.link
        }
        fn tags(&self) -> &str {
            &self.tags
        }
        fn label_ids(&self) -> &[i64] {
            &self.label_ids
        }
        fn label_names(&self) -> &[String] {
            &self.label_names
        }
        fn date(&self) -> i64 {
            self.date
        }
        fn last_modified(&self) -> Option<i64> {
            self.last_modified
        }
        fn last_user_modified(&self) -> Option<i64> {
            self.last_user_modified
        }
    }

    fn m(q: &str, target: &dyn SearchTarget) -> bool {
        matches(&parse(q, NOW).unwrap(), target).unwrap()
    }

    fn article() -> Article {
        Article::stored()
    }

    // --- text predicates ---------------------------------------------------

    #[test]
    fn free_text_searches_title_and_content() {
        assert!(m("rust", &article()));
        assert!(m("axum", &article()), "found in the body");
        assert!(m("école", &article()), "accent-folded on both sides");
        assert!(m("ECOLE", &article()), "case-folded");
        assert!(!m("python", &article()));
    }

    #[test]
    fn intitle_and_intext_are_distinct() {
        assert!(m("intitle:rust", &article()));
        assert!(!m("intitle:axum", &article()), "axum is only in the body");
        assert!(m("intext:axum", &article()));
        assert!(!m("intext:rust", &article()), "rust is only in the title");
    }

    #[test]
    fn a_substring_match_is_a_substring() {
        assert!(m("intitle:Ru", &article()));
        assert!(m("author:Lovelace", &article()));
        assert!(m("inurl:rust-article", &article()));
    }

    #[test]
    fn author_matching_sees_the_joined_form() {
        // The column holds "Ada Lovelace; Grace Hopper", so a needle spanning the separator matches —
        // but it has to be quoted, because a bare value ends at whitespace like any other.
        assert!(m(r#"author:"Lovelace; Grace""#, &article()));
        assert!(m("author:Hopper", &article()));
        assert!(m("author:Lovelace", &article()));
    }

    #[test]
    fn a_regex_is_evaluated_against_the_folded_text() {
        assert!(m("intitle:/^a rust/i", &article()));
        assert!(
            m("intitle:/ecole/", &article()),
            "the needle is folded, so the pattern must be too"
        );
        assert!(!m("intitle:/^rust/", &article()));
    }

    // --- negation ----------------------------------------------------------

    #[test]
    fn negation_inverts_a_single_predicate() {
        assert!(m("-python", &article()));
        assert!(!m("-rust", &article()));
        assert!(m("!intitle:axum", &article()));
    }

    #[test]
    fn two_negations_in_one_leaf_both_have_to_hold() {
        assert!(m("-python -haskell", &article()));
        assert!(!m("-rust -haskell", &article()));
    }

    #[test]
    fn a_negated_group_inverts_the_whole_subtree() {
        assert!(m("!(python OR haskell)", &article()));
        assert!(!m("!(rust OR haskell)", &article()));
    }

    // --- tags and labels ---------------------------------------------------

    #[test]
    fn a_tag_matches_whole_tags_only() {
        assert!(m("#rust", &article()));
        assert!(m("#rss", &article()));
        assert!(!m("#ru", &article()), "a prefix is not a tag");
        assert!(!m("#rusts", &article()));
    }

    #[test]
    fn a_tag_regex_is_applied_per_tag() {
        assert!(m("#/^rus/", &article()));
        assert!(m("#/^rss$/", &article()), "anchored to one tag");
        assert!(!m("#/^rust #rss$/", &article()));
    }

    #[test]
    fn labels_match_by_id_and_by_name() {
        assert!(m("l:11", &article()));
        assert!(!m("l:12", &article()));
        assert!(m("l:*", &article()), "has at least one label");
        assert!(m("labels:todo", &article()));
        assert!(!m("labels:todos", &article()), "whole-name comparison");
        assert!(
            m("labels:todo,reading", &article()),
            "several names are ORed"
        );
    }

    #[test]
    fn an_unlabelled_article_fails_l_star() {
        let mut a = article();
        a.label_ids.clear();
        assert!(!m("l:*", &a));
        assert!(m("-l:*", &a));
    }

    // --- ids ---------------------------------------------------------------

    #[test]
    fn id_operators() {
        assert!(m("f:7", &article()));
        assert!(!m("f:8", &article()));
        assert!(m("f:1,7,9", &article()));
        assert!(m("-f:8", &article()));
        assert!(m("c:3", &article()));
        assert!(!m("c:4", &article()));
    }

    #[test]
    fn an_empty_id_list_matches_nothing() {
        assert!(!m("f:", &article()));
        assert!(m("-f:", &article()));
        assert!(!m("l:", &article()));
    }

    #[test]
    fn an_article_with_no_category_fails_c() {
        let mut a = article();
        a.category_id = None;
        assert!(!m("c:3", &a));
        assert!(m("-c:3", &a));
    }

    // --- dates -------------------------------------------------------------

    // A bare unix timestamp is not a date-interval operand — `strtotime` rejects it too — so these
    // tests spell the instants out. `NOW` is 2006-01-02T15:04:05Z.
    #[test]
    fn pubdate_compares_against_the_publication_time() {
        assert!(m("pubdate:2006-01-02", &article()));
        assert!(m("pubdate:2006-01-01/2006-01-03", &article()));
        assert!(!m("pubdate:2006-01-03/2006-01-10", &article()));
        assert!(!m("pubdate:2005-01-01/2005-12-31", &article()));
    }

    #[test]
    fn mdate_and_userdate_read_their_own_columns() {
        // The fixture's last_modified is NOW + 100 and last_user_modified is NOW + 200, both still
        // inside 2006-01-02, so a single day distinguishes them only by the wider band below.
        assert!(m("mdate:2006-01-02", &article()));
        assert!(m("userdate:2006-01-02", &article()));

        let mut a = article();
        a.last_modified = Some(1_136_214_245 + 86_400 * 5);
        a.last_user_modified = Some(1_136_214_245);
        assert!(m("mdate:2006-01-07", &a), "edited five days later");
        assert!(!m("userdate:2006-01-07", &a), "but not user-modified then");
        assert!(m("userdate:2006-01-02", &a));
    }

    #[test]
    fn a_null_timestamp_never_satisfies_a_range_in_either_polarity() {
        let mut a = article();
        a.last_modified = None;
        assert!(!m("mdate:2006-01-02", &a));
        // SQL's `NOT (NULL BETWEEN …)` is NULL, which a WHERE clause reads as false. The matcher
        // must agree, or the equivalence test fails on every article that was never edited.
        assert!(!m("-mdate:2006-01-02", &a));
    }

    #[test]
    fn a_relative_interval_looks_back_from_now() {
        let mut a = article();
        a.id = Some(NOW * 1_000_000);
        assert!(m("date:P1W", &a), "added now, inside the last week");
        a.id = Some((NOW - 30 * 86_400) * 1_000_000);
        assert!(!m("date:P1W", &a), "added a month ago");
    }

    #[test]
    fn a_negated_range_is_complemented_not_wrapped() {
        let mut a = article();
        a.id = Some(NOW * 1_000_000);
        assert!(
            !m("-date:2006-01-01/2006-01-03", &a),
            "inside the band, so excluded"
        );
        assert!(
            m("-date:2006-02-01/2006-02-28", &a),
            "outside the band, so included"
        );
    }

    #[test]
    fn date_uses_the_id_in_microseconds() {
        // The scale factor is the single most likely place for the two backends to diverge.
        let mut a = article();
        a.id = Some(NOW * 1_000_000 + 500_000);
        assert!(m("date:2006-01-01/2006-01-03", &a));
        // An id read as seconds instead of microseconds would land in 1970 and fail this.
        a.id = Some(NOW);
        assert!(!m("date:2006-01-01/2006-01-03", &a));
    }

    // --- boolean structure -------------------------------------------------

    #[test]
    fn juxtaposition_is_conjunction() {
        assert!(m("rust axum", &article()));
        assert!(!m("rust python", &article()));
    }

    #[test]
    fn or_is_disjunction_and_binds_loosest() {
        assert!(m("python OR axum", &article()));
        assert!(
            m("rust axum OR python haskell", &article()),
            "(rust AND axum) OR …"
        );
        assert!(!m("python axum OR rust haskell", &article()));
    }

    #[test]
    fn parentheses_override_precedence() {
        // Without the brackets, `axum OR python haskell` would group as `axum OR (python AND haskell)`
        // — which also happens to be true here, so assert a case where the grouping is observable.
        assert!(
            m("(rust python) OR axum", &article()),
            "axum alone satisfies the second branch"
        );
        assert!(
            !m("(rust python) OR (axum haskell)", &article()),
            "neither branch holds"
        );
        assert!(m("rust (axum OR python)", &article()));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(m("", &article()));
        assert!(m("()", &article()));
    }

    // --- unstored items ----------------------------------------------------

    #[test]
    fn a_filter_action_can_match_an_item_that_has_no_id_yet() {
        // This is the reason the matcher exists at all.
        let item = Article::unstored();
        assert!(m("intitle:rust", &item));
        assert!(m("#rust", &item));
        assert!(m("-python", &item));
        assert!(m("pubdate:2006-01-02/2006-01-03", &item));
    }

    #[test]
    fn a_query_that_needs_an_id_is_refused_for_an_unstored_item() {
        let item = Article::unstored();
        // Answering `false` here would silently make every date-based filter action a no-op.
        let e = matches(&parse("date:P1W", NOW).unwrap(), &item).unwrap_err();
        assert!(matches!(e, SearchError::Unsupported(_)), "{e}");
        assert!(matches!(
            matches(&parse("e:1", NOW).unwrap(), &item).unwrap_err(),
            SearchError::Unsupported(_)
        ));
    }

    #[test]
    fn an_unexpanded_saved_query_is_refused() {
        let e = matches(&parse("S:3", NOW).unwrap(), &article()).unwrap_err();
        assert!(matches!(e, SearchError::Unsupported(_)), "{e}");
    }

    // --- agreement with the SQL side ---------------------------------------

    #[test]
    fn folding_happens_once_per_target_not_once_per_predicate() {
        // Behavioural, not a performance assertion: the point is that a twenty-predicate query and a
        // one-predicate query see the same folded text, so they cannot disagree with each other.
        let a = article();
        let single = m("intitle:école", &a);
        let many = m(
            "intitle:école intext:école author:école inurl:école #école intitle:ecole",
            &a,
        );
        assert!(single);
        assert!(
            !many,
            "the #école tag does not exist, so the conjunction fails"
        );
        assert!(m("intitle:ecole intext:AXUM", &a));
    }

    #[test]
    fn evaluation_is_deterministic() {
        let a = article();
        for q in ["rust", "-python", "#rust OR l:11", "intitle:/^a rust/i"] {
            assert_eq!(m(q, &a), m(q, &a), "{q}");
        }
    }

    #[test]
    fn split_folded_tags_handles_both_separators_and_blanks() {
        assert_eq!(split_folded_tags("#a #b"), vec!["a", "b"]);
        assert_eq!(split_folded_tags("#a,b"), vec!["a", "b"]);
        assert_eq!(split_folded_tags("#Étiquette"), vec!["etiquette"]);
        assert!(split_folded_tags("").is_empty());
        assert!(split_folded_tags("# #").is_empty());
    }

    #[test]
    fn range_contains_covers_every_shape() {
        let v = 100;
        assert!(range_contains(DateRange::default(), v), "unbounded matches");
        assert!(range_contains(
            DateRange {
                min: Some(50),
                max: Some(150)
            },
            v
        ));
        assert!(!range_contains(
            DateRange {
                min: Some(150),
                max: Some(200)
            },
            v
        ));
        assert!(
            range_contains(
                DateRange {
                    min: Some(100),
                    max: None
                },
                v
            ),
            "inclusive lower bound"
        );
        assert!(
            range_contains(
                DateRange {
                    min: None,
                    max: Some(100)
                },
                v
            ),
            "inclusive upper bound"
        );
        // The complemented form: min > max means "outside the band".
        assert!(!range_contains(
            DateRange {
                min: Some(151),
                max: Some(49)
            },
            v
        ));
        assert!(range_contains(
            DateRange {
                min: Some(151),
                max: Some(49)
            },
            200
        ));
        assert!(range_contains(
            DateRange {
                min: Some(151),
                max: Some(49)
            },
            10
        ));
    }
}
