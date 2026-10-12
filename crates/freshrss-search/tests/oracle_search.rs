//! Oracle translation of FreshRSS's two PHP search suites.
//!
//! Every data-provider row of
//!
//! - `tests/app/Models/SearchTest.php`
//! - `tests/app/Models/BooleanSearchTest.php`
//!
//! is represented below, in the same order and with the same section comments, so the two files can
//! be diffed by eye. The PHP input strings are verbatim.
//!
//! What is *asserted* differs, because the PHP providers assert on SQL text built against FreshRSS's
//! MySQL/PostgreSQL/SQLite schemas (`e.title LIKE ?` with `%needle%`, `e.title ~ ?`,
//! `REGEXP_LIKE(e.title,?,'mc')`, …) and this port deliberately emits none of that: one tree drives
//! `frss_ilike`/`frss_ieq`/`frss_regexp` UDFs instead. So each row pins the three things that are
//! portable:
//!
//! 1. the **tree shape** — which predicates, with which polarity, grouped under which operator
//!    ([`shape`]);
//! 2. the **canonical text** — [`render`], the closest thing this port has to a stable string form;
//! 3. the **bound values** — [`compile`]'s parameters, which is where the PHP suites pin a date
//!    endpoint, an id list, or the fact that a needle is compared case- and accent-insensitively.
//!
//! Rows where this port is *meant* to differ carry a `// Divergence:` comment. Rows that assert
//! nothing portable (a MySQL-specific operator spelling) say so and are folded into the nearest
//! dialect-independent assertion.

use std::collections::HashMap;

use freshrss_search::ast::{DateField, DateRange, Expr, IdSet, Op, Pred, QueryRef, TextPat};
use freshrss_search::expand::{expand, QueryResolver};
use freshrss_search::render::{enforce, prepend, remove, render};
use freshrss_search::sql::{compile, Fragment, Param, SqlCtx};
use freshrss_search::visibility::{min_priority, PRIORITY_CATEGORY, PRIORITY_HIDDEN};
use freshrss_search::{lexer, parse, parser, SearchError};

/// The reference instant for relative intervals, injected so every row is reproducible.
///
/// The same value the server crate's equivalence test uses. Only `date:P30D` in
/// [`the_whole_operator_vocabulary_renders_canonically`] depends on it.
const NOW: i64 = 1_700_000_000;

/// `date:` compares against the entry id, which is a microsecond wall clock. Every other date field
/// is in seconds.
const MICROS: i64 = 1_000_000;

// --- helpers -----------------------------------------------------------------

fn ctx() -> SqlCtx<'static> {
    SqlCtx {
        alias: "e",
        entrytag_alias: "et",
        scoped_to_user: true,
    }
}

fn parse_ok(query: &str) -> Expr {
    parse(query, NOW).unwrap_or_else(|e| panic!("{query:?} should parse: {e}"))
}

fn fragment(query: &str) -> Fragment {
    compile(&parse_ok(query), &ctx()).unwrap_or_else(|e| panic!("{query:?} should compile: {e}"))
}

fn params(query: &str) -> Vec<Param> {
    fragment(query).params
}

fn sql(query: &str) -> String {
    fragment(query).sql
}

fn text(query: &str) -> String {
    render(&parse_ok(query))
}

/// An RFC 3339 instant as unix seconds — how the PHP rows spell their expectations
/// (`strtotime('2007-03-01T13:00:00Z')`).
fn at(iso: &str) -> i64 {
    iso.parse::<jiff::Timestamp>()
        .unwrap_or_else(|e| panic!("{iso:?}: {e}"))
        .as_second()
}

/// The same instant in microseconds, which is what `date:` binds. PHP spells this
/// `strtotime('…') . '000000'`.
fn micros(iso: &str) -> i64 {
    at(iso) * MICROS
}

const fn int(value: i64) -> Param {
    Param::Int(value)
}

fn text_param(value: &str) -> Param {
    Param::Text(value.to_owned())
}

/// An expected bound value in a `static` table, where a `Param::Text` cannot be constructed.
enum Want {
    /// A text needle, already in the form the port must bind (folded, literal, or an inlined
    /// pattern).
    T(&'static str),
    I(i64),
}

const fn t(needle: &'static str) -> Want {
    Want::T(needle)
}

const fn i(value: i64) -> Want {
    Want::I(value)
}

fn wanted(row: &[Want]) -> Vec<Param> {
    row.iter()
        .map(|w| match w {
            Want::T(s) => text_param(s),
            Want::I(v) => Param::Int(*v),
        })
        .collect()
}

/// One row of a provider: the verbatim PHP input, the tree it must produce, the canonical text, and
/// the values that must be bound.
///
/// A row this port rejects instead puts `Err <Variant>` in the shape column — the PHP suite has no
/// error rows because PHP folds almost every malformed query into free text rather than reporting
/// it, so the rejection is part of what the row asserts here.
type Row = (&'static str, &'static str, &'static str, &'static [Want]);

/// Asserts one [`Row`].
fn check_row(row: &Row) {
    let (input, want_shape, want_render, want_params) = row;
    if let Some(variant) = want_shape.strip_prefix("Err ") {
        let err = match parse(input, NOW) {
            Ok(expr) => panic!("{input:?} should be rejected, got {}", shape(&expr)),
            Err(e) => e,
        };
        assert_eq!(err_name(&err), variant, "{input:?} -> {err}");
        return;
    }
    let expr = parse_ok(input);
    assert_eq!(shape(&expr), *want_shape, "{input:?} tree");
    assert_eq!(render(&expr), *want_render, "{input:?} canonical text");
    // The canonical spelling has to read back as the same tree, or saving a search would rewrite it.
    assert_eq!(
        parse(want_render, NOW).unwrap_or_else(|e| panic!("{want_render:?}: {e}")),
        expr,
        "{input:?} does not round-trip"
    );
    match compile(&expr, &ctx()) {
        Ok(f) => {
            assert_eq!(f.params, wanted(want_params), "{input:?} bound values");
            assert_eq!(
                f.sql.matches('?').count(),
                f.params.len(),
                "{input:?}: {}",
                f.sql
            );
        }
        // A row that still holds a saved-query reference is asserted through `expand` instead.
        Err(SearchError::Unsupported(_)) => assert!(
            want_params.is_empty(),
            "{input:?} binds nothing until it is expanded"
        ),
        Err(e) => panic!("{input:?} should compile: {e}"),
    }
}

/// The `SearchError` variant name, so a table can pin which rejection a row gets.
fn err_name(e: &SearchError) -> &'static str {
    match e {
        SearchError::TooLong { .. } => "TooLong",
        SearchError::TooDeep { .. } => "TooDeep",
        SearchError::UnbalancedParen { .. } => "UnbalancedParen",
        SearchError::UnexpectedEnd { .. } => "UnexpectedEnd",
        SearchError::UnexpectedToken { .. } => "UnexpectedToken",
        SearchError::BadRegex { .. } => "BadRegex",
        SearchError::BadDateInterval { .. } => "BadDateInterval",
        SearchError::BadOperand { .. } => "BadOperand",
        SearchError::QueryCycle { .. } => "QueryCycle",
        SearchError::QueryNotFound { .. } => "QueryNotFound",
        SearchError::BudgetExceeded { .. } => "BudgetExceeded",
        SearchError::Unsupported(_) => "Unsupported",
    }
}

/// The operator name of a predicate, spelled the way the query language spells it.
fn kind(pred: &Pred) -> &'static str {
    match pred {
        Pred::EntryIds { .. } => "e",
        Pred::FeedIds { .. } => "f",
        Pred::CategoryIds { .. } => "c",
        Pred::SavedQuery { .. } => "S",
        Pred::LabelIds { .. } => "l",
        Pred::LabelNames { .. } => "labels",
        Pred::Title { .. } => "intitle",
        Pred::Content { .. } => "intext",
        Pred::FreeText { .. } => "free",
        Pred::Author { .. } => "author",
        Pred::Url { .. } => "inurl",
        Pred::Tag { .. } => "#tag",
        Pred::Date { field, .. } => match field {
            DateField::Ingestion => "date",
            DateField::Publication => "pubdate",
            DateField::Modified => "mdate",
            DateField::UserModified => "userdate",
        },
    }
}

/// A readable form of the tree: `Leaf[intitle,!free]`, `Or[Leaf[a], Leaf[b]]`, `Not[Leaf[a,b]]`.
///
/// This is what makes the tables below diffable against the PHP ones — one column that says which
/// predicates the row produced, with which polarity, under which operator.
fn shape(expr: &Expr) -> String {
    match expr {
        Expr::Leaf(preds) => format!(
            "Leaf[{}]",
            preds
                .iter()
                .map(|p| if p.is_negated() {
                    format!("!{}", kind(p))
                } else {
                    kind(p).to_owned()
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        Expr::Node { op, children } => format!(
            "{}[{}]",
            match op {
                Op::And => "And",
                Op::Or => "Or",
            },
            children.iter().map(shape).collect::<Vec<_>>().join(", ")
        ),
        Expr::Not(inner) => format!("Not[{}]", shape(inner)),
    }
}

/// The distinct operator names a query uses — the port's answer to `FreshRSS_Search::
/// hasSameOperators()`, which compares the property names a `Search` object filled in.
fn operator_set(expr: &Expr) -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = expr.preds().iter().map(|p| kind(p)).collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// The single predicate of a one-term query.
fn only_pred(query: &str) -> Pred {
    match parse_ok(query) {
        Expr::Leaf(preds) => {
            assert_eq!(preds.len(), 1, "{query:?} should hold one predicate");
            preds.into_iter().next().expect("one predicate")
        }
        other => panic!("{query:?} should be one leaf, got {}", shape(&other)),
    }
}

/// The field, range and polarity of a date predicate.
fn date_of(query: &str) -> (DateField, DateRange, bool) {
    match only_pred(query) {
        Pred::Date {
            field,
            range,
            negated,
        } => (field, range, negated),
        other => panic!("{query:?} is not a date predicate: {other:?}"),
    }
}

/// A saved-search table, standing in for the `user_query` rows the PHP suite installs into
/// `FreshRSS_Context::userConf()->queries`.
///
/// Ids are the array indices PHP uses, so `S:0` is the first entry — the same spelling the PHP rows
/// use.
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
            if !name.is_empty() {
                saved
                    .by_name
                    .insert((*name).to_owned(), (*search).to_owned());
            }
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

// ===========================================================================
// SearchTest.php
// ===========================================================================

// --- provideEmptyInput -------------------------------------------------------
// test__construct_whenInputIsEmpty_getsOnlyNullValues

/// `[''], [' ']` — an empty search constrains nothing.
#[test]
fn an_empty_search_matches_everything() {
    for input in ["", " "] {
        let expr = parse_ok(input);
        assert!(expr.is_true(), "{input:?} should be the And identity");
        assert_eq!(render(&expr), "");
        let f = compile(&expr, &ctx()).unwrap();
        assert_eq!(f.sql, "1=1", "{input:?}");
        assert!(f.params.is_empty(), "{input:?}");
    }
}

// --- provideIntitleSearch ----------------------------------------------------
// test__construct_whenInputContainsIntitle_setsIntitleProperty
//
// PHP splits the input into `getIntitle()` and `getSearch()` (the free-text remainder); the port
// keeps both in one leaf, so the shape column shows the whole row at once.

static INTITLE_ROWS: &[Row] = &[
    (
        "intitle:word1",
        "Leaf[intitle]",
        "intitle:word1",
        &[t("word1")],
    ),
    (
        "intitle:word1-word2",
        "Leaf[intitle]",
        "intitle:word1-word2",
        &[t("word1-word2")],
    ),
    // PHP: intitle=['word1'], search=['word2'] — the space ends the operand.
    (
        "intitle:word1 word2",
        "Leaf[intitle,free]",
        "intitle:word1 word2",
        &[t("word1"), t("word2"), t("word2")],
    ),
    (
        r#"intitle:"word1 word2""#,
        "Leaf[intitle]",
        r#"intitle:"word1 word2""#,
        &[t("word1 word2")],
    ),
    // Single quotes are a quoting character too, and the canonical form uses double ones.
    (
        "intitle:'word1 word2'",
        "Leaf[intitle]",
        r#"intitle:"word1 word2""#,
        &[t("word1 word2")],
    ),
    (
        "word1 intitle:word2",
        "Leaf[free,intitle]",
        "word1 intitle:word2",
        &[t("word1"), t("word1"), t("word2")],
    ),
    (
        "word1 intitle:word2 word3",
        "Leaf[free,intitle,free]",
        "word1 intitle:word2 word3",
        &[t("word1"), t("word1"), t("word2"), t("word3"), t("word3")],
    ),
    (
        r#"word1 intitle:"word2 word3""#,
        "Leaf[free,intitle]",
        r#"word1 intitle:"word2 word3""#,
        &[t("word1"), t("word1"), t("word2 word3")],
    ),
    (
        "word1 intitle:'word2 word3'",
        "Leaf[free,intitle]",
        r#"word1 intitle:"word2 word3""#,
        &[t("word1"), t("word1"), t("word2 word3")],
    ),
    // Two intitle: in one query are two predicates, both of which must hold.
    (
        "intitle:word1 intitle:word2",
        "Leaf[intitle,intitle]",
        "intitle:word1 intitle:word2",
        &[t("word1"), t("word2")],
    ),
    // Divergence: an empty operand is `BadOperand` here; PHP drops the operator and keeps `word1
    // word2` as free text.
    ("intitle: word1 word2", "Err BadOperand", "", &[]),
    ("intitle:123", "Leaf[intitle]", "intitle:123", &[t("123")]),
    // Divergence: an unterminated quote is a lexing error here; PHP keeps the stray `"` as part of
    // the free text (`word3"`).
    (
        r#"intitle:"word1 word2" word3""#,
        "Err UnexpectedEnd",
        "",
        &[],
    ),
    ("intitle:'word1 word2' word3'", "Err UnexpectedEnd", "", &[]),
    (
        r#"intitle:"word1 word2' word3""#,
        "Leaf[intitle]",
        r#"intitle:"word1 word2' word3""#,
        &[t("word1 word2' word3")],
    ),
    (
        r#"intitle:'word1 word2" word3'"#,
        "Leaf[intitle]",
        r#"intitle:"word1 word2\" word3""#,
        &[t("word1 word2\" word3")],
    ),
    // Divergence: the AST holds plaintext, so the needle is `< & >` rather than PHP's HTML-encoded
    // `&lt; &amp; &gt;` — this port stores article titles as plaintext too, so the two agree.
    (
        r#"intitle:"< & >""#,
        "Leaf[intitle]",
        r#"intitle:"< & >""#,
        &[t("< & >")],
    ),
    (
        "intitle:word1 'word2 word3' word4",
        "Leaf[intitle,free,free]",
        r#"intitle:word1 "word2 word3" word4"#,
        &[
            t("word1"),
            t("word2 word3"),
            t("word2 word3"),
            t("word4"),
            t("word4"),
        ],
    ),
    (
        "intitle:word1+word2",
        "Leaf[intitle]",
        "intitle:word1+word2",
        &[t("word1+word2")],
    ),
];

#[test]
fn intitle_rows() {
    for row in INTITLE_ROWS {
        check_row(row);
    }
    // The operator really is the title column, and the needle is bound bare.
    assert_eq!(sql("intitle:word1"), "frss_ilike(e.title, ?)");
}

// --- provideIntextSearch -----------------------------------------------------
// test__construct_whenInputContainsIntext

static INTEXT_ROWS: &[Row] = &[
    (
        "intext:word1",
        "Leaf[intext]",
        "intext:word1",
        &[t("word1")],
    ),
    (
        r#"intext:"word1 word2""#,
        "Leaf[intext]",
        r#"intext:"word1 word2""#,
        &[t("word1 word2")],
    ),
];

#[test]
fn intext_rows() {
    for row in INTEXT_ROWS {
        check_row(row);
    }
    // `intext:` is the body only; conflating it with free text would match on the title.
    assert_eq!(sql("intext:word1"), "frss_ilike(e.content, ?)");
    assert_ne!(
        sql("intext:word1"),
        sql("word1"),
        "free text searches the title too"
    );
}

// --- provideAuthorSearch -----------------------------------------------------
// test__construct_whenInputContainsAuthor_setsAuthorValue

static AUTHOR_ROWS: &[Row] = &[
    (
        "author:word1",
        "Leaf[author]",
        "author:word1",
        &[t("word1")],
    ),
    (
        "author:word1-word2",
        "Leaf[author]",
        "author:word1-word2",
        &[t("word1-word2")],
    ),
    (
        "author:word1 word2",
        "Leaf[author,free]",
        "author:word1 word2",
        &[t("word1"), t("word2"), t("word2")],
    ),
    (
        r#"author:"word1 word2""#,
        "Leaf[author]",
        r#"author:"word1 word2""#,
        &[t("word1 word2")],
    ),
    (
        "author:'word1 word2'",
        "Leaf[author]",
        r#"author:"word1 word2""#,
        &[t("word1 word2")],
    ),
    (
        "word1 author:word2",
        "Leaf[free,author]",
        "word1 author:word2",
        &[t("word1"), t("word1"), t("word2")],
    ),
    (
        "word1 author:word2 word3",
        "Leaf[free,author,free]",
        "word1 author:word2 word3",
        &[t("word1"), t("word1"), t("word2"), t("word3"), t("word3")],
    ),
    (
        r#"word1 author:"word2 word3""#,
        "Leaf[free,author]",
        r#"word1 author:"word2 word3""#,
        &[t("word1"), t("word1"), t("word2 word3")],
    ),
    (
        "word1 author:'word2 word3'",
        "Leaf[free,author]",
        r#"word1 author:"word2 word3""#,
        &[t("word1"), t("word1"), t("word2 word3")],
    ),
    (
        "author:word1 author:word2",
        "Leaf[author,author]",
        "author:word1 author:word2",
        &[t("word1"), t("word2")],
    ),
    // Divergence: `BadOperand` here; PHP turns the empty operand into free text.
    ("author: word1 word2", "Err BadOperand", "", &[]),
    ("author:123", "Leaf[author]", "author:123", &[t("123")]),
    // Divergence: the unterminated quote is an error here, not free text.
    (
        r#"author:"word1 word2" word3""#,
        "Err UnexpectedEnd",
        "",
        &[],
    ),
    ("author:'word1 word2' word3'", "Err UnexpectedEnd", "", &[]),
    (
        r#"author:"word1 word2' word3""#,
        "Leaf[author]",
        r#"author:"word1 word2' word3""#,
        &[t("word1 word2' word3")],
    ),
    (
        r#"author:'word1 word2" word3'"#,
        "Leaf[author]",
        r#"author:"word1 word2\" word3""#,
        &[t("word1 word2\" word3")],
    ),
    (
        "author:word1 'word2 word3' word4",
        "Leaf[author,free,free]",
        r#"author:word1 "word2 word3" word4"#,
        &[
            t("word1"),
            t("word2 word3"),
            t("word2 word3"),
            t("word4"),
            t("word4"),
        ],
    ),
    (
        "author:word1+word2",
        "Leaf[author]",
        "author:word1+word2",
        &[t("word1+word2")],
    ),
];

#[test]
fn author_rows() {
    for row in AUTHOR_ROWS {
        check_row(row);
    }
    assert_eq!(sql("author:word1"), "frss_ilike(e.author, ?)");
}

// --- provideInurlSearch ------------------------------------------------------
// test__construct_whenInputContainsInurl_setsInurlValue

static INURL_ROWS: &[Row] = &[
    ("inurl:word1", "Leaf[inurl]", "inurl:word1", &[t("word1")]),
    // Divergence: `BadOperand` here; PHP turns the empty operand into free text.
    ("inurl: word1", "Err BadOperand", "", &[]),
    ("inurl:123", "Leaf[inurl]", "inurl:123", &[t("123")]),
    (
        "inurl:word1 word2",
        "Leaf[inurl,free]",
        "inurl:word1 word2",
        &[t("word1"), t("word2"), t("word2")],
    ),
    (
        r#"inurl:"word1 word2""#,
        "Leaf[inurl]",
        r#"inurl:"word1 word2""#,
        &[t("word1 word2")],
    ),
    (
        "inurl:word1 word2 inurl:word3",
        "Leaf[inurl,free,inurl]",
        "inurl:word1 word2 inurl:word3",
        &[t("word1"), t("word2"), t("word2"), t("word3")],
    ),
    (
        "inurl:word1 'word2 word3' word4",
        "Leaf[inurl,free,free]",
        r#"inurl:word1 "word2 word3" word4"#,
        &[
            t("word1"),
            t("word2 word3"),
            t("word2 word3"),
            t("word4"),
            t("word4"),
        ],
    ),
    (
        "inurl:word1+word2",
        "Leaf[inurl]",
        "inurl:word1+word2",
        &[t("word1+word2")],
    ),
];

#[test]
fn inurl_rows() {
    for row in INURL_ROWS {
        check_row(row);
    }
    assert_eq!(sql("inurl:word1"), "frss_ilike(e.link, ?)");
}

// --- provideDateSearch -------------------------------------------------------
// test__construct_whenInputContainsDate_setsDateValues
//
// PHP asserts `getMinDate()`/`getMaxDate()`; the port asserts the same two endpoints on
// `Pred::Date::range`. `strtotime('2008-05-12') - 1` is spelled `2008-05-11T23:59:59Z` here: a
// partial date is a period, so its ceiling is the last second of the day.

#[test]
fn date_rows() {
    let rows: &[(&str, Option<i64>, Option<i64>)] = &[
        (
            "date:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        // A duration after a date runs forward from it: max is `min + P1Y2M10DT2H30M - 1s`.
        (
            "date:2007-03-01T13:00:00Z/P1Y2M10DT2H30M",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:29:59Z")),
        ),
        // A duration before a date runs backward from it: min is `max - P1Y2M10DT2H30M + 1s`.
        (
            "date:P1Y2M10DT2H30M/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:01Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        (
            "date:2007-03-01/2008-05-11",
            Some(at("2007-03-01T00:00:00Z")),
            Some(at("2008-05-11T23:59:59Z")),
        ),
        ("date:2007-03-01/", Some(at("2007-03-01T00:00:00Z")), None),
        ("date:/2008-05-11", None, Some(at("2008-05-11T23:59:59Z"))),
    ];
    for (input, min, max) in rows {
        let (field, range, negated) = date_of(input);
        assert_eq!(field, DateField::Ingestion, "{input:?}");
        assert!(!negated, "{input:?}");
        assert_eq!(range.min, *min, "{input:?} min");
        assert_eq!(range.max, *max, "{input:?} max");
        // Divergence (kept from PHP, deliberately): `date:` is *ingestion* time, held in the entry
        // id, so both endpoints are bound in microseconds.
        let want: Vec<Param> = [min.map(|v| v * MICROS), max.map(|v| v * MICROS)]
            .into_iter()
            .flatten()
            .map(int)
            .collect();
        assert_eq!(params(input), want, "{input:?} bound values");
        assert!(sql(input).contains("e.id"), "{input:?}: {}", sql(input));
    }
}

// --- providePubdateSearch ----------------------------------------------------
// test__construct_whenInputContainsPubdate_setsPubdateValues

#[test]
fn pubdate_rows() {
    let rows: &[(&str, Option<i64>, Option<i64>)] = &[
        (
            "pubdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        (
            "pubdate:2007-03-01T13:00:00Z/P1Y2M10DT2H30M",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:29:59Z")),
        ),
        (
            "pubdate:P1Y2M10DT2H30M/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:01Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        (
            "pubdate:2007-03-01/2008-05-11",
            Some(at("2007-03-01T00:00:00Z")),
            Some(at("2008-05-11T23:59:59Z")),
        ),
        (
            "pubdate:2007-03-01/",
            Some(at("2007-03-01T00:00:00Z")),
            None,
        ),
        (
            "pubdate:/2008-05-11",
            None,
            Some(at("2008-05-11T23:59:59Z")),
        ),
    ];
    for (input, min, max) in rows {
        let (field, range, _) = date_of(input);
        assert_eq!(field, DateField::Publication, "{input:?}");
        assert_eq!((range.min, range.max), (*min, *max), "{input:?}");
        // `pubdate:` is the publication timestamp, in seconds — no scaling.
        let want: Vec<Param> = [*min, *max].into_iter().flatten().map(int).collect();
        assert_eq!(params(input), want, "{input:?} bound values");
        assert!(sql(input).contains("e.date"), "{input:?}: {}", sql(input));
    }
}

// --- provideModifiedDateSearch ----------------------------------------------
// test__construct_whenInputContainsModifiedDate

#[test]
fn modified_date_rows() {
    let rows: &[(&str, Option<i64>, Option<i64>)] = &[
        (
            "mdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        ("mdate:/2008-05-11", None, Some(at("2008-05-11T23:59:59Z"))),
    ];
    for (input, min, max) in rows {
        let (field, range, _) = date_of(input);
        assert_eq!(field, DateField::Modified, "{input:?}");
        assert_eq!((range.min, range.max), (*min, *max), "{input:?}");
        let want: Vec<Param> = [*min, *max].into_iter().flatten().map(int).collect();
        assert_eq!(params(input), want, "{input:?} bound values");
        // Divergence: PHP guards the upper bound with `COALESCE(e.lastModified, 0)`, so an article
        // that was never edited satisfies `mdate:/2008`; the port guards the whole comparison with
        // `IS NOT NULL`, so a missing timestamp fails in both polarities — the same answer the
        // in-memory matcher gives.
        assert!(
            sql(input).contains("e.lastModified IS NOT NULL"),
            "{input:?}: {}",
            sql(input)
        );
    }
}

// --- provideUserdateSearch ---------------------------------------------------
// test__construct_whenInputContainsUserdate

#[test]
fn userdate_rows() {
    let rows: &[(&str, Option<i64>, Option<i64>)] = &[
        (
            "userdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            Some(at("2007-03-01T13:00:00Z")),
            Some(at("2008-05-11T15:30:00Z")),
        ),
        (
            "userdate:/2008-05-11",
            None,
            Some(at("2008-05-11T23:59:59Z")),
        ),
    ];
    for (input, min, max) in rows {
        let (field, range, _) = date_of(input);
        assert_eq!(field, DateField::UserModified, "{input:?}");
        assert_eq!((range.min, range.max), (*min, *max), "{input:?}");
        let want: Vec<Param> = [*min, *max].into_iter().flatten().map(int).collect();
        assert_eq!(params(input), want, "{input:?} bound values");
        assert!(
            sql(input).contains("e.lastUserModified"),
            "{input:?}: {}",
            sql(input)
        );
    }
}

// --- provideTagsSearch -------------------------------------------------------
// test__construct_whenInputContainsTags_setsTagsValue

static TAG_ROWS: &[Row] = &[
    // Divergence: a tag is a whole-tag match, so the needle carries both markers.
    ("#word1", "Leaf[#tag]", "#word1", &[t("#word1 #")]),
    // Divergence: `#` with nothing after it is `BadOperand` here; PHP keeps `#` as free text.
    ("# word1", "Err BadOperand", "", &[]),
    ("#123", "Leaf[#tag]", "#123", &[t("#123 #")]),
    (
        "#word1 word2",
        "Leaf[#tag,free]",
        "#word1 word2",
        &[t("#word1 #"), t("word2"), t("word2")],
    ),
    (
        r#"#"word1 word2""#,
        "Leaf[#tag]",
        r#"#"word1 word2""#,
        &[t("#word1 word2 #")],
    ),
    (
        "#word1 #word2",
        "Leaf[#tag,#tag]",
        "#word1 #word2",
        &[t("#word1 #"), t("#word2 #")],
    ),
    (
        "#word1 'word2 word3' word4",
        "Leaf[#tag,free,free]",
        r#"#word1 "word2 word3" word4"#,
        &[
            t("#word1 #"),
            t("word2 word3"),
            t("word2 word3"),
            t("word4"),
            t("word4"),
        ],
    ),
    // Divergence: PHP runs `str_replace('+', ' ', …)` over tag values (a URL-decoding artefact, and
    // the only operator it does it for), so `#word1+word2` searches for a tag named "word1 word2".
    // The port binds what was typed.
    (
        "#word1+word2",
        "Leaf[#tag]",
        "#word1+word2",
        &[t("#word1+word2 #")],
    ),
];

#[test]
fn tag_rows() {
    for row in TAG_ROWS {
        check_row(row);
    }
    assert_eq!(
        sql("#word1"),
        "frss_ilike(TRIM(e.tags) || ' #', ?)",
        "both markers are load-bearing for a whole-tag match"
    );
}

// --- provideSavedQueriesExpansion -------------------------------------------
// test__construct_whenInputContainsSavedQueries_expandsSavedSearches
//
// Divergence: `S:`/`search:` are *not* expanded by the parser here — a reference stays a
// `Pred::SavedQuery` until `expand::expand` resolves it against a `QueryResolver`, so every row
// below goes through `expand` with the `Saved` table standing in for the PHP `queries` config.

/// The two saved searches most rows install.
fn two_saved() -> Saved {
    Saved::new(&[(0, "First", "author:Alice"), (1, "Second", "intitle:World")])
}

/// The four saved searches the `S:0,1 S:2,3,5` rows install.
fn four_saved() -> Saved {
    Saved::new(&[
        (0, "First", "author:Alice"),
        (1, "Second", "intitle:World"),
        (2, "Third", "inurl:Example"),
        (3, "Fourth", "author:Bob"),
    ])
}

#[test]
fn saved_query_rows() {
    // 'not found ID' — PHP binds nothing at all (`''`, `[]`) and keeps `S:3` in `toString()`.
    // Divergence: an unresolvable reference is an error here, not a silently dropped term.
    let saved = two_saved();
    let expr = parse_ok("S:3");
    assert_eq!(render(&expr), "S:3", "unexpanded, the reference survives");
    assert!(matches!(
        expand(&expr, &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "S:3"
    ));

    // 'not found name'
    let expr = parse_ok("search:Third");
    assert_eq!(render(&expr), "search:Third");
    assert!(matches!(
        expand(&expr, &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "search:Third"
    ));

    // 'expanded single group name' — PHP: `((e.author LIKE ?)) OR ((e.title LIKE ?))` with
    // `['%Alice%', '%World%']`.
    let expanded = expand(&parse_ok("search:First OR search:Second"), &saved, NOW).unwrap();
    assert_eq!(
        shape(&expanded),
        "Or[Leaf[author], Leaf[intitle]]",
        "{}",
        shape(&expanded)
    );
    let f = compile(&expanded, &ctx()).unwrap();
    // The needles are bound folded and lowercased, which is how both backends compare them
    // case- and accent-insensitively.
    assert_eq!(
        f.params,
        vec![text_param("alice"), text_param("world")],
        "{}",
        f.sql
    );

    // 'expanded single group name quotes' — PHP `search:"A First" OR search:'A Second'`.
    let saved = Saved::new(&[
        (0, "A First", "author:Alice"),
        (1, "A Second", "intitle:World"),
    ]);
    let input = r#"search:"A First" OR search:'A Second'"#;
    // PHP's `toString(expandUserQueries: false)` is `(search:"A First") OR (search:"A Second")`;
    // the canonical form here drops the redundant brackets.
    assert_eq!(
        render(&parse_ok(input)),
        r#"search:"A First" OR search:"A Second""#
    );
    let expanded = expand(&parse_ok(input), &saved, NOW).unwrap();
    assert_eq!(render(&expanded), "author:alice OR intitle:world");

    // 'expanded single group name quotes special characters' — a name holding `OR` and parentheses
    // must stay one name rather than being re-read as boolean syntax.
    let saved = Saved::new(&[
        (0, "A or B", "author:Alice"),
        (1, "(C OR D)", "intitle:World"),
    ]);
    let input = r#"search:"A or B" OR search:'(C OR D)'"#;
    assert_eq!(
        render(&parse_ok(input)),
        r#"search:"A or B" OR search:"(C OR D)""#
    );
    let expanded = expand(&parse_ok(input), &saved, NOW).unwrap();
    assert_eq!(render(&expanded), "author:alice OR intitle:world");

    // 'separate groups with AND' — PHP binds `['%Alice%', '%World%', '%Example%', '%Bob%']` and
    // silently drops the id 5 that does not exist.
    // Divergence: one unresolvable id fails the whole reference here rather than narrowing it
    // behind the user's back.
    let saved = four_saved();
    assert!(matches!(
        expand(&parse_ok("S:0,1 S:2,3,5"), &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "S:5"
    ));
    // With every id present, the row's point holds: two OR-groups, ANDed, values in order.
    let expanded = expand(&parse_ok("S:0,1 S:2,3"), &saved, NOW).unwrap();
    assert_eq!(
        shape(&expanded),
        "And[Or[Leaf[author], Leaf[intitle]], Or[Leaf[inurl], Leaf[author]]]"
    );
    assert_eq!(
        compile(&expanded, &ctx()).unwrap().params,
        vec![
            text_param("alice"),
            text_param("world"),
            text_param("example"),
            text_param("bob"),
        ]
    );

    // 'separate groups with OR' — same divergence on the missing id.
    assert!(matches!(
        expand(&parse_ok("S:0,1 OR S:2,3,5"), &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "S:5"
    ));
    // A disjunction of disjunctions flattens, which is the same set of articles.
    let expanded = expand(&parse_ok("S:0,1 OR S:2,3"), &saved, NOW).unwrap();
    assert_eq!(
        shape(&expanded),
        "Or[Leaf[author], Leaf[intitle], Leaf[inurl], Leaf[author]]"
    );
    assert_eq!(
        compile(&expanded, &ctx()).unwrap().params,
        vec![
            text_param("alice"),
            text_param("world"),
            text_param("example"),
            text_param("bob"),
        ]
    );

    // 'mixed with other clauses' — PHP:
    // `((e.id >= ? AND e.id <= ? AND e.title LIKE ?)) AND (((e.author LIKE ?)) OR ((e.title LIKE ?)))`
    // with `[strtotime('2025-10-01') . '000000', (strtotime('2025-11-01') - 1) . '000000',
    // '%Hello%', '%Alice%', '%World%']`.
    let saved = two_saved();
    let expanded = expand(&parse_ok("date:2025-10 intitle:Hello S:0,1"), &saved, NOW).unwrap();
    assert_eq!(
        shape(&expanded),
        "And[Leaf[date,intitle], Or[Leaf[author], Leaf[intitle]]]",
        "the expansion cannot re-associate with the terms around it"
    );
    assert_eq!(
        compile(&expanded, &ctx()).unwrap().params,
        vec![
            int(micros("2025-10-01T00:00:00Z")),
            int(micros("2025-10-31T23:59:59Z")),
            text_param("hello"),
            text_param("alice"),
            text_param("world"),
        ]
    );
}

// --- provideMultipleSearch ---------------------------------------------------
// test__construct_whenInputContainsMultipleKeywords_setsValues
//
// PHP asserts nine getters per row; the shape column below pins the same information — every
// operator landed in one conjunction, in the order it was written.

static MULTIPLE_SEARCH_ROWS: &[Row] = &[
    (
        "author:word1 date:2007-03-01/2008-05-11 intitle:word2 inurl:word3 \
         pubdate:2007-03-01/2008-05-11 #word4 #word5",
        "Leaf[author,date,intitle,inurl,pubdate,#tag,#tag]",
        "author:word1 date:20070301T000000/20080511T235959 intitle:word2 inurl:word3 \
         pubdate:20070301T000000/20080511T235959 #word4 #word5",
        &[],
    ),
    (
        "word6 intitle:word2 inurl:word3 pubdate:2007-03-01/2008-05-11 #word4 author:word1 \
         #word5 date:2007-03-01/2008-05-11",
        "Leaf[free,intitle,inurl,pubdate,#tag,author,#tag,date]",
        "word6 intitle:word2 inurl:word3 pubdate:20070301T000000/20080511T235959 #word4 \
         author:word1 #word5 date:20070301T000000/20080511T235959",
        &[],
    ),
    (
        "word6 intitle:word2 inurl:word3 pubdate:2007-03-01/2008-05-11 #word4 author:word1 \
         #word5 word7 date:2007-03-01/2008-05-11",
        "Leaf[free,intitle,inurl,pubdate,#tag,author,#tag,free,date]",
        "word6 intitle:word2 inurl:word3 pubdate:20070301T000000/20080511T235959 #word4 \
         author:word1 #word5 word7 date:20070301T000000/20080511T235959",
        &[],
    ),
    (
        "word6 intitle:word2 inurl:word3 pubdate:2007-03-01/2008-05-11 #word4 author:word1 \
         #word5 \"word7 word8\" date:2007-03-01/2008-05-11",
        "Leaf[free,intitle,inurl,pubdate,#tag,author,#tag,free,date]",
        "word6 intitle:word2 inurl:word3 pubdate:20070301T000000/20080511T235959 #word4 \
         author:word1 #word5 \"word7 word8\" date:20070301T000000/20080511T235959",
        &[],
    ),
];

#[test]
fn multiple_keyword_rows() {
    for row in MULTIPLE_SEARCH_ROWS {
        let (input, want_shape, want_render, _) = row;
        let expr = parse_ok(input);
        assert_eq!(shape(&expr), *want_shape, "{input:?}");
        assert_eq!(render(&expr), *want_render, "{input:?}");
        assert_eq!(
            parse(want_render, NOW).unwrap(),
            expr,
            "{input:?} does not round-trip"
        );
    }

    // The bound values PHP pins per getter, for the first row: both date fields scale differently.
    let f = fragment(
        "author:word1 date:2007-03-01/2008-05-11 intitle:word2 inurl:word3 \
         pubdate:2007-03-01/2008-05-11 #word4 #word5",
    );
    assert_eq!(f.params.len(), 9, "{:?}", f.params);
    for want in [
        text_param("word1"),
        int(micros("2007-03-01T00:00:00Z")),
        int(micros("2008-05-11T23:59:59Z")),
        text_param("word2"),
        text_param("word3"),
        int(at("2007-03-01T00:00:00Z")),
        int(at("2008-05-11T23:59:59Z")),
        text_param("#word4 #"),
        text_param("#word5 #"),
    ] {
        assert!(
            f.params.contains(&want),
            "{want:?} missing from {:?}",
            f.params
        );
    }
}

// --- provideAddOrParentheses -------------------------------------------------
// test__addOrParentheses
//
// PHP's `addOrParentheses()` brackets each `OR` segment by rewriting the string. The port has a
// parser, so what the row pins is the grouping the rewrite was for — asserted as the tree, and as
// the canonical text `render` produces from it.

static OR_PARENTHESES_ROWS: &[(&str, &str, &str)] = &[
    ("ab", "Leaf[free]", "ab"),
    ("ab cd", "Leaf[free,free]", "ab cd"),
    // `!` and `-` both negate; the canonical spelling is `-`.
    ("!ab -cd", "Leaf[!free,!free]", "-ab -cd"),
    ("ab OR cd", "Or[Leaf[free], Leaf[free]]", "ab OR cd"),
    ("!ab OR -cd", "Or[Leaf[!free], Leaf[!free]]", "-ab OR -cd"),
    (
        r#"ab cd OR ef OR "gh ij""#,
        "Or[Leaf[free,free], Leaf[free], Leaf[free]]",
        r#"(ab cd) OR ef OR "gh ij""#,
    ),
    // PHP: `ab (!cd)`. A negated *term* carries its flag inside the predicate, so the brackets are
    // redundant and the canonical form drops them.
    ("ab (!cd)", "Leaf[free,!free]", "ab -cd"),
    // PHP: `"ab" (!"cd")`. Quotes are dropped too, where the value does not need them.
    (r#""ab" (!"cd")"#, "Leaf[free,!free]", "ab -cd"),
];

#[test]
fn or_parentheses_rows() {
    for (input, want_shape, want_render) in OR_PARENTHESES_ROWS {
        let expr = parse_ok(input);
        assert_eq!(shape(&expr), *want_shape, "{input:?}");
        assert_eq!(render(&expr), *want_render, "{input:?}");
        assert_eq!(
            parse(want_render, NOW).unwrap(),
            expr,
            "{input:?} does not round-trip"
        );
    }
}

// --- provideconsistentOrParentheses ------------------------------------------
// test__consistentOrParentheses
//
// Same story for `consistentOrParentheses()`: the row's content is the grouping, and the port's
// canonical text is the equivalent of PHP's rewritten string (minus brackets that carry no weight).

static CONSISTENT_OR_ROWS: &[(&str, &str)] = &[
    ("ab cd ef", "ab cd ef"),
    ("(ab cd ef)", "ab cd ef"),
    (r#"("ab cd" ef)"#, r#""ab cd" ef"#),
    (r#""ab cd" (ef gh) "ij kl""#, r#""ab cd" ef gh "ij kl""#),
    ("ab (!cd)", "ab -cd"),
    // `!(cd)` negates a *group*, which is a different tree from the flagged predicate above, so the
    // brackets stay — PHP keeps them here too.
    ("ab !(cd)", "ab -(cd)"),
    ("(ab) -(cd)", "ab -(cd)"),
    (r#"ab cd OR ef OR "gh ij""#, r#"(ab cd) OR ef OR "gh ij""#),
    (r#""plain or text" OR (cd)"#, r#""plain or text" OR cd"#),
    ("(ab) OR cd OR ef OR (gh)", "ab OR cd OR ef OR gh"),
    (
        "(ab (cd OR ef)) OR gh OR ij OR (kl)",
        "(ab (cd OR ef)) OR gh OR ij OR kl",
    ),
    (
        "(ab (cd OR ef OR (gh))) OR ij",
        "(ab (cd OR ef OR gh)) OR ij",
    ),
    (
        "(ab (!cd OR ef OR (gh))) OR ij",
        "(ab (-cd OR ef OR gh)) OR ij",
    ),
    (
        "(ab !(cd OR ef OR !(gh))) OR ij",
        "(ab -(cd OR ef OR -(gh))) OR ij",
    ),
    (r#""ab" OR (!"cd")"#, "ab OR -cd"),
];

#[test]
fn consistent_or_parentheses_rows() {
    for (input, want) in CONSISTENT_OR_ROWS {
        let expr = parse_ok(input);
        assert_eq!(render(&expr), *want, "{input:?}");
        assert_eq!(parse(want, NOW).unwrap(), expr, "{input:?} round-trip");
        assert_eq!(
            render(&parse(want, NOW).unwrap()),
            *want,
            "{input:?} stable"
        );
    }
}

// --- provideParentheses ------------------------------------------------------
// test__parentheses
//
// The PHP provider pins PostgreSQL SQL text and its value list. Only the value list is portable, so
// each row asserts the tree and the bound values; the SQL shape is checked where the row is *about*
// the SQL (tags, labels, categories).

static PARENTHESES_ROWS: &[Row] = &[
    (
        "f:1 (f:2 OR f:3 OR f:4) (f:5 OR (f:6 OR f:7))",
        "And[Leaf[f], Or[Leaf[f], Leaf[f], Leaf[f]], Or[Leaf[f], Leaf[f], Leaf[f]]]",
        "f:1 (f:2 OR f:3 OR f:4) (f:5 OR f:6 OR f:7)",
        &[i(1), i(2), i(3), i(4), i(5), i(6), i(7)],
    ),
    (
        "c:1 OR c:2,3",
        "Or[Leaf[c], Leaf[c]]",
        "c:1 OR c:2,3",
        &[i(1), i(2), i(3)],
    ),
    (
        "#tag Hello OR (author:Alice inurl:example) OR (f:3 intitle:World) OR L:12",
        "Or[Leaf[#tag,free], Leaf[author,inurl], Leaf[f,intitle], Leaf[l]]",
        "(#tag hello) OR (author:alice inurl:example) OR (f:3 intitle:world) OR l:12",
        &[
            t("#tag #"),
            t("hello"),
            t("hello"),
            t("alice"),
            t("example"),
            i(3),
            t("world"),
            i(12),
        ],
    ),
    (
        "#tag Hello (author:Alice inurl:example) (f:3 intitle:World) label:Bleu",
        "Leaf[#tag,free,author,inurl,f,intitle,labels]",
        "#tag hello author:alice inurl:example f:3 intitle:world labels:bleu",
        &[
            t("#tag #"),
            t("hello"),
            t("hello"),
            t("alice"),
            t("example"),
            i(3),
            t("world"),
            t("bleu"),
        ],
    ),
    (
        "!((author:Alice intitle:hello) OR (author:Bob intitle:world))",
        "Not[Or[Leaf[author,intitle], Leaf[author,intitle]]]",
        "-((author:alice intitle:hello) OR (author:bob intitle:world))",
        &[t("alice"), t("hello"), t("bob"), t("world")],
    ),
    (
        "(author:Alice intitle:hello) !(author:Bob intitle:world)",
        "And[Leaf[author,intitle], Not[Leaf[author,intitle]]]",
        "author:alice intitle:hello -(author:bob intitle:world)",
        &[t("alice"), t("hello"), t("bob"), t("world")],
    ),
    // Divergence: the needle is bound literally, so the parentheses are data, not syntax — PHP
    // binds `%(test)%` and relies on `LIKE` having no use for them.
    (
        r#"intitle:"(test)""#,
        "Leaf[intitle]",
        r#"intitle:"(test)""#,
        &[t("(test)")],
    ),
    (
        r#"intitle:'"hello world"'"#,
        "Leaf[intitle]",
        r#"intitle:"\"hello world\"""#,
        &[t("\"hello world\"")],
    ),
    (
        r#"intext:'"hello world"'"#,
        "Leaf[intext]",
        r#"intext:"\"hello world\"""#,
        &[t("\"hello world\"")],
    ),
    (
        "(ab) OR (cd) OR (ef)",
        "Or[Leaf[free], Leaf[free], Leaf[free]]",
        "ab OR cd OR ef",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    (
        r#"("plain or text") OR (cd)"#,
        "Or[Leaf[free], Leaf[free]]",
        r#""plain or text" OR cd"#,
        &[t("plain or text"), t("plain or text"), t("cd"), t("cd")],
    ),
    (
        r#""plain or text" OR cd"#,
        "Or[Leaf[free], Leaf[free]]",
        r#""plain or text" OR cd"#,
        &[t("plain or text"), t("plain or text"), t("cd"), t("cd")],
    ),
    // A quoted `OR` is text, not an operator — and the needle is bound folded, hence lower-case.
    (
        r#""plain OR text" OR cd"#,
        "Or[Leaf[free], Leaf[free]]",
        r#""plain or text" OR cd"#,
        &[t("plain or text"), t("plain or text"), t("cd"), t("cd")],
    ),
    (
        "ab OR cd OR (ef)",
        "Or[Leaf[free], Leaf[free], Leaf[free]]",
        "ab OR cd OR ef",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    (
        "ab OR cd OR ef",
        "Or[Leaf[free], Leaf[free], Leaf[free]]",
        "ab OR cd OR ef",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    // Juxtaposition binds tighter than `OR`, so the first two terms are one branch.
    (
        "(ab) cd OR ef OR (gh)",
        "Or[Leaf[free,free], Leaf[free], Leaf[free]]",
        "(ab cd) OR ef OR gh",
        &[
            t("ab"),
            t("ab"),
            t("cd"),
            t("cd"),
            t("ef"),
            t("ef"),
            t("gh"),
            t("gh"),
        ],
    ),
    (
        "(ab) OR cd OR ef OR (gh)",
        "Or[Leaf[free], Leaf[free], Leaf[free], Leaf[free]]",
        "ab OR cd OR ef OR gh",
        &[
            t("ab"),
            t("ab"),
            t("cd"),
            t("cd"),
            t("ef"),
            t("ef"),
            t("gh"),
            t("gh"),
        ],
    ),
    (
        "ab OR (!(cd OR ef))",
        "Or[Leaf[free], Not[Or[Leaf[free], Leaf[free]]]]",
        "ab OR -(cd OR ef)",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    (
        "ab !(cd OR ef)",
        "And[Leaf[free], Not[Or[Leaf[free], Leaf[free]]]]",
        "ab -(cd OR ef)",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    (
        "ab OR !(cd OR ef)",
        "Or[Leaf[free], Not[Or[Leaf[free], Leaf[free]]]]",
        "ab OR -(cd OR ef)",
        &[t("ab"), t("ab"), t("cd"), t("cd"), t("ef"), t("ef")],
    ),
    (
        "(ab (!cd OR ef OR (gh))) OR !(ij OR kl)",
        "Or[And[Leaf[free], Or[Leaf[!free], Leaf[free], Leaf[free]]], \
         Not[Or[Leaf[free], Leaf[free]]]]",
        "(ab (-cd OR ef OR gh)) OR -(ij OR kl)",
        &[
            t("ab"),
            t("ab"),
            t("cd"),
            t("cd"),
            t("ef"),
            t("ef"),
            t("gh"),
            t("gh"),
            t("ij"),
            t("ij"),
            t("kl"),
            t("kl"),
        ],
    ),
    (
        r#""ab" "cd" ("ef") intitle:"gh" !"ij" -"kl""#,
        "Leaf[free,free,free,intitle,!free,!free]",
        "ab cd ef intitle:gh -ij -kl",
        &[
            t("ab"),
            t("ab"),
            t("cd"),
            t("cd"),
            t("ef"),
            t("ef"),
            t("gh"),
            t("ij"),
            t("ij"),
            t("kl"),
            t("kl"),
        ],
    ),
    // Divergence: plaintext needles, so `é & ' è` is bound accent-folded (`e & ' e`) rather than
    // HTML-encoded (`é &amp; ' è`) the way PHP binds it.
    (
        r#"intitle:"é & ' è" intext:/<&>/ '< & " >'"#,
        "Leaf[intitle,intext,free]",
        r#"intitle:"e & ' e" intext:/<&>/ "< & \" >""#,
        &[
            t("e & ' e"),
            // Divergence: a pattern is bound with its flags inlined, never as `/…/`.
            t("<&>"),
            t("< & \" >"),
            t("< & \" >"),
        ],
    ),
    (
        r"/^(ab|cd) [(] \) (ef|gh)/",
        "Leaf[free]",
        r"/^(ab|cd) [(] \) (ef|gh)/",
        &[t(r"^(ab|cd) [(] \) (ef|gh)"), t(r"^(ab|cd) [(] \) (ef|gh)")],
    ),
    (
        "!/^(ab|cd)/",
        "Leaf[!free]",
        "-/^(ab|cd)/",
        &[t("^(ab|cd)"), t("^(ab|cd)")],
    ),
    (
        "intitle:/^(ab|cd)/",
        "Leaf[intitle]",
        "intitle:/^(ab|cd)/",
        &[t("^(ab|cd)")],
    ),
    (
        "intext:/^(ab|cd)/",
        "Leaf[intext]",
        "intext:/^(ab|cd)/",
        &[t("^(ab|cd)")],
    ),
    ("L:1 L:2", "Leaf[l,l]", "l:1 l:2", &[i(1), i(2)]),
    ("L:1,2", "Leaf[l]", "l:1,2", &[i(1), i(2)]),
];

#[test]
fn parentheses_rows() {
    for row in PARENTHESES_ROWS {
        check_row(row);
    }

    // The rows whose PHP SQL is the point of the row.
    // A category is a subquery over the entry's feed, never a join that could change the row count.
    assert!(
        sql("c:1").starts_with("EXISTS (SELECT 1 FROM feed f2"),
        "{}",
        sql("c:1")
    );
    assert!(
        sql("-c:1").starts_with("NOT EXISTS (SELECT 1 FROM feed f2"),
        "{}",
        sql("-c:1")
    );
    // A label by id is an `entrytag` subquery; `L:1 L:2` is two of them, ANDed.
    assert_eq!(
        sql("L:1 L:2").matches("EXISTS").count(),
        2,
        "{}",
        sql("L:1 L:2")
    );
    assert_eq!(
        sql("L:1,2").matches("EXISTS").count(),
        1,
        "{}",
        sql("L:1,2")
    );
    // Divergence: a label *name* is a whole-name equality (`frss_ieq`), so `label:Bleu` cannot match
    // a label called "Bleu ciel" the way PHP's `t.name IN (?)` — a `LIKE` in other dialects — can.
    assert!(
        sql("label:Bleu").contains("frss_ieq(t.name, ?)"),
        "{}",
        sql("label:Bleu")
    );
    assert_eq!(params("label:Bleu"), vec![text_param("bleu")]);
    // A negated group stays a `NOT (…)` around the whole subtree.
    assert!(
        sql("!((author:Alice intitle:hello) OR (author:Bob intitle:world))").starts_with("NOT ("),
        "{}",
        sql("!((author:Alice intitle:hello) OR (author:Bob intitle:world))")
    );
    // Free text binds the same needle twice: once for the title, once for the body.
    let f = fragment("ab");
    assert_eq!(
        f.sql, "(frss_ilike(e.title, ?) OR frss_ilike(e.content, ?))",
        "{}",
        f.sql
    );
}

// --- test__add_single_search_combines_conditions_with_and --------------------
// --- test__add_multiple_searches_combines_conditions_with_or -----------------
//
// PHP builds two `FreshRSS_Search` objects programmatically and `add()`s them to an empty
// `BooleanSearch`. The port builds the same two predicates and combines them with `Expr::and` /
// `Expr::or`, which is what `prepend` and the filter-action code do.

#[test]
fn added_conditions_combine_with_and_then_or() {
    let start_time = at("2026-02-21T12:00:00Z");

    let min_date = Pred::Date {
        field: DateField::Ingestion,
        range: DateRange {
            min: Some(start_time),
            max: None,
        },
        negated: false,
    };
    let min_modified = Pred::Date {
        field: DateField::Modified,
        range: DateRange {
            min: Some(start_time),
            max: None,
        },
        negated: false,
    };

    // PHP: `(e.id >= ? AND e.`lastModified` >= ?)` with `[$startTime . '000000', $startTime]`.
    let both = Expr::and(vec![
        Expr::pred(min_date.clone()),
        Expr::pred(min_modified.clone()),
    ]);
    let f = compile(&both, &ctx()).unwrap();
    assert_eq!(
        f.params,
        vec![int(start_time * MICROS), int(start_time)],
        "date: is ingestion time, held in the microsecond id: {}",
        f.sql
    );
    assert_eq!(
        f.sql, "(e.id >= ?) AND ((e.lastModified IS NOT NULL AND e.lastModified >= ?))",
        "{}",
        f.sql
    );

    // PHP: `(e.id >= ?) OR (e.`lastModified` >= ?)` with the same two values.
    let either = Expr::or(vec![Expr::pred(min_date), Expr::pred(min_modified)]);
    let f = compile(&either, &ctx()).unwrap();
    assert_eq!(f.params, vec![int(start_time * MICROS), int(start_time)]);
    assert_eq!(
        f.sql, "(e.id >= ?) OR ((e.lastModified IS NOT NULL AND e.lastModified >= ?))",
        "{}",
        f.sql
    );
}

// --- provideDateOperators ----------------------------------------------------
// test__date_operators
//
// The PHP rows pin the comparison operator and the bound instants. Both are asserted here; the SQL
// column is this port's own spelling, because the operator choice (`BETWEEN`, `>=`, `< OR >`, the
// `IS NOT NULL` guard) is exactly what the row is about.

#[test]
fn date_operator_rows() {
    let rows: Vec<(&str, &str, Vec<Param>)> = vec![
        // Basic date operator tests
        (
            "date:2007-03-01/2008-05-11",
            "e.id BETWEEN ? AND ?",
            vec![
                int(micros("2007-03-01T00:00:00Z")),
                int(micros("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "date:2007-03-01/",
            "e.id >= ?",
            vec![int(micros("2007-03-01T00:00:00Z"))],
        ),
        (
            "date:/2008-05-11",
            "e.id <= ?",
            vec![int(micros("2008-05-11T23:59:59Z"))],
        ),
        // Basic pubdate operator tests
        (
            "pubdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "e.date BETWEEN ? AND ?",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        (
            "pubdate:2007-03-01/",
            "e.date >= ?",
            vec![int(at("2007-03-01T00:00:00Z"))],
        ),
        (
            "pubdate:/2008-05-11",
            "e.date <= ?",
            vec![int(at("2008-05-11T23:59:59Z"))],
        ),
        // Basic modified date operator tests
        // Divergence: PHP writes `COALESCE(e.lastModified, 0) <= ?` for the upper bound; the port
        // guards the whole comparison, so a NULL timestamp fails in both polarities.
        (
            "mdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "(e.lastModified IS NOT NULL AND e.lastModified BETWEEN ? AND ?)",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        (
            "mdate:2007-03-01/",
            "(e.lastModified IS NOT NULL AND e.lastModified >= ?)",
            vec![int(at("2007-03-01T00:00:00Z"))],
        ),
        (
            "mdate:/2008-05-11",
            "(e.lastModified IS NOT NULL AND e.lastModified <= ?)",
            vec![int(at("2008-05-11T23:59:59Z"))],
        ),
        // Basic userdate operator tests
        (
            "userdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "(e.lastUserModified IS NOT NULL AND e.lastUserModified BETWEEN ? AND ?)",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        (
            "userdate:2007-03-01/",
            "(e.lastUserModified IS NOT NULL AND e.lastUserModified >= ?)",
            vec![int(at("2007-03-01T00:00:00Z"))],
        ),
        (
            "userdate:/2008-05-11",
            "(e.lastUserModified IS NOT NULL AND e.lastUserModified <= ?)",
            vec![int(at("2008-05-11T23:59:59Z"))],
        ),
        // Negative date operator tests — the complement is emitted as two disjoint halves rather
        // than as `NOT (… BETWEEN …)`, and reuses the very bounds of the positive form.
        (
            "-date:2007-03-01/2008-05-11",
            "(e.id < ? OR e.id > ?)",
            vec![
                int(micros("2007-03-01T00:00:00Z")),
                int(micros("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "!pubdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "(e.date < ? OR e.date > ?)",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        (
            "!mdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "(e.lastModified IS NOT NULL AND (e.lastModified < ? OR e.lastModified > ?))",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        (
            "!userdate:2007-03-01T13:00:00Z/2008-05-11T15:30:00Z",
            "(e.lastUserModified IS NOT NULL AND (e.lastUserModified < ? OR e.lastUserModified > ?))",
            vec![
                int(at("2007-03-01T13:00:00Z")),
                int(at("2008-05-11T15:30:00Z")),
            ],
        ),
        // Combined date operators. PHP reorders these by its own field order; the port keeps the
        // order the query was written in, which is what makes the bound values line up with the
        // canonical text.
        (
            "date:2007-03-01/ pubdate:/2008-05-11",
            "(e.id >= ?) AND (e.date <= ?)",
            vec![
                int(micros("2007-03-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "pubdate:2007-03-01/ userdate:/2008-05-11",
            "(e.date >= ?) AND ((e.lastUserModified IS NOT NULL AND e.lastUserModified <= ?))",
            vec![
                int(at("2007-03-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "userdate:2007-03-01/ mdate:/2008-05-11",
            "((e.lastUserModified IS NOT NULL AND e.lastUserModified >= ?)) AND \
             ((e.lastModified IS NOT NULL AND e.lastModified <= ?))",
            vec![
                int(at("2007-03-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "date:2007-03-01/ userdate:2007-06-01/",
            "(e.id >= ?) AND ((e.lastUserModified IS NOT NULL AND e.lastUserModified >= ?))",
            vec![
                int(micros("2007-03-01T00:00:00Z")),
                int(at("2007-06-01T00:00:00Z")),
            ],
        ),
        // Complex combinations with other operators
        (
            "intitle:test date:2007-03-01/ pubdate:/2008-05-11",
            "(frss_ilike(e.title, ?)) AND (e.id >= ?) AND (e.date <= ?)",
            vec![
                text_param("test"),
                int(micros("2007-03-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
        (
            "author:john userdate:2007-03-01/2008-05-11",
            "(frss_ilike(e.author, ?)) AND \
             ((e.lastUserModified IS NOT NULL AND e.lastUserModified BETWEEN ? AND ?))",
            vec![
                text_param("john"),
                int(at("2007-03-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
        // Mixed positive and negative date operators
        (
            "date:2007-03-01/ !pubdate:2008-01-01/2008-05-11",
            "(e.id >= ?) AND ((e.date < ? OR e.date > ?))",
            vec![
                int(micros("2007-03-01T00:00:00Z")),
                int(at("2008-01-01T00:00:00Z")),
                int(at("2008-05-11T23:59:59Z")),
            ],
        ),
    ];

    for (input, want_sql, want_params) in rows {
        let f = fragment(input);
        assert_eq!(f.sql, want_sql, "{input:?}");
        assert_eq!(f.params, want_params, "{input:?}");
    }
}

// --- provideRegexPostreSQL ---------------------------------------------------
// test__regex_postgresql
//
// The provider's SQL column is PostgreSQL-specific (`~`, `~*`, `REPLACE(author, ';', '\n')`), so
// what is asserted is the portable part: which column, whether the value stayed a pattern, and what
// pattern is bound.

static REGEX_ROWS: &[Row] = &[
    (
        "intitle:/^ab$/",
        "Leaf[intitle]",
        "intitle:/^ab$/",
        &[t("^ab$")],
    ),
    // Divergence: the `i` flag is inlined into the bound pattern (`(?i)^ab$`) rather than choosing
    // a different operator (`~*`).
    (
        "intitle:/^ab$/i",
        "Leaf[intitle]",
        "intitle:/^ab$/i",
        &[t("(?i)^ab$")],
    ),
    (
        "intitle:/^ab$/m",
        "Leaf[intitle]",
        "intitle:/^ab$/m",
        &[t("(?m)^ab$")],
    ),
    // Divergence: `\b` is bound verbatim; PHP rewrites it to PostgreSQL's `\y`.
    (
        r"intitle:/\b\d+/",
        "Leaf[intitle]",
        r"intitle:/\b\d+/",
        &[t(r"\b\d+")],
    ),
    // Divergence: PHP anchors a regex per author by rewriting `;` to a newline first. The port
    // matches the whole `author` column, and its in-memory matcher does the same, so the two
    // backends still agree.
    (
        "author:/^ab$/",
        "Leaf[author]",
        "author:/^ab$/",
        &[t("^ab$")],
    ),
    ("inurl:/^ab$/", "Leaf[inurl]", "inurl:/^ab$/", &[t("^ab$")]),
    ("/^ab$/", "Leaf[free]", "/^ab$/", &[t("^ab$"), t("^ab$")]),
    ("!/^ab$/", "Leaf[!free]", "-/^ab$/", &[t("^ab$"), t("^ab$")]),
    // Divergence: `(?m)` is forced on because the tags column is rewritten one tag per line, so `^`
    // anchors a single tag. PHP relies on the same rewrite but spells the `i` flag as `~*`.
    (
        "#/^a(b|c)$/im",
        "Leaf[#tag]",
        "#/^a(b|c)$/im",
        &[t("(?i)(?m)^a(b|c)$")],
    ),
    // Not a regex: no closing delimiter before the whitespace, so the URL is a literal needle.
    (
        "inurl:https://example.net/test/",
        "Leaf[inurl]",
        r#"inurl:"https://example.net/test/""#,
        &[t("https://example.net/test/")],
    ),
    (
        "https://example.net/test/",
        "Leaf[free]",
        r#""https://example.net/test/""#,
        &[
            t("https://example.net/test/"),
            t("https://example.net/test/"),
        ],
    ),
    // Not a regex: quoted, so the slashes are data.
    (
        "author:'/u/Alice'",
        "Leaf[author]",
        r#"author:"/u/alice""#,
        &[t("/u/alice")],
    ),
    (
        "inurl:'/shorts/'",
        "Leaf[inurl]",
        r#"inurl:"/shorts/""#,
        &[t("/shorts/")],
    ),
    (
        r#"inurl:"/shorts/" OR inurl:"/spam/""#,
        "Or[Leaf[inurl], Leaf[inurl]]",
        r#"inurl:"/shorts/" OR inurl:"/spam/""#,
        &[t("/shorts/"), t("/spam/")],
    ),
    // Regex with literal 'or'
    (
        "intitle:/^A or B/i",
        "Leaf[intitle]",
        "intitle:/^A or B/i",
        &[t("(?i)^A or B")],
    ),
    // Regex with literal 'OR' — inside the delimiters it is not an operator, and the pattern's own
    // case is preserved because a regex is matched against the stored text, not its folded form.
    (
        "intitle:/^A B OR C D/i OR intitle:/^A B OR C D/i",
        "Or[Leaf[intitle], Leaf[intitle]]",
        "intitle:/^A B OR C D/i OR intitle:/^A B OR C D/i",
        &[t("(?i)^A B OR C D"), t("(?i)^A B OR C D")],
    ),
    // Quote with literal 'OR' — and bound folded, which is what makes it case-insensitive.
    (
        r#"intitle:"A B OR C D" OR intitle:"E or F""#,
        "Or[Leaf[intitle], Leaf[intitle]]",
        r#"intitle:"a b or c d" OR intitle:"e or f""#,
        &[t("a b or c d"), t("e or f")],
    ),
];

#[test]
fn regex_rows() {
    for row in REGEX_ROWS {
        check_row(row);
    }
    assert_eq!(sql("intitle:/^ab$/"), "frss_regexp(e.title, ?)");
    assert_eq!(
        sql("#/^a(b|c)$/im"),
        "frss_regexp(REPLACE(REPLACE(e.tags, ' #', '#'), '#', CHAR(10)), ?)",
        "one tag per line, so `^` and `$` anchor a single tag"
    );

    // Divergence: `\M` is PostgreSQL's "end of word", which no portable engine has. It is a bad
    // regular expression here rather than a pattern that silently matches nothing.
    for input in [r"intitle:/^ab\M/", r"intext:/^ab\M/"] {
        let err = parse(input, NOW).unwrap_err();
        assert!(
            matches!(err, SearchError::BadRegex { .. }),
            "{input}: {err}"
        );
    }
}

// --- provideRegexMariaDB / provideRegexMySQL / provideRegexSQLite ------------
// test__regex_mariadb / test__regex_mysql / test__regex_sqlite
//
// Omitted as written: all three providers exist only to pin a dialect's operator spelling
// (`REGEXP`, `REGEXP_LIKE(e.title,?,'mc')`, `e.title REGEXP ?` with the PHP-style `/^ab$/i` bound
// verbatim) and MariaDB's `UNCOMPRESS(e.content_bin)`. This port emits one `frss_regexp` UDF over
// the plain column for every dialect, so there is nothing dialect-specific left to assert. What the
// rows say beyond the spelling is covered here: the column, the negation, and the inlined flags.

#[test]
fn regex_dialect_rows_reduce_to_one_form() {
    // The union of the inputs of all three providers.
    let rows: &[(&str, &str, &[Want])] = &[
        ("intitle:/^ab$/", "frss_regexp(e.title, ?)", &[t("^ab$")]),
        (
            "intitle:/^ab$/i",
            "frss_regexp(e.title, ?)",
            &[t("(?i)^ab$")],
        ),
        (
            "intitle:/^ab$/m",
            "frss_regexp(e.title, ?)",
            &[t("(?m)^ab$")],
        ),
        (
            "intext:/^ab$/m",
            "frss_regexp(e.content, ?)",
            &[t("(?m)^ab$")],
        ),
        (
            "-intext:/^ab$/m",
            "NOT frss_regexp(e.content, ?)",
            &[t("(?m)^ab$")],
        ),
        (
            "-/^ab$/",
            "(NOT frss_regexp(e.title, ?) AND NOT frss_regexp(e.content, ?))",
            &[t("^ab$"), t("^ab$")],
        ),
        (
            r"intitle:/^ab\b/",
            "frss_regexp(e.title, ?)",
            &[t(r"^ab\b")],
        ),
        (
            r"intext:/^ab\b/",
            "frss_regexp(e.content, ?)",
            &[t(r"^ab\b")],
        ),
        // MariaDB spells the negation of a body pattern `NOT UNCOMPRESS(e.content_bin) REGEXP ?`;
        // the port reads the plain column and pushes the `NOT` onto the UDF call.
        (
            r"intitle:/\b\d+/",
            "frss_regexp(e.title, ?)",
            &[t(r"\b\d+")],
        ),
    ];
    for (input, want_sql, want_params) in rows {
        let f = fragment(input);
        assert_eq!(f.sql, *want_sql, "{input:?}");
        assert_eq!(f.params, wanted(want_params), "{input:?}");
    }
}

// --- provideToString ---------------------------------------------------------
// test__toString
//
// PHP asserts that a `FreshRSS_Search` built from this text renders back as the same text (with the
// newlines replaced by spaces), which works because the heredoc is written in `__toString()`'s own
// operator order. The port's equivalent guarantee is that every operator survives into the tree, in
// the order it was written, and that the canonical text parses back to the identical tree.

/// The one query that uses every operator, positive and negative. Verbatim from the PHP heredoc.
const EVERY_OPERATOR: &str = r#"e:1,2 f:10,11 c:20,21 L:30,31 labels:"My label,My other label"
userdate:2025-01-01T00:00:00/2026-01-01T00:00:00
mdate:2025-12
pubdate:2025-02-01T00:00:00/2026-01-01T00:00:00
date:2025-03-01T00:00:00/2026-01-01T00:00:00
intitle:/<Inter&sting>/i intitle:"g ' & d\\:"
intext:/<Inter&sting>/i intext:g&d
author:/Bob/ author:"/u/Alice" author:Alice
inurl:/https/ inurl:"/shorts/" inurl:example.net
#/tag2/ #tag1
/search_regex/i "quoted search" search search:"A user search" search:U1
-e:3,4 -f:12,13 -c:22,23 -L:32,33 -labels:"Not label,Not other label"
-userdate:2025-06-01T00:00:00/2025-09-01T00:00:00
-mdate:2025-12-27
-pubdate:2025
-date:P30D
-intitle:/Spam/i -intitle:"'bad"
-intext:/Spam/i -intext:"'bad"
-author:/Dave/i -author:"/u/Charlie" -author:Charlie
-inurl:/ftp/ -inurl:"/spam/" -inurl:example.com
-#/tag4/ -#tag3
-/not_regex/i -"not quoted" -not_search -search:"Negative user search" -search:U2"#;

#[test]
fn the_whole_operator_vocabulary_renders_canonically() {
    let expr = parse_ok(EVERY_OPERATOR);
    let preds = expr.preds();

    // Every operator, in the order the query wrote them, with its polarity. This is the row's
    // content: nothing was dropped, merged into free text, or inverted.
    let want_kinds = [
        "e",
        "f",
        "c",
        "l",
        "labels",
        "userdate",
        "mdate",
        "pubdate",
        "date",
        "intitle",
        "intitle",
        "intext",
        "intext",
        "author",
        "author",
        "author",
        "inurl",
        "inurl",
        "inurl",
        "#tag",
        "#tag",
        "free",
        "free",
        "free",
        "S",
        "S",
        "!e",
        "!f",
        "!c",
        "!l",
        "!labels",
        "!userdate",
        "!mdate",
        "!pubdate",
        "!date",
        "!intitle",
        "!intitle",
        "!intext",
        "!intext",
        "!author",
        "!author",
        "!author",
        "!inurl",
        "!inurl",
        "!inurl",
        "!#tag",
        "!#tag",
        "!free",
        "!free",
        "!free",
        "!S",
        "!S",
    ];
    let got: Vec<String> = preds
        .iter()
        .map(|p| {
            if p.is_negated() {
                format!("!{}", kind(p))
            } else {
                kind(p).to_owned()
            }
        })
        .collect();
    assert_eq!(got, want_kinds, "{got:?}");

    // It is one conjunction: no operator broke the implicit AND.
    assert!(matches!(expr, Expr::Leaf(_)), "{}", shape(&expr));

    let canonical = render(&expr);
    assert_eq!(parse(&canonical, NOW).unwrap(), expr, "round-trip");
    assert_eq!(
        render(&parse(&canonical, NOW).unwrap()),
        canonical,
        "stable"
    );

    // The interesting bits of the canonical text: values are folded, quoted only when they must be,
    // dates are absolute, and a saved-query reference survives unexpanded.
    for want in [
        "e:1,2",
        "l:30,31",
        r#"labels:"my label,my other label""#,
        "mdate:20251201T000000/20251231T235959",
        "intitle:/<Inter&sting>/i",
        r#"intitle:"g ' & d\\:""#,
        "intext:g&d",
        r#"author:"/u/alice""#,
        "#/tag2/",
        r#""quoted search""#,
        r#"search:"A user search""#,
        "search:U1",
        r#"-labels:"not label,not other label""#,
        "-pubdate:20250101T000000/20251231T235959",
        // `date:P30D` is relative to `NOW`, and renders as the absolute band it resolved to.
        "-date:20231015T221321/",
        r#"-intitle:"'bad""#,
        r#"-author:"/u/charlie""#,
        "-#/tag4/",
        r#"-search:"Negative user search""#,
        "-search:U2",
    ] {
        assert!(
            canonical.contains(want),
            "{want:?} missing from {canonical}"
        );
    }

    // Divergence: the two `search:` references are still `Pred::SavedQuery`, so neither backend can
    // evaluate the tree until `expand` runs.
    let unexpanded: Vec<&QueryRef> = preds
        .iter()
        .filter_map(|p| match p {
            Pred::SavedQuery { target, .. } => Some(target),
            _ => None,
        })
        .collect();
    assert_eq!(unexpanded.len(), 4, "{unexpanded:?}");
    assert!(matches!(
        compile(&expr, &ctx()).unwrap_err(),
        SearchError::Unsupported(_)
    ));
}

// --- provideBooleanSearchToString -------------------------------------------
// testBooleanSearchToString

static BOOLEAN_TO_STRING_ROWS: &[(&str, &str)] = &[
    (
        "((a OR b) (c OR d) -e) OR -(f g)",
        // PHP: `((a OR b) (c OR d) (-e)) OR -(f g)` — a negated term needs no brackets of its own.
        "((a OR b) (c OR d) -e) OR -(f g)",
    ),
    (
        "((a OR b) ((c) OR ((d))) (-e)) OR -(((f g)))",
        "((a OR b) (c OR d) -e) OR -(f g)",
    ),
    ("!((b c))", "-(b c)"),
    ("(a) OR !((b c))", "a OR -(b c)"),
    ("((a) (b))", "a b"),
    ("((a) OR (b))", "a OR b"),
    // Divergence: an even number of negators cancels out, so `!(!a)` is `a`; PHP keeps `-(-a)`.
    // The empty group `( )` is the And identity and contributes nothing.
    (" ( !( !( ( a ) ) ) ) ( ) ", "a"),
    ("-intitle:a -inurl:b", "-intitle:a -inurl:b"),
    ("intitle:/^A or B/i", "intitle:/^A or B/i"),
    ("intitle:/^A B OR C D/i", "intitle:/^A B OR C D/i"),
];

#[test]
fn boolean_search_to_string_rows() {
    for (input, want) in BOOLEAN_TO_STRING_ROWS {
        let expr = parse_ok(input);
        assert_eq!(render(&expr), *want, "{input:?}");
        assert_eq!(parse(want, NOW).unwrap(), expr, "{input:?} round-trip");
        assert_eq!(
            render(&parse(want, NOW).unwrap()),
            *want,
            "{input:?} stable"
        );
    }
}

// --- provideBooleanSearchToStringExpansion ----------------------------------
// testBooleanSearchToStringExpansion

#[test]
fn boolean_search_to_string_expansion_rows() {
    let saved = two_saved();

    // 'Not found ID' — PHP expands to `''`; here the unexpanded text survives and expanding errors.
    // Divergence: an unresolvable reference is reported, not silently emptied.
    let expr = parse_ok("S:3 S:4,5 ");
    assert_eq!(render(&expr), "S:3 S:4,5");
    assert!(matches!(
        expand(&expr, &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "S:3"
    ));

    // 'Not found name'
    let expr = parse_ok("search:Third ");
    assert_eq!(render(&expr), "search:Third");
    assert!(matches!(
        expand(&expr, &saved, NOW).unwrap_err(),
        SearchError::QueryNotFound { name } if name == "search:Third"
    ));

    // 'Found IDs' — PHP: `author:Alice OR intitle:World`.
    let expr = parse_ok("S:0,1 ");
    assert_eq!(render(&expr), "S:0,1");
    let expanded = expand(&expr, &saved, NOW).unwrap();
    assert_eq!(render(&expanded), "author:alice OR intitle:world");

    // 'Found names' — PHP: `author:Alice intitle:World`, an implicit AND of the two references.
    let expr = parse_ok("search:First search:Second ");
    assert_eq!(render(&expr), "search:First search:Second");
    let expanded = expand(&expr, &saved, NOW).unwrap();
    assert_eq!(render(&expanded), "author:alice intitle:world");
}

// --- provideHasSameOperators -------------------------------------------------
// testHasSameOperators

#[test]
fn has_same_operators_rows() {
    let rows = [
        ("", "", true),
        ("intitle:a intext:b", "intitle:c intext:d", true),
        ("intitle:a intext:b", "intitle:c inurl:d", false),
    ];
    for (input1, input2, want) in rows {
        let one = operator_set(&parse_ok(input1));
        let two = operator_set(&parse_ok(input2));
        assert_eq!(
            one == two,
            want,
            "{input1:?} vs {input2:?}: {one:?} {two:?}"
        );
    }
}

// --- provideBooleanSearchEnforce --------------------------------------------
// testBooleanSearchEnforce
//
// Divergence: PHP's `enforce()` works on a `FreshRSS_Search`'s property bag, so enforcing
// `intitle:b` *replaces* any existing `intitle:` condition. The port's `render::enforce` takes a
// single predicate and adds it unless an equal one is already there — it is the scope-chip
// operation ("also restrict to this"), which has to be idempotent rather than replacing, so
// clicking a chip twice cannot widen the query. Every row below therefore keeps the original term.

#[test]
fn enforce_rows() {
    let rows: &[(&str, &str, &str)] = &[
        ("", "", ""),
        ("", "intitle:b", "intitle:b"),
        ("intitle:a", "", "intitle:a"),
        ("intitle:a", "intitle:b", "intitle:a intitle:b"),
        ("a", "intitle:b", "a intitle:b"),
        (
            "intitle:a intext:a",
            "intitle:b",
            "intitle:a intext:a intitle:b",
        ),
        (
            "intitle:a inurl:a",
            "intitle:b",
            "intitle:a inurl:a intitle:b",
        ),
        (
            "intitle:a OR inurl:a",
            "intitle:b",
            "(intitle:a OR inurl:a) intitle:b",
        ),
        (
            "intitle:a ((inurl:a) (intitle:c))",
            "intitle:b",
            "intitle:a inurl:a intitle:c intitle:b",
        ),
        (
            "intitle:a ((inurl:a) OR (intitle:c))",
            "intitle:b",
            "intitle:a (inurl:a OR intitle:c) intitle:b",
        ),
        (
            "(intitle:a) (inurl:a)",
            "intitle:b",
            "intitle:a inurl:a intitle:b",
        ),
        (
            "(inurl:a) (intitle:a)",
            "intitle:b",
            "inurl:a intitle:a intitle:b",
        ),
        ("(a b) OR (c d)", "e", "((a b) OR (c d)) e"),
        ("(a b) (c d)", "e", "a b c d e"),
        ("(a b)", "e", "a b e"),
        // A date bound is added alongside the existing one, not substituted for it.
        (
            "date:2024/",
            "date:/2025",
            "date:20240101T000000/ date:/20251231T235959",
        ),
        ("a", "date:/2025", "a date:/20251231T235959"),
    ];
    for (initial, extra, want) in rows {
        let base = parse_ok(initial);
        let target = parse_ok(extra);
        let enforced = if target.is_true() {
            base.clone()
        } else {
            enforce(&base, only_pred(extra))
        };
        assert_eq!(render(&enforced), *want, "{initial:?} + {extra:?}");
        // Enforcing is idempotent: a UI that calls it on every render cannot accumulate terms.
        let twice = if target.is_true() {
            enforced.clone()
        } else {
            enforce(&enforced, only_pred(extra))
        };
        assert_eq!(twice, enforced, "{initial:?} + {extra:?} twice");
        // PHP also asserts `assertNotSame($booleanSearch, $newBooleanSearch)`. There is no Rust
        // counterpart to check: `enforce` takes `&Expr` and returns a new one, so the input cannot
        // have been mutated.
        assert_eq!(render(&base), render(&parse_ok(initial)), "input untouched");
    }

    // Enforcing a term that is already there changes nothing.
    let base = parse_ok("intitle:b c:3");
    assert_eq!(enforce(&base, only_pred("c:3")), base);
}

// --- provideBooleanSearchRemove ---------------------------------------------
// testBooleanSearchRemove
//
// Divergence: as with `enforce`, PHP's `remove()` clears a whole operator from the property bag, so
// removing `intitle:b` also drops an existing `intitle:a`. The port removes the predicate it was
// given, which is what dismissing one scope chip means.

#[test]
fn remove_rows() {
    let rows: &[(&str, &str, &str)] = &[
        ("", "intitle:b", ""),
        ("intitle:a", "intitle:b", "intitle:a"),
        ("intitle:a intext:a", "intitle:b", "intitle:a intext:a"),
        ("intitle:a inurl:a", "intitle:b", "intitle:a inurl:a"),
        ("intitle:a OR inurl:a", "intitle:b", "intitle:a OR inurl:a"),
        (
            "intitle:a ((inurl:a) (intitle:c))",
            "intitle:b",
            "intitle:a inurl:a intitle:c",
        ),
        (
            "intitle:a ((inurl:a) OR (intitle:c))",
            "intitle:b",
            "intitle:a (inurl:a OR intitle:c)",
        ),
        ("(intitle:a) (inurl:a)", "intitle:b", "intitle:a inurl:a"),
        ("(inurl:a) (intitle:a)", "intitle:b", "inurl:a intitle:a"),
        // Removing the term that *is* there: PHP's row `e ((a b) OR (c d))` minus `e`.
        ("e ((a b) OR (c d))", "e", "(a b) OR (c d)"),
        ("e ((a b) (c d))", "e", "a b c d"),
        ("date:2024/", "date:/2025", "date:20240101T000000/"),
        ("date:2024/ a", "date:/2025", "date:20240101T000000/ a"),
        // The same rows with a target that matches, which is what the port's `remove` acts on.
        ("intitle:a", "intitle:a", ""),
        ("intitle:a intext:a", "intitle:a", "intext:a"),
        ("intitle:a OR inurl:a", "inurl:a", "intitle:a"),
        ("date:2024/ a", "date:2024/", "a"),
    ];
    for (initial, target, want) in rows {
        let base = parse_ok(initial);
        let removed = remove(&base, &only_pred(target));
        assert_eq!(render(&removed), *want, "{initial:?} - {target:?}");
    }
}

// --- provideNeedVisibility ---------------------------------------------------
// testNeedVisibility

#[test]
fn need_visibility_rows() {
    let rows: &[(&str, Option<i8>)] = &[
        ("", None),
        ("f:1", Some(PRIORITY_HIDDEN)),
        ("c:2", Some(PRIORITY_CATEGORY)),
        ("f:1 c:2", Some(PRIORITY_HIDDEN)),
        ("-f:1", None),
        ("-c:2", None),
    ];
    for (input, want) in rows {
        assert_eq!(min_priority(&parse_ok(input)), *want, "{input:?}");
    }
}

// ===========================================================================
// BooleanSearchTest.php
// ===========================================================================

// --- providePrependMaxPubdate ------------------------------------------------
// test_prepend_restrictsTheSearchInsteadOfWideningIt
//
// `prepend()` is how the "mark as read → articles older than one day/week" action restricts an
// existing search. The point of every row is that the extra condition ANDs with the *whole* query,
// including a disjunction, rather than with one branch of it.

#[test]
fn prepend_restricts_the_search_instead_of_widening_it() {
    let max_pubdate = Expr::pred(Pred::Date {
        field: DateField::Publication,
        range: DateRange {
            min: None,
            max: Some(1_700_000_000),
        },
        negated: false,
    });

    // PHP: `(e.date <= ?)`, `[1700000000]`.
    let narrowed = prepend(&parse_ok(""), max_pubdate.clone());
    let f = compile(&narrowed, &ctx()).unwrap();
    assert_eq!(f.sql, "e.date <= ?", "{}", f.sql);
    assert_eq!(f.params, vec![int(1_700_000_000)]);

    // PHP: `(e.date <= ?) AND ((e.title LIKE ?))`, `[1700000000, '%sale%']`. The port keeps the
    // original query first, so the two values are bound in the other order.
    let narrowed = prepend(&parse_ok("intitle:sale"), max_pubdate.clone());
    assert_eq!(
        shape(&narrowed),
        "Leaf[intitle,pubdate]",
        "{}",
        shape(&narrowed)
    );
    let f = compile(&narrowed, &ctx()).unwrap();
    assert_eq!(
        f.params,
        vec![text_param("sale"), int(1_700_000_000)],
        "{}",
        f.sql
    );

    // PHP: `(e.date <= ?) AND ((e.title LIKE ?) OR (e.title LIKE ?))`, `[1700000000, '%a%', '%b%']`.
    let base = parse_ok("intitle:a OR intitle:b");
    let narrowed = prepend(&base, max_pubdate);
    assert_eq!(
        shape(&narrowed),
        "And[Or[Leaf[intitle], Leaf[intitle]], Leaf[pubdate]]",
        "the restriction covers both branches: {}",
        shape(&narrowed)
    );
    let f = compile(&narrowed, &ctx()).unwrap();
    assert_eq!(
        f.params,
        vec![text_param("a"), text_param("b"), int(1_700_000_000)],
        "{}",
        f.sql
    );
    assert_eq!(
        render(&narrowed),
        "(intitle:a OR intitle:b) pubdate:/20231114T221320",
        "and the canonical text keeps the disjunction bracketed"
    );
}

// --- test_constructor_acceptsSearchesAtTheLimits -----------------------------
// --- provideTooLongOrTooDeepSearches -----------------------------------------
// test_constructor_rejectsTooLongOrTooDeepSearches
//
// The PHP limits come from `config.default.php`: `max_search_length` 16384 and
// `max_search_parentheses_depth` 32 — the same two numbers this port hardcodes.

#[test]
fn searches_at_the_limits_are_accepted() {
    let input = "a".repeat(lexer::MAX_QUERY_LENGTH);
    let expr = parse_ok(&input);
    assert_eq!(render(&expr), input, "one free-text term, unchanged");
    assert_eq!(expr.preds().len(), 1);

    let input = format!(
        "{}ab{}",
        "(".repeat(parser::MAX_DEPTH),
        ")".repeat(parser::MAX_DEPTH)
    );
    assert_eq!(text(&input), "ab", "redundant brackets are dropped");
}

#[test]
fn too_long_or_too_deep_searches_are_rejected() {
    let too_long = "a".repeat(lexer::MAX_QUERY_LENGTH + 1);
    assert!(matches!(
        parse(&too_long, NOW).unwrap_err(),
        SearchError::TooLong { len, max } if len == lexer::MAX_QUERY_LENGTH + 1
            && max == lexer::MAX_QUERY_LENGTH
    ));

    let too_deep = format!(
        "{}ab{}",
        "(".repeat(parser::MAX_DEPTH + 1),
        ")".repeat(parser::MAX_DEPTH + 1)
    );
    assert!(matches!(
        parse(&too_deep, NOW).unwrap_err(),
        SearchError::TooDeep { depth, max } if depth == parser::MAX_DEPTH + 1
            && max == parser::MAX_DEPTH
    ));
}

// ===========================================================================
// Rows that have no PHP counterpart but pin the divergences the port documents
// ===========================================================================

#[test]
fn the_port_specific_operator_spellings() {
    // Divergence: `searc:` (PHP's `\bsearch?:` regex makes the `h` optional) is not an operator
    // here, so the whole word is free text — a typo cannot become a saved-search lookup.
    assert_eq!(shape(&parse_ok("searc:foo")), "Leaf[free]");
    assert_eq!(
        params("searc:foo"),
        vec![text_param("searc:foo"), text_param("searc:foo")]
    );
    // …while the real spelling stays a reference.
    assert!(matches!(
        only_pred("search:foo"),
        Pred::SavedQuery {
            target: QueryRef::Names(_),
            negated: false
        }
    ));

    // Divergence: needles are bound literally, so `_` and `%` are characters rather than `LIKE`
    // wildcards.
    assert_eq!(params("intitle:a_b"), vec![text_param("a_b")]);
    assert_eq!(params("intitle:100%"), vec![text_param("100%")]);

    // The id-list and wildcard operators the PHP suite exercises only inside bigger queries.
    assert_eq!(params("e:1,2"), vec![int(1), int(2)]);
    assert_eq!(sql("-e:3,4"), "e.id NOT IN (?,?)");
    assert!(matches!(
        only_pred("l:*"),
        Pred::LabelIds {
            ids: IdSet::Any,
            negated: false
        }
    ));
    assert!(params("l:*").is_empty(), "a wildcard binds no ids");
    assert!(sql("-l:*").starts_with("NOT EXISTS"), "{}", sql("-l:*"));
    // Divergence: a leading or doubled `OR`, and a trailing negation, are errors here; PHP folds
    // them away silently.
    for (input, variant) in [
        ("OR a", "UnexpectedToken"),
        ("a OR OR b", "UnexpectedToken"),
        ("a OR", "UnexpectedEnd"),
        ("a -", "UnexpectedEnd"),
    ] {
        let err = parse(input, NOW).unwrap_err();
        assert_eq!(err_name(&err), variant, "{input}: {err}");
    }

    // A substring needle is bound accent-folded and lowercased, which is how one comparison serves
    // both backends; a regex keeps its own case because it is matched against the stored text.
    assert_eq!(
        params(r#"intitle:"Café ÉCOLE""#),
        vec![text_param("cafe ecole")]
    );
    assert_eq!(params("intitle:/Café/"), vec![text_param("Café")]);
    assert!(matches!(
        only_pred("intitle:/Café/"),
        Pred::Title {
            pat: TextPat::Regex(_),
            ..
        }
    ));
}
