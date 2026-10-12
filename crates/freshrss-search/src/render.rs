//! Rendering an AST back to query text, and the transforms that edit one.
//!
//! [`render`] exists so `POST /api/v1/search/parse` can hand the reader a canonical spelling of what
//! it typed — which is only useful if parsing that spelling again gives back the same tree. Every
//! test here asserts that round-trip, because a renderer that produces text the parser reads
//! differently would silently rewrite the user's saved searches.
//!
//! The transforms are what the reader's scope chips are built on: clicking a folder in the sidebar
//! narrows the current query to that folder without the user having to type `c:3`, and clicking it
//! again removes the narrowing. Doing that as an AST edit rather than string concatenation is the
//! point — `intitle:a OR b` narrowed to category 3 is `(intitle:a OR b) c:3`, and no amount of
//! appending gets the brackets right.

use crate::ast::{Expr, IdSet, Pred, QueryRef, TextPat};
use crate::error::SearchError;
use crate::parser::parse;

/// Renders an expression as query text.
///
/// The output is canonical rather than a reconstruction of the input: redundant brackets are dropped,
/// operators are lower-cased, and values are quoted only when they need it.
pub fn render(expr: &Expr) -> String {
    let mut out = String::new();
    write_expr(expr, &mut out, false);
    out
}

/// Renders `expr`, parenthesizing it when `inside_or` makes that necessary.
///
/// A conjunction nested inside a disjunction needs brackets; a disjunction nested inside a
/// conjunction needs them too, because juxtaposition binds tighter. A single leaf never does.
fn write_expr(expr: &Expr, out: &mut String, needs_parens: bool) {
    if needs_parens {
        out.push('(');
    }
    match expr {
        Expr::Leaf(preds) => {
            for (i, pred) in preds.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_pred(pred, out);
            }
        }
        Expr::Node { op, children } => {
            for (i, child) in children.iter().enumerate() {
                if i > 0 {
                    match op {
                        crate::ast::Op::And => out.push(' '),
                        crate::ast::Op::Or => out.push_str(" OR "),
                    }
                }
                // A child of an `Or` needs brackets if it is itself a conjunction of more than one
                // term; a child of an `And` needs them if it is a disjunction.
                let child_needs = match (op, child) {
                    (_, Expr::Not(_)) => false,
                    (crate::ast::Op::Or, Expr::Node { .. }) => true,
                    (crate::ast::Op::Or, Expr::Leaf(p)) => p.len() > 1,
                    (crate::ast::Op::And, Expr::Node { .. }) => true,
                    (crate::ast::Op::And, Expr::Leaf(_)) => false,
                };
                write_expr(child, out, child_needs);
            }
        }
        Expr::Not(inner) => {
            out.push('-');
            // A negated subtree is always bracketed. `-` applies to one atom, so the brackets are
            // load-bearing for a group — and they are load-bearing for a *single* term too, because
            // `-cd` parses back as a predicate carrying its own `negated` flag rather than as a
            // negated leaf, which is a different tree. The two are equivalent for text but not for a
            // nullable timestamp, where a missing value fails in both polarities.
            write_expr(inner, out, true);
        }
    }
    if needs_parens {
        out.push(')');
    }
}

fn write_pred(pred: &Pred, out: &mut String) {
    if pred.is_negated() {
        out.push('-');
    }
    match pred {
        Pred::EntryIds { ids, .. } => write_id_list(out, "e", ids),
        Pred::FeedIds { ids, .. } => write_id_list(out, "f", ids),
        Pred::CategoryIds { ids, .. } => write_id_list(out, "c", ids),
        Pred::SavedQuery { target, .. } => match target {
            QueryRef::Ids(ids) => write_ids(out, "S", ids),
            QueryRef::Names(names) => {
                out.push_str("search:");
                out.push_str(&quote(&names.join(",")));
            }
        },
        Pred::LabelIds { ids, .. } => write_ids(out, "l", ids),
        Pred::LabelNames { names, .. } => {
            out.push_str("labels:");
            write_patterns(out, names, ",");
        }
        Pred::Title { pat, .. } => write_keyed(out, "intitle", pat),
        Pred::Content { pat, .. } => write_keyed(out, "intext", pat),
        Pred::FreeText { pat, .. } => write_value(out, pat),
        Pred::Author { pat, .. } => write_keyed(out, "author", pat),
        Pred::Url { pat, .. } => write_keyed(out, "inurl", pat),
        Pred::Tag { pat, .. } => {
            out.push('#');
            write_value(out, pat);
        }
        Pred::Date { field, range, .. } => {
            let key = match field {
                crate::ast::DateField::Ingestion => "date",
                crate::ast::DateField::Publication => "pubdate",
                crate::ast::DateField::Modified => "mdate",
                crate::ast::DateField::UserModified => "userdate",
            };
            out.push_str(key);
            out.push(':');
            write_range(out, *range);
        }
    }
}

fn write_ids(out: &mut String, key: &str, ids: &IdSet) {
    match ids {
        IdSet::Any => {
            out.push_str(key);
            out.push_str(":*");
        }
        IdSet::List(list) => write_id_list(out, key, list),
    }
}

fn write_id_list(out: &mut String, key: &str, ids: &[i64]) {
    out.push_str(key);
    out.push(':');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
}

fn write_keyed(out: &mut String, key: &str, pat: &TextPat) {
    out.push_str(key);
    out.push(':');
    write_value(out, pat);
}

fn write_value(out: &mut String, pat: &TextPat) {
    match pat {
        TextPat::Contains(needle) => out.push_str(&quote(needle)),
        TextPat::Regex(re) => out.push_str(&re.render()),
    }
}

fn write_patterns(out: &mut String, pats: &[TextPat], sep: &str) {
    let parts: Vec<String> = pats
        .iter()
        .map(|p| match p {
            TextPat::Contains(n) => n.clone(),
            TextPat::Regex(re) => re.render(),
        })
        .collect();
    // A list of regexes cannot be joined with commas without becoming ambiguous, so each is rendered
    // separately and the whole thing is quoted only when it is a plain comma-joined list.
    if parts.iter().all(|p| !p.starts_with('/')) {
        out.push_str(&quote(&parts.join(sep)));
    } else {
        out.push_str(&parts.join(sep));
    }
}

/// Renders a date range in the interval grammar [`crate::dateinterval`] accepts.
///
/// Unix seconds rather than ISO 8601: the parser's `instant()` reads `YYYYMMDDTHHMMSS`, and emitting
/// that form round-trips exactly, whereas re-deriving an ISO string would risk a timezone shift.
///
/// An open end keeps its `/`. Without it, a lower bound alone would be re-read as a whole *day*
/// (floor and ceiling), which is a different range — so `date:P1W` would gain an upper bound on the
/// way through and silently stop matching articles added since.
fn write_range(out: &mut String, range: crate::ast::DateRange) {
    match (range.min, range.max) {
        (Some(lo), Some(hi)) => {
            out.push_str(&compact_instant(lo));
            out.push('/');
            out.push_str(&compact_instant(hi));
        }
        (Some(lo), None) => {
            out.push_str(&compact_instant(lo));
            out.push('/');
        }
        (None, Some(hi)) => {
            out.push('/');
            out.push_str(&compact_instant(hi));
        }
        (None, None) => {}
    }
}

/// `YYYYMMDDTHHMMSS`, the compact form the interval parser accepts without delimiters.
fn compact_instant(secs: i64) -> String {
    jiff::Timestamp::from_second(secs)
        .map(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y%m%dT%H%M%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// Quotes a value only when it has to be.
///
/// A value containing whitespace, a quote, a colon, a slash or a parenthesis would otherwise lex as
/// several tokens, so it is wrapped in double quotes with `"` and `\` escaped. Everything else is
/// emitted bare, which keeps the canonical form readable.
fn quote(value: &str) -> String {
    let needs = value.is_empty()
        || value.chars().any(|c| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ':' | '/' | '(' | ')' | '#' | ',')
        });
    if !needs {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// --- transforms ------------------------------------------------------------

/// Conjoins `extra` onto `expr`, bracketing the original when it is a disjunction.
///
/// This is what the sidebar's scope chips call: narrowing "everything matching `a OR b`" to one
/// folder has to produce `(a OR b) c:3`, not `a OR b c:3` — the latter would only narrow the second
/// branch and silently widen the first.
pub fn prepend(expr: &Expr, extra: Expr) -> Expr {
    if expr.is_true() {
        return extra;
    }
    if extra.is_true() {
        return expr.clone();
    }
    Expr::and(vec![expr.clone(), extra])
}

/// Removes from `expr` every predicate equal to `target`, at any depth.
///
/// Used when a scope chip is dismissed. A subtree left with no predicates collapses to its operator's
/// identity through [`Expr::and`]/[`Expr::or`], so removing the only term of a branch removes the
/// branch rather than leaving a `1=1` behind.
pub fn remove(expr: &Expr, target: &Pred) -> Expr {
    match expr {
        Expr::Leaf(preds) => {
            let kept: Vec<Pred> = preds.iter().filter(|p| *p != target).cloned().collect();
            Expr::Leaf(kept)
        }
        Expr::Node { op, children } => {
            let kept: Vec<Expr> = children
                .iter()
                .map(|c| remove(c, target))
                // A leaf emptied by removal must disappear, not propagate. Left in place it would be
                // the operator's identity — TRUE inside an `Or`, which absorbs every other branch and
                // turns `a OR c:3` into "match everything".
                .filter(|c| !matches!(c, Expr::Leaf(preds) if preds.is_empty()))
                .collect();
            match op {
                crate::ast::Op::And => Expr::and(kept),
                crate::ast::Op::Or => Expr::or(kept),
            }
        }
        // A negated group is not the same as its parts negated, so the target is only removed when it
        // *is* the whole group. Recursing would turn `-(a OR b)` into `-(a)` — a different query, and
        // one the user never asked for.
        Expr::Not(inner) => {
            if matches!(&**inner, Expr::Leaf(preds) if preds.len() == 1 && preds[0] == *target) {
                Expr::TRUE
            } else {
                expr.clone()
            }
        }
    }
}

/// Ensures `target` is present, adding it only if an equal predicate is not already there.
///
/// Idempotent by construction, so a UI that calls this on every render cannot accumulate duplicate
/// terms — which would be visible as a query that grows each time the user clicks the same chip.
pub fn enforce(expr: &Expr, target: Pred) -> Expr {
    if expr.preds().contains(&&target) {
        return expr.clone();
    }
    prepend(expr, Expr::pred(target))
}

/// Parses a query, renders it canonically, and parses that again.
///
/// The round-trip check the renderer's contract rests on; exposed because the `/search/parse`
/// endpoint reports it as a warning rather than failing the request.
///
/// # Errors
///
/// Whatever [`parse`] reports for either direction.
pub fn round_trip(query: &str, now: i64) -> Result<(Expr, String, Expr), SearchError> {
    let first = parse(query, now)?;
    let canonical = render(&first);
    let second = parse(&canonical, now)?;
    Ok((first, canonical, second))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{DateField, DateRange, Op};

    const NOW: i64 = 1_136_214_245;

    fn rt(q: &str) -> String {
        let (first, canonical, second) = round_trip(q, NOW).unwrap();
        assert_eq!(first, second, "{q} -> {canonical:?} did not round-trip");
        // Rendering the re-parsed tree must be a fixed point.
        assert_eq!(render(&second), canonical, "{q} is not stable");
        canonical
    }

    fn pred(q: &str) -> Pred {
        match parse(q, NOW).unwrap() {
            Expr::Leaf(p) => p.into_iter().next().unwrap(),
            other => panic!("{other:?}"),
        }
    }

    /// A single-predicate expression, for the transforms that take trees rather than predicates.
    fn scope(q: &str) -> Expr {
        Expr::pred(pred(q))
    }

    // --- rendering ---------------------------------------------------------

    #[test]
    fn a_single_term_renders_bare() {
        assert_eq!(rt("rust"), "rust");
        assert_eq!(rt("intitle:rust"), "intitle:rust");
        assert_eq!(rt("f:1"), "f:1");
        assert_eq!(rt("#tag"), "#tag");
    }

    #[test]
    fn a_value_with_a_space_is_quoted() {
        assert_eq!(rt(r#"intitle:"hello world""#), r#"intitle:"hello world""#);
        assert_eq!(rt("author:ada"), "author:ada");
        assert_eq!(rt(r#"author:"ada lovelace""#), r#"author:"ada lovelace""#);
    }

    #[test]
    fn a_value_containing_a_delimiter_is_quoted() {
        // Without quoting, these would lex back as several tokens.
        for q in [
            r#"intitle:"a/b""#,
            r#"intitle:"a:b""#,
            r##"intitle:"#hash""##,
            r#"intitle:"(paren)""#,
            r#"intitle:"comma,separated""#,
        ] {
            let canonical = rt(q);
            assert!(canonical.contains('"'), "{q} -> {canonical}");
        }
    }

    #[test]
    fn a_quote_inside_a_value_is_escaped() {
        assert_eq!(rt(r#"intitle:"say \"hi\"""#), r#"intitle:"say \"hi\"""#);
        assert_eq!(rt(r#"intitle:"back\\slash""#), r#"intitle:"back\\slash""#);
    }

    #[test]
    fn an_empty_query_renders_empty() {
        assert_eq!(rt(""), "");
    }

    #[test]
    fn a_regex_renders_with_its_delimiters_and_flags() {
        assert_eq!(rt("intitle:/^ru.t$/i"), "intitle:/^ru.t$/i");
        assert_eq!(rt("#/^ru/"), "#/^ru/");
        assert_eq!(rt("/a|b/"), "/a|b/");
    }

    #[test]
    fn negation_renders_as_a_leading_minus() {
        assert_eq!(rt("-rust"), "-rust");
        assert_eq!(rt("-intitle:x"), "-intitle:x");
        assert_eq!(rt("-#tag"), "-#tag");
        assert_eq!(rt("-f:1"), "-f:1");
    }

    #[test]
    fn a_negated_group_keeps_its_brackets() {
        // `-` binds to one atom, so the brackets are load-bearing.
        let canonical = rt("-(a OR b)");
        assert!(canonical.starts_with("-("), "{canonical}");
    }

    #[test]
    fn a_negated_group_of_a_single_term_keeps_its_brackets_too() {
        // `!(cd)` is a negated *leaf*, which is a different tree from the flagged predicate `-cd`.
        // The two agree on text but not on a nullable timestamp, where a missing value fails in both
        // polarities — so dropping the brackets would quietly rewrite one into the other.
        let group = parse("!(cd)", NOW).unwrap();
        assert!(matches!(group, Expr::Not(_)), "{group:?}");
        assert_ne!(parse("-cd", NOW).unwrap(), group);
        assert_eq!(rt("!(cd)"), "-(cd)");
        assert_eq!(rt("ab !(cd)"), "ab -(cd)");
    }

    #[test]
    fn conjunctions_render_as_juxtaposition() {
        assert_eq!(rt("a b c"), "a b c");
        assert_eq!(rt("intitle:a intext:b"), "intitle:a intext:b");
    }

    #[test]
    fn disjunctions_render_with_or() {
        assert_eq!(rt("a OR b"), "a OR b");
        assert_eq!(rt("a OR b OR c"), "a OR b OR c");
    }

    #[test]
    fn mixed_operators_get_the_brackets_they_need() {
        // `(a AND b) OR c` must keep the conjunction together.
        let canonical = rt("a b OR c");
        assert_eq!(canonical, "(a b) OR c");

        let canonical = rt("a OR (b c)");
        assert_eq!(canonical, "a OR (b c)");

        let canonical = rt("(a OR b) c");
        assert_eq!(canonical, "(a OR b) c");
    }

    #[test]
    fn redundant_brackets_are_dropped() {
        assert_eq!(rt("((a))"), "a");
        assert_eq!(rt("(a OR b)"), "a OR b");
    }

    #[test]
    fn an_empty_group_renders_as_nothing() {
        // `()` is the And identity, which renders as the empty query.
        assert_eq!(rt("()"), "");
    }

    #[test]
    fn every_operator_round_trips() {
        for q in [
            "e:1,2",
            "f:1,2",
            "c:1,2",
            "S:1,2",
            "S:*",
            "l:1,2",
            "l:*",
            "labels:todo",
            "intitle:x",
            "intext:x",
            "author:x",
            "inurl:x",
            "#tag",
            "date:20060101T000000/20060131T235959",
            "pubdate:20060101T000000",
            "mdate:20060101T000000",
            "userdate:20060101T000000",
        ] {
            let canonical = rt(q);
            assert!(!canonical.is_empty(), "{q} rendered empty");
        }
    }

    #[test]
    fn the_wildcard_forms_round_trip() {
        assert_eq!(rt("l:*"), "l:*");
        assert_eq!(rt("S:*"), "S:*");
    }

    #[test]
    fn a_date_range_renders_in_compact_form() {
        let canonical = rt("date:2006-01-02/2006-01-05");
        assert!(canonical.starts_with("date:2006"), "{canonical}");
        assert!(canonical.contains('/'), "{canonical}");
        // And it reads back as the same instant.
        let again = parse(&canonical, NOW).unwrap();
        assert_eq!(parse("date:2006-01-02/2006-01-05", NOW).unwrap(), again);
    }

    #[test]
    fn a_single_date_renders_as_a_full_day_band() {
        // `date:2006-01-02` is a period, so the parser already gives it both bounds; the canonical
        // form makes that explicit rather than relying on the reader to know.
        let canonical = rt("date:2006-01-02");
        assert!(canonical.contains('/'), "{canonical}");
        assert!(canonical.starts_with("date:20060102"), "{canonical}");
    }

    #[test]
    fn an_open_ended_interval_keeps_its_slash() {
        // Losing the trailing `/` would make the lower bound re-read as a whole day and silently
        // add an upper bound that was never there.
        let canonical = rt("date:2006-01-02/");
        assert!(canonical.ends_with('/'), "{canonical}");
        assert_eq!(
            parse(&canonical, NOW).unwrap(),
            parse("date:2006-01-02/", NOW).unwrap()
        );
    }

    #[test]
    fn a_negated_date_renders_as_the_positive_form_with_a_minus() {
        // The parser complements the range; the renderer has to un-complement it, or the canonical
        // form would carry an impossible `min > max` band with a `-` in front of it.
        let canonical = rt("-date:2006-01-02/2006-01-05");
        assert!(canonical.starts_with("-date:"), "{canonical}");
        assert_eq!(
            parse(&canonical, NOW).unwrap(),
            parse("-date:2006-01-02/2006-01-05", NOW).unwrap()
        );
    }

    #[test]
    fn a_complex_realistic_query_round_trips() {
        let q = r#"(intitle:rust OR #axum) -author:"bot account" f:12,13 date:P1W"#;
        // `date:P1W` is relative to `now`, so it renders as an absolute band; the tree still matches.
        let first = parse(q, NOW).unwrap();
        let canonical = render(&first);
        assert_eq!(parse(&canonical, NOW).unwrap(), first);
        assert!(canonical.contains("intitle:rust"), "{canonical}");
        assert!(
            canonical.contains(r#"-author:"bot account""#),
            "{canonical}"
        );
        assert!(canonical.contains("f:12,13"), "{canonical}");
    }

    // --- transforms --------------------------------------------------------

    #[test]
    fn prepend_narrows_a_disjunction_with_brackets() {
        let base = parse("a OR b", NOW).unwrap();
        let narrowed = prepend(&base, scope("c:3"));
        let text = render(&narrowed);
        assert_eq!(text, "(a OR b) c:3");
        // And it really is a conjunction of the group with the scope.
        assert!(matches!(narrowed, Expr::Node { op: Op::And, .. }));
    }

    #[test]
    fn prepend_onto_an_empty_query_is_just_the_scope() {
        assert_eq!(render(&prepend(&Expr::TRUE, scope("c:3"))), "c:3");
        assert_eq!(
            render(&prepend(&parse("intitle:x", NOW).unwrap(), Expr::TRUE)),
            "intitle:x"
        );
    }

    #[test]
    fn prepend_is_associative_enough_for_repeated_narrowing() {
        let once = prepend(&parse("a OR b", NOW).unwrap(), scope("c:3"));
        let twice = prepend(&once, scope("f:7"));
        let text = render(&twice);
        // Both scopes apply to the whole disjunction.
        assert!(text.contains("c:3") && text.contains("f:7"), "{text}");
        assert!(
            text.starts_with('('),
            "the disjunction stays grouped: {text}"
        );
    }

    #[test]
    fn remove_drops_a_predicate_at_any_depth() {
        let base = parse("intitle:a c:3 f:7", NOW).unwrap();
        let removed = remove(&base, &pred("c:3"));
        assert_eq!(render(&removed), "intitle:a f:7");
    }

    #[test]
    fn remove_collapses_a_branch_left_empty() {
        let base = parse("a OR c:3", NOW).unwrap();
        let removed = remove(&base, &pred("c:3"));
        // The `Or` now has one child, which flattens to that child rather than leaving a `1=0`.
        assert_eq!(render(&removed), "a");
    }

    #[test]
    fn remove_of_everything_yields_the_identity() {
        let base = parse("c:3", NOW).unwrap();
        assert!(remove(&base, &pred("c:3")).is_true());
    }

    #[test]
    fn remove_leaves_an_unrelated_query_alone() {
        let base = parse("intitle:a", NOW).unwrap();
        assert_eq!(remove(&base, &pred("c:3")), base);
    }

    #[test]
    fn remove_does_not_reach_inside_a_negated_group() {
        // `-(a OR b)` is not `(-a OR -b)`. Removing `b` from inside would yield `-(a)`, a different
        // query, so only the whole group is removable.
        let base = parse("-(a OR b)", NOW).unwrap();
        let inner = pred("b");
        assert_eq!(remove(&base, &inner), base);
        // But the group as a whole can be removed by matching its own predicate form.
        let whole = Pred::FreeText {
            pat: TextPat::contains("a"),
            negated: false,
        };
        assert_eq!(
            remove(&base, &whole),
            base,
            "a bare `a` is not the negated group"
        );
    }

    #[test]
    fn enforce_is_idempotent() {
        let base = parse("intitle:a", NOW).unwrap();
        let once = enforce(&base, pred("c:3"));
        let twice = enforce(&once, pred("c:3"));
        assert_eq!(once, twice, "a second enforce must not duplicate the term");
        assert_eq!(render(&twice), "intitle:a c:3");
    }

    #[test]
    fn enforce_adds_what_is_missing() {
        let base = parse("intitle:a", NOW).unwrap();
        let enforced = enforce(&base, pred("f:7"));
        assert_eq!(render(&enforced), "intitle:a f:7");
    }

    #[test]
    fn enforce_and_remove_are_inverses() {
        let base = parse("intitle:a OR intext:b", NOW).unwrap();
        let scope = pred("c:3");
        let enforced = enforce(&base, scope.clone());
        assert_ne!(enforced, base);
        assert_eq!(remove(&enforced, &scope), base);
    }

    #[test]
    fn a_round_trip_is_reported_as_a_triple() {
        let (first, canonical, second) = round_trip("a OR b", NOW).unwrap();
        assert_eq!(first, second);
        assert_eq!(canonical, "a OR b");
    }

    #[test]
    fn a_round_trip_of_an_invalid_query_reports_the_parse_error() {
        let e = round_trip("(a", NOW).unwrap_err();
        assert!(matches!(e, SearchError::UnbalancedParen { .. }), "{e}");
    }

    #[test]
    fn rendering_never_emits_an_unparseable_string() {
        // Property-flavoured smoke test over the shapes a UI can build.
        let queries = [
            "a",
            "-a",
            "a b",
            "a OR b",
            "(a OR b) c",
            "-(a OR b)",
            "f:1,2 c:3",
            r#"intitle:"a b" author:"c d""#,
            "l:* -l:1",
            "#x #y",
            "intitle:/a|b/i",
        ];
        for q in queries {
            let first = parse(q, NOW).unwrap();
            let canonical = render(&first);
            let second = parse(&canonical, NOW).unwrap_or_else(|e| {
                panic!("{q} rendered to {canonical:?}, which failed to parse: {e}")
            });
            assert_eq!(first, second, "{q} -> {canonical}");
        }
    }

    #[test]
    fn a_date_predicate_survives_render_and_parse() {
        let p = Pred::Date {
            field: DateField::Publication,
            range: DateRange {
                min: Some(1_136_073_600),
                max: Some(1_138_751_999),
            },
            negated: false,
        };
        let text = render(&Expr::pred(p.clone()));
        assert_eq!(parse(&text, NOW).unwrap(), Expr::Leaf(vec![p]));
    }
}
