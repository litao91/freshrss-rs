//! The minimum feed priority a query needs in order to see everything it names.
//!
//! Port of `FreshRSS_Search::needVisibility()` and `FreshRSS_BooleanSearch::needVisibility()`.
//!
//! A listing scope filters feeds by priority before the search runs — the main stream shows only
//! `priority >= 10`, so a hidden feed is invisible to it. That is right for browsing and wrong for an
//! explicit query: `f:12` naming a hidden feed must find that feed, or the reader reports "no results"
//! for an article the user can see in the sidebar. This module computes how far the floor has to be
//! lowered, and nothing further.

use crate::ast::{Expr, Pred};

/// `PRIORITY_HIDDEN`. A feed explicitly named by id is always in scope, however low its priority.
pub const PRIORITY_HIDDEN: i8 = -10;
/// `PRIORITY_CATEGORY`. A feed reached through its category is in scope down to category priority.
pub const PRIORITY_CATEGORY: i8 = 0;
/// `PRIORITY_IMPORTANT`. The sentinel the walk starts from; anything at or above it means "no
/// special visibility needed".
pub const PRIORITY_IMPORTANT: i8 = 20;

/// The lowest feed priority this query needs, or `None` when the listing's own floor is fine.
///
/// The answer is the **minimum** over the whole tree, because a disjunction only needs one branch to
/// be reachable — but every branch has to be *able* to contribute, so a query naming both a hidden
/// feed and a category needs the lower of the two floors.
///
/// Note what does *not* lower the floor: `intitle:`, `#tag`, `labels:`, date ranges. Those are
/// properties of articles, not of feeds, and widening the feed set for them would pull archived and
/// hidden feeds into every ordinary search.
pub fn min_priority(expr: &Expr) -> Option<i8> {
    let mut lowest = PRIORITY_IMPORTANT;
    let mut found = false;

    for pred in expr.preds() {
        let priority = match pred {
            // A negated feed id does not lower the floor: `-f:12` excludes a feed, so there is no
            // reason to widen the listing to reach it. FreshRSS ignores the polarity here too, but
            // honouring it avoids dragging hidden feeds into every exclusion query.
            Pred::FeedIds {
                ids,
                negated: false,
            } if !ids.is_empty() => PRIORITY_HIDDEN,
            Pred::CategoryIds {
                ids,
                negated: false,
            } if !ids.is_empty() => PRIORITY_CATEGORY,
            _ => continue,
        };
        lowest = lowest.min(priority);
        found = true;
    }

    (found && lowest < PRIORITY_IMPORTANT).then_some(lowest)
}

/// True when the query names specific feeds, which is what lets a caller skip the priority filter
/// entirely rather than lowering it.
pub fn names_feeds(expr: &Expr) -> bool {
    expr.preds()
        .iter()
        .any(|p| matches!(p, Pred::FeedIds { ids, negated: false } if !ids.is_empty()))
}

/// True when the query names categories.
pub fn names_categories(expr: &Expr) -> bool {
    expr.preds()
        .iter()
        .any(|p| matches!(p, Pred::CategoryIds { ids, negated: false } if !ids.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    const NOW: i64 = 1_136_214_245;

    fn v(q: &str) -> Option<i8> {
        min_priority(&parse(q, NOW).unwrap())
    }

    #[test]
    fn an_ordinary_query_does_not_widen_the_listing() {
        for q in [
            "",
            "rust",
            "intitle:rust",
            "#tag",
            "labels:todo",
            "date:P1W",
            "author:ada",
            "-rust",
            "a OR b",
        ] {
            assert_eq!(v(q), None, "{q} should not lower the floor");
        }
    }

    #[test]
    fn naming_a_feed_reaches_hidden_feeds() {
        assert_eq!(v("f:12"), Some(PRIORITY_HIDDEN));
        assert_eq!(v("f:1,2"), Some(PRIORITY_HIDDEN));
    }

    #[test]
    fn naming_a_category_reaches_category_priority_feeds() {
        assert_eq!(v("c:3"), Some(PRIORITY_CATEGORY));
        assert_eq!(v("c:3,4"), Some(PRIORITY_CATEGORY));
    }

    #[test]
    fn the_lower_of_the_two_wins() {
        // A feed id reaches further down than a category, so a query naming both needs the feed floor.
        assert_eq!(v("f:12 c:3"), Some(PRIORITY_HIDDEN));
        assert_eq!(v("c:3 f:12"), Some(PRIORITY_HIDDEN));
    }

    #[test]
    fn either_branch_of_an_or_can_lower_the_floor() {
        assert_eq!(v("rust OR f:12"), Some(PRIORITY_HIDDEN));
        assert_eq!(v("(a OR b) OR c:3"), Some(PRIORITY_CATEGORY));
    }

    #[test]
    fn a_negated_id_does_not_widen_the_listing() {
        // `-f:12` excludes a feed. Lowering the floor to reach it would pull every hidden feed into
        // the result set and then remove one, which is both wasteful and wrong: articles from other
        // hidden feeds would appear.
        assert_eq!(v("-f:12"), None);
        assert_eq!(v("-c:3"), None);
        // But a positive one alongside it still counts.
        assert_eq!(v("-f:12 f:13"), Some(PRIORITY_HIDDEN));
    }

    #[test]
    fn an_empty_id_list_does_not_widen_the_listing() {
        // `f:` matches nothing, so there is no feed to reach.
        assert_eq!(v("f:"), None);
        assert_eq!(v("c:"), None);
    }

    #[test]
    fn nesting_does_not_hide_a_named_feed() {
        assert_eq!(v("(((f:12)))"), Some(PRIORITY_HIDDEN));
        assert_eq!(v("a (b OR (c f:12))"), Some(PRIORITY_HIDDEN));
    }

    #[test]
    fn the_predicates_report_what_they_name() {
        assert!(names_feeds(&parse("f:1", NOW).unwrap()));
        assert!(!names_feeds(&parse("-f:1", NOW).unwrap()));
        assert!(!names_feeds(&parse("c:1", NOW).unwrap()));
        assert!(names_categories(&parse("c:1", NOW).unwrap()));
        assert!(!names_categories(&parse("-c:1", NOW).unwrap()));
        assert!(!names_categories(&parse("f:1", NOW).unwrap()));
    }

    #[test]
    fn the_priority_constants_match_the_server_ladder() {
        // These are the FreshRSS values; if the server's `domain::priority` ever changes them, the
        // listing floor and this module would disagree and hidden feeds would silently vanish.
        assert_eq!(PRIORITY_HIDDEN, -10);
        assert_eq!(PRIORITY_CATEGORY, 0);
        assert_eq!(PRIORITY_IMPORTANT, 20);
    }
}
