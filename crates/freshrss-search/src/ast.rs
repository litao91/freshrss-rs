//! The query AST: one tree, two backends.
//!
//! [`crate::sql`] compiles it to a SQL fragment and [`crate::matcher`] evaluates it in memory. Both
//! read the same nodes and both call [`crate::normalize::fold`] before comparing text, which is what
//! makes them agree *by construction* rather than by coincidence. FreshRSS's two paths do not agree —
//! its SQL uses ASCII-only `LIKE` while its in-memory matcher accent-folds — so `intitle:ecole`
//! matches "École" for filter actions but not for listings. That divergence is unfixable without a
//! shared tree, which is why the tree is the centre of this crate.
//!
//! Everything here is **plaintext**. No field holds an HTML-encoded or SQL-escaped value; escaping
//! happens at the boundary in `sql.rs` (as a bound parameter) and never in `matcher.rs`.

use serde::{Deserialize, Serialize};

use crate::regex::Pattern;

/// A node in the boolean tree.
///
/// Serializes with external tags (`{"leaf": […]}` / `{"node": {"op": …, "children": […]}}`), which
/// is what `POST /api/v1/search/parse` returns. Internal tagging is not an option here: a `Leaf`
/// holds a sequence, and serde cannot place a sequence under a tag key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    /// A boolean combination of children. An empty `children` list is [`Expr::TRUE`] or
    /// [`Expr::FALSE`] depending on `op`, and the smart constructors enforce that.
    Node { op: Op, children: Vec<Expr> },
    /// A set of predicates that must *all* hold. This is the leaf of the tree: `intitle:a b` is one
    /// leaf holding two title predicates, not an `And` node.
    Leaf(Vec<Pred>),
    /// Negation of a whole subtree.
    ///
    /// Only used for a negated *group* — `!(a OR b)`. A negated single term carries its flag inside
    /// the predicate instead, which is both what FreshRSS does and what keeps the emitted SQL flat:
    /// `intitle:a -intitle:b` becomes `title LIKE … AND title NOT LIKE …`, not `… AND NOT (…)`.
    Not(Box<Expr>),
}

/// How a [`Expr::Node`]'s children combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    And,
    Or,
}

impl Op {
    /// The identity element: `And` of nothing is true, `Or` of nothing is false.
    pub const fn neutral(self) -> Expr {
        match self {
            Op::And => Expr::Leaf(Vec::new()),
            Op::Or => Expr::FALSE,
        }
    }

    pub const fn is_and(self) -> bool {
        matches!(self, Op::And)
    }
}

impl Expr {
    /// Matches everything. The neutral element of `And`.
    pub const TRUE: Expr = Expr::Leaf(Vec::new());
    /// Matches nothing. Expressed as an `Or` with no children, because a `Leaf` with an empty
    /// predicate list already means "true" and the two must stay distinguishable.
    pub const FALSE: Expr = Expr::Node {
        op: Op::Or,
        children: Vec::new(),
    };

    pub fn is_true(&self) -> bool {
        matches!(self, Expr::Leaf(preds) if preds.is_empty())
    }

    pub fn is_false(&self) -> bool {
        matches!(self, Expr::Node { op: Op::Or, children } if children.is_empty())
    }

    /// Negation with the double-negative and constant cases collapsed.
    pub fn negate(child: Expr) -> Expr {
        match child {
            c if c.is_true() => Expr::FALSE,
            c if c.is_false() => Expr::TRUE,
            Expr::Not(inner) => *inner,
            other => Expr::Not(Box::new(other)),
        }
    }

    /// Conjunction with flattening and short-circuiting.
    ///
    /// Flattening nested `And`s keeps the tree shallow, which matters for the SQL compiler: a
    /// right-nested chain of twenty terms would otherwise emit twenty levels of parentheses.
    pub fn and(children: Vec<Expr>) -> Expr {
        combine(Op::And, children)
    }

    /// Disjunction with flattening and short-circuiting.
    pub fn or(children: Vec<Expr>) -> Expr {
        combine(Op::Or, children)
    }

    /// A single-predicate leaf.
    pub fn pred(pred: Pred) -> Expr {
        Expr::Leaf(vec![pred])
    }

    /// Every predicate in this tree, in document order.
    ///
    /// Used by [`crate::visibility`] and by the explain endpoint, both of which need to look at what
    /// the query mentions without caring how it is combined.
    pub fn preds(&self) -> Vec<&Pred> {
        let mut out = Vec::new();
        self.collect_preds(&mut out);
        out
    }

    fn collect_preds<'a>(&'a self, out: &mut Vec<&'a Pred>) {
        match self {
            Expr::Leaf(preds) => out.extend(preds),
            Expr::Node { children, .. } => {
                for child in children {
                    child.collect_preds(out);
                }
            }
            Expr::Not(inner) => inner.collect_preds(out),
        }
    }
}

/// Flattens same-`op` children, drops identities, and collapses on annihilators.
///
/// `Or` short-circuits on a `TRUE` child and drops `FALSE` ones; `And` does the reverse. Without
/// this, `a OR (b OR c)` stays nested and `x AND TRUE` emits a pointless `AND 1=1`.
fn combine(op: Op, children: Vec<Expr>) -> Expr {
    let mut flat: Vec<Expr> = Vec::with_capacity(children.len());

    for child in children {
        match child {
            // Absorbing element: `x OR TRUE` is TRUE, `x AND FALSE` is FALSE.
            c if c.is_true() && !op.is_and() => return Expr::TRUE,
            c if c.is_false() && op.is_and() => return Expr::FALSE,
            // Identity: contributes nothing.
            c if c.is_true() || c.is_false() => continue,
            // Flatten a nested node of the same operator.
            Expr::Node {
                op: child_op,
                children: grand,
            } if child_op == op => flat.extend(grand),
            other => flat.push(other),
        }
    }

    // A conjunction of leaves is one leaf: see `merge_adjacent_leaves`.
    if op.is_and() {
        flat = merge_adjacent_leaves(flat);
    }

    match flat.len() {
        0 => op.neutral(),
        1 => flat.pop().expect("len 1"),
        _ => Expr::Node { op, children: flat },
    }
}

/// Merges runs of adjacent [`Expr::Leaf`] children of a conjunction into a single leaf.
///
/// A leaf already *is* an implicit AND, so two of them side by side under an `And` node say nothing
/// that one leaf holding both predicate lists does not — and [`crate::render`] writes both as plain
/// juxtaposition. Merging is what makes that faithful: `(a) (b)` renders as `a b`, which has to parse
/// back to the same tree rather than to a differently shaped equivalent, or a saved search silently
/// changes shape every time it is round-tripped through the canonical spelling.
///
/// Only *adjacent* leaves merge, so `a (b OR c) d` keeps its three children and their order.
fn merge_adjacent_leaves(children: Vec<Expr>) -> Vec<Expr> {
    let mut out: Vec<Expr> = Vec::with_capacity(children.len());
    let mut pending: Vec<Pred> = Vec::new();

    for child in children {
        match child {
            Expr::Leaf(preds) => pending.extend(preds),
            other => {
                if !pending.is_empty() {
                    out.push(Expr::Leaf(std::mem::take(&mut pending)));
                }
                out.push(other);
            }
        }
    }
    if !pending.is_empty() {
        out.push(Expr::Leaf(pending));
    }
    out
}

/// One condition on one article.
///
/// Every variant carries its own `negated` flag rather than living under a `Not` node. That is a
/// deliberate flattening: SQL has no `NOT` operator that composes well with the keyset-pagination
/// `WHERE` clause, and the De Morgan work for a negated date range has to happen somewhere — putting
/// it in the node makes the compiler a straight-line walk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pred {
    /// `e:` — specific entry ids.
    EntryIds { ids: Vec<i64>, negated: bool },
    /// `f:` — feed ids.
    FeedIds { ids: Vec<i64>, negated: bool },
    /// `c:` — category ids.
    CategoryIds { ids: Vec<i64>, negated: bool },
    /// `S:` or `search:` — a reference to a saved search (a `user_query` row).
    ///
    /// This variant exists only until [`crate::expand`] replaces it with the referenced query's own
    /// subtree; neither backend can evaluate it, and both treat an unexpanded one as an error.
    SavedQuery { target: QueryRef, negated: bool },
    /// `l:` / `L:` — a label by id. `Any` is `l:*`, meaning "has at least one label".
    LabelIds { ids: IdSet, negated: bool },
    /// `label:` / `labels:` — a label by name.
    LabelNames { names: Vec<TextPat>, negated: bool },
    /// `intitle:` — the article title.
    Title { pat: TextPat, negated: bool },
    /// `intext:` — the article body only. Distinct from [`Pred::FreeText`], which also searches the
    /// title; conflating them makes `intext:` match articles whose *title* contains the word.
    Content { pat: TextPat, negated: bool },
    /// Free text — title **or** body.
    FreeText { pat: TextPat, negated: bool },
    /// `author:` — the `author` column, which stores `"name1; name2"`.
    Author { pat: TextPat, negated: bool },
    /// `inurl:` — the article's own link.
    Url { pat: TextPat, negated: bool },
    /// `#tag` — a feed-supplied keyword, matched as a whole tag.
    Tag { pat: TextPat, negated: bool },
    /// `date:` / `pubdate:` / `mdate:` / `userdate:`.
    Date {
        field: DateField,
        range: DateRange,
        negated: bool,
    },
}

impl Pred {
    pub fn is_negated(&self) -> bool {
        match self {
            Pred::EntryIds { negated, .. }
            | Pred::FeedIds { negated, .. }
            | Pred::CategoryIds { negated, .. }
            | Pred::SavedQuery { negated, .. }
            | Pred::LabelIds { negated, .. }
            | Pred::LabelNames { negated, .. }
            | Pred::Title { negated, .. }
            | Pred::Content { negated, .. }
            | Pred::FreeText { negated, .. }
            | Pred::Author { negated, .. }
            | Pred::Url { negated, .. }
            | Pred::Tag { negated, .. }
            | Pred::Date { negated, .. } => *negated,
        }
    }

    /// The text patterns this predicate compares against, for the explain endpoint.
    pub fn patterns(&self) -> Vec<&TextPat> {
        match self {
            Pred::Title { pat, .. }
            | Pred::Content { pat, .. }
            | Pred::FreeText { pat, .. }
            | Pred::Author { pat, .. }
            | Pred::Url { pat, .. }
            | Pred::Tag { pat, .. } => vec![pat],
            Pred::LabelNames { names, .. } => names.iter().collect(),
            _ => Vec::new(),
        }
    }
}

/// What a `SavedQuery` predicate points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryRef {
    /// `S:3` or `S:1,2` — by id. `S:*` is [`IdSet::Any`], meaning every saved search.
    Ids(IdSet),
    /// `search:my query` or `label:`-style lookup by name.
    Names(Vec<String>),
}

/// An id list, or the `*` wildcard meaning "any".
///
/// `S:*` and `l:*` both exist and both mean "at least one", which is not the same as an empty list —
/// an empty `f:` means "match nothing", while `f:*` is not a valid spelling at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IdSet {
    /// Specific ids. Empty means "no ids were given", which matches nothing.
    List(Vec<i64>),
    /// The `*` form.
    Any,
}

impl IdSet {
    pub const fn is_any(&self) -> bool {
        matches!(self, IdSet::Any)
    }

    pub fn ids(&self) -> &[i64] {
        match self {
            IdSet::List(v) => v,
            IdSet::Any => &[],
        }
    }
}

/// A text comparison: either a substring match or a compiled regular expression.
///
/// The `Contains` form stores the needle **already accent-folded and lowercased** by
/// [`crate::normalize::fold`], because both backends compare it against a folded haystack. Folding
/// once at parse time rather than per comparison is what makes a query over a million rows
/// affordable, and folding in exactly one place is what keeps the two backends in agreement.
///
/// The `Regex` form stores what the user wrote and is matched against the **stored** text — see
/// [`crate::normalize::Text`].
///
/// Serializes as `{"contains": "…"}` or `{"regex": {"source": "…", "flags": {…}}}`, which is what
/// `POST /api/v1/search/parse` returns to the reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextPat {
    Contains(String),
    Regex(Pattern),
}

impl TextPat {
    /// A folded substring match.
    pub fn contains(needle: &str) -> Self {
        TextPat::Contains(crate::normalize::fold(needle))
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, TextPat::Regex(_))
    }

    /// True when `text` matches.
    ///
    /// Which half of `text` is consulted depends on the variant, and [`crate::sql`] makes the same
    /// choice: `frss_ilike` folds inside, `frss_regexp` does not.
    pub fn matches(&self, text: &crate::normalize::Text<'_>) -> bool {
        match self {
            TextPat::Contains(needle) => text.folded.contains(needle.as_str()),
            TextPat::Regex(re) => re.is_match(text.raw),
        }
    }
}

/// Which timestamp a `date:`-family operator compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateField {
    /// `date:` — **ingestion** time, held in the entry `id` (microseconds since the epoch).
    ///
    /// Surprising, and kept anyway for fidelity with FreshRSS: `date:` has always compared against
    /// the id, so `date:P1W` means "added in the last week", not "published in the last week".
    /// `pubdate:` is the publication time. The help dialog says so explicitly.
    Ingestion,
    /// `pubdate:` — the feed's publication time, from `entry.date`.
    Publication,
    /// `mdate:` — when the article was last seen changed upstream, from `entry.lastModified`.
    Modified,
    /// `userdate:` — when the user last touched it, from `entry.lastUserModified`.
    UserModified,
}

/// An inclusive timestamp range in unix seconds, either bound optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DateRange {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl DateRange {
    pub fn is_unbounded(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    /// The complement, for a negated date predicate.
    ///
    /// Applying De Morgan here rather than emitting `NOT (…)` keeps the SQL index-usable: a negated
    /// inclusive range becomes two disjoint exclusive ones joined by `OR`.
    ///
    /// The two encodings are exact complements of each other, so this is an involution — negating
    /// twice returns the original range:
    ///
    /// - `min <= max` is the band `[min, max]`;
    /// - `min > max` is everything *outside* `[max, min]`, i.e. `value < max OR value > min`.
    ///
    /// A one-sided range has no complement in the same shape — `NOT (>= 10)` is `<= 9`, not another
    /// half-open range with the same side missing — so those arms move the bound by one instead.
    pub fn negate(self) -> Self {
        match (self.min, self.max) {
            // Swapping the bounds flips between the two encodings. Off-by-one here costs the day at
            // each end of the band: `-date:d1/d2` would silently keep `d1-1` and `d2+1` out of the
            // results.
            (Some(min), Some(max)) => DateRange {
                min: Some(max),
                max: Some(min),
            },
            (Some(min), None) => DateRange {
                min: None,
                max: Some(min - 1),
            },
            (None, Some(max)) => DateRange {
                min: Some(max + 1),
                max: None,
            },
            // The complement of an unbounded range is *empty*, which `DateRange` cannot express.
            // Returning the range unchanged is the least surprising thing this function can do;
            // [`crate::sql`] settles the unbounded case before calling `negate()` rather than relying
            // on this arm.
            (None, None) => DateRange::default(),
        }
    }

    /// True when the range is in the complemented encoding, `min > max`, meaning "outside the band
    /// `[max, min]`".
    ///
    /// Only [`DateRange::negate`] produces one, and only for as long as it takes
    /// [`crate::sql::compile`] to turn it into the `OR` of two disjoint halves. The parser never
    /// builds one: `-date:d1/d2` keeps the range as written and sets `negated` instead, so the
    /// canonical text round-trips.
    pub fn is_complemented(&self) -> bool {
        match (self.min, self.max) {
            (Some(min), Some(max)) => min > max,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(p: Pred) -> Expr {
        Expr::pred(p)
    }

    fn title(word: &str) -> Pred {
        Pred::Title {
            pat: TextPat::contains(word),
            negated: false,
        }
    }

    #[test]
    fn true_and_false_are_distinguishable() {
        assert!(Expr::TRUE.is_true());
        assert!(!Expr::TRUE.is_false());
        assert!(Expr::FALSE.is_false());
        assert!(!Expr::FALSE.is_true());
    }

    #[test]
    fn and_of_nothing_is_true_and_or_of_nothing_is_false() {
        assert!(Expr::and(vec![]).is_true());
        assert!(Expr::or(vec![]).is_false());
        assert_eq!(Op::And.neutral(), Expr::TRUE);
        assert_eq!(Op::Or.neutral(), Expr::FALSE);
    }

    #[test]
    fn a_single_child_is_not_wrapped_in_a_node() {
        let e = Expr::and(vec![leaf(title("a"))]);
        assert!(matches!(e, Expr::Leaf(_)), "{e:?}");
        let e = Expr::or(vec![leaf(title("a"))]);
        assert!(matches!(e, Expr::Leaf(_)), "{e:?}");
    }

    #[test]
    fn identity_elements_are_dropped() {
        assert_eq!(
            Expr::and(vec![leaf(title("a")), Expr::TRUE]),
            leaf(title("a"))
        );
        assert_eq!(
            Expr::or(vec![leaf(title("a")), Expr::FALSE]),
            leaf(title("a"))
        );
    }

    #[test]
    fn annihilators_short_circuit() {
        assert!(Expr::or(vec![leaf(title("a")), Expr::TRUE]).is_true());
        assert!(Expr::and(vec![leaf(title("a")), Expr::FALSE]).is_false());
    }

    #[test]
    fn nested_same_operator_nodes_are_flattened() {
        // `a OR (b OR c)` becomes a three-child Or, which keeps the emitted SQL flat. (A nested
        // *conjunction* of plain terms collapses further still, into a single leaf — see
        // `adjacent_leaves_of_a_conjunction_merge_into_one`.)
        let inner = Expr::or(vec![leaf(title("b")), leaf(title("c"))]);
        let outer = Expr::or(vec![leaf(title("a")), inner]);
        match outer {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                assert_eq!(children.len(), 3, "{children:?}");
            }
            other => panic!("expected a node, got {other:?}"),
        }

        // The same for a conjunction whose children are subtrees rather than bare terms.
        let inner = Expr::and(vec![
            Expr::or(vec![leaf(title("c")), leaf(title("d"))]),
            Expr::or(vec![leaf(title("e")), leaf(title("f"))]),
        ]);
        let outer = Expr::and(vec![
            Expr::or(vec![leaf(title("a")), leaf(title("b"))]),
            inner,
        ]);
        match outer {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::And);
                assert_eq!(children.len(), 3, "{children:?}");
            }
            other => panic!("expected a node, got {other:?}"),
        }
    }

    #[test]
    fn mixing_operators_keeps_the_nesting() {
        let inner = Expr::or(vec![leaf(title("b")), leaf(title("c"))]);
        let outer = Expr::and(vec![leaf(title("a")), inner]);
        match outer {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::And);
                assert_eq!(children.len(), 2);
                assert!(matches!(children[1], Expr::Node { op: Op::Or, .. }));
            }
            other => panic!("expected a node, got {other:?}"),
        }
    }

    #[test]
    fn multiple_predicates_share_one_leaf() {
        // `intitle:a intitle:b` is a single leaf holding both, not an And node: they are already an
        // implicit conjunction and splitting them would double the emitted parentheses.
        let e = Expr::Leaf(vec![title("a"), title("b")]);
        assert_eq!(e.preds().len(), 2);
    }

    #[test]
    fn adjacent_leaves_of_a_conjunction_merge_into_one() {
        // `(a) (b)` is the same conjunction as `a b`, and `render` writes both as `a b` — so the tree
        // has to be the same too, or the canonical spelling does not round-trip.
        assert_eq!(
            Expr::and(vec![leaf(title("a")), leaf(title("b"))]),
            Expr::Leaf(vec![title("a"), title("b")])
        );
        // Merging survives a flattened nested conjunction: `a AND (b AND c)` is one leaf of three.
        assert_eq!(
            Expr::and(vec![
                leaf(title("a")),
                Expr::and(vec![leaf(title("b")), leaf(title("c"))]),
            ]),
            Expr::Leaf(vec![title("a"), title("b"), title("c")])
        );
    }

    #[test]
    fn only_adjacent_leaves_merge_and_only_under_a_conjunction() {
        // A subtree between two leaves keeps all three children, in order, so `a (b OR c) d` still
        // reads as three terms.
        match Expr::and(vec![
            leaf(title("a")),
            Expr::or(vec![leaf(title("b")), leaf(title("c"))]),
            leaf(title("d")),
        ]) {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::And);
                assert_eq!(children.len(), 3, "{children:?}");
                assert!(matches!(&children[2], Expr::Leaf(p) if p.len() == 1));
            }
            other => panic!("expected a node, got {other:?}"),
        }
        // A disjunction keeps its branches: merging two leaves under `Or` would turn "either" into
        // "both".
        assert!(matches!(
            Expr::or(vec![leaf(title("a")), leaf(title("b"))]),
            Expr::Node { op: Op::Or, .. }
        ));
    }

    #[test]
    fn preds_walks_the_whole_tree_in_order() {
        let e = Expr::and(vec![
            leaf(title("a")),
            Expr::or(vec![leaf(title("b")), Expr::and(vec![leaf(title("c"))])]),
        ]);
        let words: Vec<&str> = e
            .preds()
            .iter()
            .filter_map(|p| match p {
                Pred::Title {
                    pat: TextPat::Contains(s),
                    ..
                } => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(words, vec!["a", "b", "c"]);
    }

    #[test]
    fn negation_is_carried_by_the_predicate() {
        let p = Pred::Title {
            pat: TextPat::contains("x"),
            negated: true,
        };
        assert!(p.is_negated());
        assert!(!title("x").is_negated());
    }

    #[test]
    fn contains_folds_its_needle_once_at_construction() {
        match TextPat::contains("Café") {
            TextPat::Contains(s) => assert_eq!(s, "cafe"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_contains_needle_matches_the_folded_half() {
        let pat = TextPat::contains("ecole");
        assert!(pat.matches(&crate::normalize::Text::of("l'École")));
        assert!(!pat.matches(&crate::normalize::Text::of("college")));
    }

    #[test]
    fn a_regex_matches_the_stored_half() {
        let pat = TextPat::Regex(Pattern::parse("/École/i").expect("pattern compiles"));
        assert!(pat.matches(&crate::normalize::Text::of("l'école des chiens")));
        assert!(!pat.matches(&crate::normalize::Text::of("college")));
    }

    #[test]
    fn id_set_distinguishes_any_from_empty() {
        assert!(IdSet::Any.is_any());
        assert!(IdSet::Any.ids().is_empty());
        assert!(!IdSet::List(vec![]).is_any());
        assert_eq!(IdSet::List(vec![1, 2]).ids(), &[1, 2]);
    }

    #[test]
    fn date_range_negation_is_de_morgan() {
        // [10, 20] becomes min=20, max=10 — the complemented encoding, which both backends read as
        // `< 10 OR > 20` rather than as an impossible BETWEEN.
        let r = DateRange {
            min: Some(10),
            max: Some(20),
        };
        let n = r.negate();
        assert_eq!((n.min, n.max), (Some(20), Some(10)));
        assert!(n.is_complemented());
        assert!(!r.is_complemented());
    }

    #[test]
    fn the_band_edges_survive_negation() {
        // The off-by-one this guards: with `min=max+1, max=min-1` the complement read as
        // `< 9 OR > 21`, quietly dropping the day on each side of the excluded band. The semantics of
        // the encoding are asserted against `range_contains` in `matcher.rs`.
        let n = DateRange {
            min: Some(10),
            max: Some(20),
        }
        .negate();
        assert_eq!(n.max, Some(10), "`value < 10` is the lower half");
        assert_eq!(n.min, Some(20), "`value > 20` is the upper half");
    }

    #[test]
    fn negating_twice_returns_the_original_range() {
        for r in [
            DateRange {
                min: Some(10),
                max: Some(20),
            },
            DateRange {
                min: Some(10),
                max: None,
            },
            DateRange {
                min: None,
                max: Some(20),
            },
        ] {
            assert_eq!(r.negate().negate(), r, "{r:?}");
        }
    }

    #[test]
    fn a_one_sided_range_negates_to_the_other_side() {
        let at_least = DateRange {
            min: Some(10),
            max: None,
        };
        let n = at_least.negate();
        assert_eq!((n.min, n.max), (None, Some(9)), "NOT (>= 10) is <= 9");
        assert!(!n.is_complemented(), "a half-open range is not complemented");

        let at_most = DateRange {
            min: None,
            max: Some(20),
        };
        let n = at_most.negate();
        assert_eq!((n.min, n.max), (Some(21), None), "NOT (<= 20) is >= 21");
    }

    #[test]
    fn negating_twice_returns_the_original_for_one_sided_ranges() {
        let r = DateRange {
            min: Some(10),
            max: None,
        };
        assert_eq!(r.negate().negate(), r);
    }

    #[test]
    fn an_unbounded_range_stays_unbounded_when_negated() {
        let r = DateRange::default();
        assert!(r.is_unbounded());
        assert!(r.negate().is_unbounded());
    }

    #[test]
    fn the_ast_serializes_for_the_parse_endpoint() {
        // `POST /api/v1/search/parse` returns this to the reader, so the shape is part of the API.
        //
        // Built by hand rather than through `Expr::and`, which would merge two bare leaves into one
        // (see `adjacent_leaves_of_a_conjunction_merge_into_one`); this test is about how a *node*
        // serializes, and a hand-built one is exactly what the endpoint can be handed by a caller.
        let e = Expr::Node {
            op: Op::And,
            children: vec![
                leaf(title("rust")),
                leaf(Pred::Date {
                    field: DateField::Ingestion,
                    range: DateRange {
                        min: Some(1),
                        max: None,
                    },
                    negated: false,
                }),
            ],
        };
        // `Expr` is externally tagged: a `Leaf` holds a sequence of predicates, which serde cannot
        // represent under an internal tag.
        let json = serde_json::to_value(&e).unwrap();
        let node = &json["node"];
        assert_eq!(node["op"], "and");
        assert!(json.get("leaf").is_none(), "a node is not a leaf");
        let children = node["children"].as_array().unwrap();
        assert_eq!(children[0]["leaf"][0]["kind"], "title");
        assert_eq!(children[1]["leaf"][0]["kind"], "date");
        assert_eq!(children[1]["leaf"][0]["field"], "ingestion");
    }

    #[test]
    fn the_ast_round_trips_through_json() {
        let e = Expr::or(vec![
            leaf(Pred::FreeText {
                pat: TextPat::contains("a"),
                negated: false,
            }),
            leaf(Pred::LabelIds {
                ids: IdSet::Any,
                negated: true,
            }),
        ]);
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), e);
    }

    #[test]
    fn date_fields_are_all_distinct_in_json() {
        let fields = [
            (DateField::Ingestion, "ingestion"),
            (DateField::Publication, "publication"),
            (DateField::Modified, "modified"),
            (DateField::UserModified, "user_modified"),
        ];
        for (field, name) in fields {
            let json = serde_json::to_value(field).unwrap();
            assert_eq!(json, name);
        }
    }
}
