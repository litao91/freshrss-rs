//! Compiling an [`Expr`] to a SQL `WHERE` fragment plus bound parameters.
//!
//! Two properties this module exists to guarantee:
//!
//! - **Text comparison goes through `instr()` on the stored fold, never `LIKE`.** SQLite's `LIKE` is
//!   case-insensitive for ASCII only, so `intitle:ecole` does not match "École", and it would make a
//!   user's `_` or `%` a wildcard. The searchable columns carry an accent-folded copy written at
//!   ingest (see the server crate's `db::fold`), the needle is folded at parse time, and `instr` is a
//!   literal substring test — so SQL and the in-memory matcher agree by construction, which is what
//!   makes the equivalence test meaningful.
//! - **Regular expressions are refused here.** SQLite has no regex operator without a user-defined
//!   function and the driver cannot register one, so a regex predicate is
//!   [`SearchError::Unsupported`] in a listing. The matcher still evaluates regexes, so filter actions
//!   keep them; the two backends differ only in that a listing says "no" loudly where a filter action
//!   answers.
//! - **Values are always bound, never interpolated.** A search string is untrusted user input that
//!   ends up in a `WHERE` clause; the only difference between this being safe and being an injection
//!   hole is that every literal here is a `?`.

use crate::ast::{DateField, DateRange, Expr, IdSet, Op, Pred, QueryRef, TextPat};
use crate::error::SearchError;

/// What the compiler needs to know about the query it is being spliced into.
#[derive(Debug, Clone)]
pub struct SqlCtx<'a> {
    /// The table alias for `entry`, so the fragment can be joined into a larger query. The listing
    /// builder uses `e`; a subquery over a different alias must not collide with it.
    pub alias: &'a str,
    /// The alias for `entrytag` subqueries, and the entry-id column within them.
    pub entrytag_alias: &'a str,
    /// Whether the surrounding query already restricts rows to one user. The label subquery needs to
    /// know, because `entrytag` has no `id_user` of its own and must join through `tag`.
    pub scoped_to_user: bool,
}

impl Default for SqlCtx<'_> {
    fn default() -> Self {
        Self {
            alias: "e",
            entrytag_alias: "et",
            scoped_to_user: true,
        }
    }
}

/// A compiled fragment: SQL text with `?` placeholders, and the values to bind in order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fragment {
    pub sql: String,
    pub params: Vec<Param>,
}

/// One bound value.
///
/// Deliberately this crate's own type rather than a driver's: `freshrss-search` has no database
/// dependency at all, which is what lets the compiled SQL be asserted on as text and the whole
/// grammar be tested without a connection. The server converts a `Param` into a sqlx bind value in
/// `db::sql`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Param {
    Text(String),
    Int(i64),
    Null,
}

impl Fragment {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sql.is_empty()
    }

    /// Joins sub-fragments with an operator, parenthesizing each so precedence survives.
    fn join(op: &str, parts: Vec<Fragment>) -> Fragment {
        let mut sql = String::new();
        let mut params = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                sql.push_str(op);
            }
            sql.push('(');
            sql.push_str(&part.sql);
            sql.push(')');
            params.extend(part.params);
        }
        Fragment { sql, params }
    }
}

/// Compiles a whole expression.
///
/// # Errors
///
/// [`SearchError::Unsupported`] when the tree still holds an unexpanded [`Pred::SavedQuery`]. Run
/// [`crate::expand`] first: resolving a saved search needs the database, and this crate has none.
pub fn compile(expr: &Expr, ctx: &SqlCtx<'_>) -> Result<Fragment, SearchError> {
    match expr {
        Expr::Leaf(preds) => compile_leaf(preds, ctx),
        Expr::Node { op, children } => {
            let mut parts = Vec::with_capacity(children.len());
            for child in children {
                parts.push(compile(child, ctx)?);
            }
            // `Op::And` of nothing is TRUE and `Op::Or` of nothing is FALSE; the smart constructors
            // in `ast` already collapsed those, but a hand-built tree may not have gone through them.
            if parts.is_empty() {
                return Ok(Fragment::new(match op {
                    Op::And => "1=1",
                    Op::Or => "1=0",
                }));
            }
            Ok(Fragment::join(
                match op {
                    Op::And => " AND ",
                    Op::Or => " OR ",
                },
                parts,
            ))
        }
        Expr::Not(inner) => {
            let part = compile(inner, ctx)?;
            Ok(Fragment {
                sql: format!("NOT ({})", part.sql),
                params: part.params,
            })
        }
    }
}

/// Compiles a leaf: every predicate in it must hold.
fn compile_leaf(preds: &[Pred], ctx: &SqlCtx<'_>) -> Result<Fragment, SearchError> {
    if preds.is_empty() {
        return Ok(Fragment::new("1=1"));
    }
    let mut parts = Vec::with_capacity(preds.len());
    for pred in preds {
        parts.push(compile_pred(pred, ctx)?);
    }
    if parts.len() == 1 {
        return Ok(parts.pop().expect("len 1"));
    }
    Ok(Fragment::join(" AND ", parts))
}

fn compile_pred(pred: &Pred, ctx: &SqlCtx<'_>) -> Result<Fragment, SearchError> {
    let a = ctx.alias;
    match pred {
        Pred::EntryIds { ids, negated } => Ok(id_condition(&format!("{a}.id"), ids, *negated)),
        Pred::FeedIds { ids, negated } => Ok(id_condition(&format!("{a}.id_feed"), ids, *negated)),
        Pred::CategoryIds { ids, negated } => Ok(category_condition(a, ids, *negated)),
        Pred::Title { pat, negated } => text_condition(&format!("{a}.title"), pat, *negated),
        Pred::Content { pat, negated } => text_condition(&format!("{a}.content"), pat, *negated),
        Pred::FreeText { pat, negated } => free_text_condition(
            &format!("{a}.title"),
            &format!("{a}.content"),
            pat,
            *negated,
        ),
        Pred::Author { pat, negated } => text_condition(&format!("{a}.author"), pat, *negated),
        Pred::Url { pat, negated } => text_condition(&format!("{a}.link"), pat, *negated),
        Pred::Tag { pat, negated } => tag_condition(&format!("{a}.tags"), pat, *negated),
        Pred::LabelIds { ids, negated } => label_condition(ctx, ids, *negated),
        Pred::LabelNames { names, negated } => label_name_condition(ctx, names, *negated),
        Pred::Date {
            field,
            range,
            negated,
        } => Ok(date_condition(a, *field, *range, *negated)),
        Pred::SavedQuery { target, .. } => Err(SearchError::Unsupported(match target {
            QueryRef::Ids(_) => "S:".to_owned(),
            QueryRef::Names(_) => "search:".to_owned(),
        })),
    }
}

/// `col IN (…)` / `col NOT IN (…)`, with the empty list collapsed.
///
/// An empty list must not become `IN ()`, which is a syntax error in SQLite. It becomes `1=0`
/// (matches nothing) or `1=1` when negated, so `f:` with no ids behaves as "no feeds" rather than
/// blowing up the query.
fn id_condition(col: &str, ids: &[i64], negated: bool) -> Fragment {
    if ids.is_empty() {
        return Fragment::new(if negated { "1=1" } else { "1=0" });
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let op = if negated { "NOT IN" } else { "IN" };
    Fragment {
        sql: format!("{col} {op} ({placeholders})"),
        params: ids.iter().map(|id| Param::Int(*id)).collect(),
    }
}

/// `c:` matches the entry's feed's category, so it needs a join or a subquery.
///
/// A subquery rather than a join: the listing query already joins `feed`, and adding a second join
/// path to `category` would change the row count if a feed were ever in two categories.
fn category_condition(alias: &str, ids: &[i64], negated: bool) -> Fragment {
    if ids.is_empty() {
        return Fragment::new(if negated { "1=1" } else { "1=0" });
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let op = if negated { "NOT EXISTS" } else { "EXISTS" };
    Fragment {
        sql: format!(
            "{op} (SELECT 1 FROM feed f2 WHERE f2.id = {alias}.id_feed AND f2.category IN ({placeholders}))"
        ),
        params: ids.iter().map(|id| Param::Int(*id)).collect(),
    }
}

/// One text predicate: `frss_ilike` for a substring, `frss_regexp` for a pattern.
///
/// Both UDFs are registered with `SQLITE_DETERMINISTIC` and take `(text, pattern)` in that order,
/// which is the reverse of SQLite's infix operators — worth remembering when reading the SQL.
fn text_condition(col: &str, pat: &TextPat, negated: bool) -> Result<Fragment, SearchError> {
    // The comparison is against the stored fold of the column, so the needle — folded at parse time —
    // meets it on equal terms. `instr` rather than `LIKE` because LIKE would make a user's `_` or `%`
    // a wildcard; `instr` is a literal substring test and nothing else.
    let cmp = if negated { "= 0" } else { "> 0" };
    match pat {
        TextPat::Contains(needle) => Ok(Fragment {
            sql: format!("instr({col}_folded, ?) {cmp}"),
            params: vec![Param::Text(needle.clone())],
        }),
        // SQLite has no regex operator without a user-defined function, and the driver cannot
        // register one. The matcher still evaluates regexes, so filter actions keep them; a listing
        // says so plainly rather than quietly matching nothing.
        TextPat::Regex(re) => Err(SearchError::Unsupported(format!(
            "the regular expression /{}/ in a listing query",
            re.source
        ))),
    }
}

/// Free text, which searches the title *or* the body.
///
/// Negation has to be pushed inside: `NOT (title LIKE x OR content LIKE x)` is
/// `title NOT LIKE x AND content NOT LIKE x`. Emitting the `NOT` outside would be correct too, but
/// this form is what FreshRSS produces and it keeps the two branches independently indexable.
fn free_text_condition(
    title: &str,
    content: &str,
    pat: &TextPat,
    negated: bool,
) -> Result<Fragment, SearchError> {
    let a = text_condition(title, pat, negated)?;
    let b = text_condition(content, pat, negated)?;
    let op = if negated { " AND " } else { " OR " };
    Ok(Fragment {
        sql: format!("({a}{op}{b})", a = a.sql, b = b.sql),
        params: [a.params, b.params].concat(),
    })
}

/// A `#tag` predicate.
///
/// Tags are stored as `"#a #b"`, so a whole-tag match appends `' #'` to the haystack and looks for
/// `"#needle #"`. Without the trailing marker `#ru` would match the tag `rust`; without the leading
/// one `#st` would match it too. The in-memory matcher gets the same rule by splitting the column and
/// comparing whole tags.
fn tag_condition(col: &str, pat: &TextPat, negated: bool) -> Result<Fragment, SearchError> {
    let cmp = if negated { "= 0" } else { "> 0" };
    match pat {
        // Both markers are load-bearing: the trailing " #" stops `#ru` matching the tag `rust`, and
        // the leading one stops `#st` matching it too. The matcher gets the same rule by splitting the
        // column and comparing whole tags.
        TextPat::Contains(needle) => Ok(Fragment {
            sql: format!("instr(TRIM({col}_folded) || ' #', ?) {cmp}"),
            params: vec![Param::Text(format!("#{needle} #"))],
        }),
        TextPat::Regex(re) => Err(SearchError::Unsupported(format!(
            "the regular expression /{}/ in a listing query",
            re.source
        ))),
    }
}

/// A label predicate by id: `l:3`, `l:1,2`, or `l:*`.
fn label_condition(ctx: &SqlCtx<'_>, ids: &IdSet, negated: bool) -> Result<Fragment, SearchError> {
    let a = ctx.alias;
    let et = ctx.entrytag_alias;
    let op = if negated { "NOT EXISTS" } else { "EXISTS" };
    let inner = match ids {
        IdSet::Any => format!(
            "{op} (SELECT 1 FROM entrytag {et} JOIN tag t ON t.id = {et}.id_tag \
             WHERE {et}.id_entry = {a}.id{user})",
            user = if ctx.scoped_to_user {
                format!(" AND t.id_user = {a}.id_user")
            } else {
                String::new()
            }
        ),
        IdSet::List(list) if list.is_empty() => {
            return Ok(Fragment::new(if negated { "1=1" } else { "1=0" }));
        }
        IdSet::List(list) => {
            let placeholders = vec!["?"; list.len()].join(",");
            format!(
                "{op} (SELECT 1 FROM entrytag {et} WHERE {et}.id_entry = {a}.id \
                 AND {et}.id_tag IN ({placeholders}))"
            )
        }
    };
    let params = match ids {
        IdSet::List(list) => list.iter().map(|id| Param::Int(*id)).collect(),
        IdSet::Any => Vec::new(),
    };
    Ok(Fragment { sql: inner, params })
}

/// A label predicate by name: `labels:todo` or `labels:a,b`.
///
/// Several names are ORed together, matching how every other list-valued operator behaves. A plain
/// name is matched **whole**, not as a substring: `labels:todo` must not match a label called
/// "todos". That is `frss_ieq` rather than `frss_ilike` — one accent-folded comparison per name, and
/// the same rule the matcher applies when it compares against each of the target's label names.
fn label_name_condition(
    ctx: &SqlCtx<'_>,
    names: &[TextPat],
    negated: bool,
) -> Result<Fragment, SearchError> {
    if names.is_empty() {
        return Ok(Fragment::new(if negated { "1=1" } else { "1=0" }));
    }
    let a = ctx.alias;
    let et = ctx.entrytag_alias;
    let op = if negated { "NOT EXISTS" } else { "EXISTS" };
    let scope = if ctx.scoped_to_user {
        format!("t.id_user = {a}.id_user AND ")
    } else {
        String::new()
    };

    let mut clauses = Vec::with_capacity(names.len());
    let mut params = Vec::with_capacity(names.len());
    for name in names {
        match name {
            // Whole-name equality on the stored fold: `labels:todo` must not match a label "todos".
            TextPat::Contains(needle) => {
                clauses.push("t.name_folded = ?".to_owned());
                params.push(Param::Text(needle.clone()));
            }
            TextPat::Regex(re) => {
                return Err(SearchError::Unsupported(format!(
                    "the regular expression /{}/ in a listing query",
                    re.source
                )))
            }
        }
    }

    Ok(Fragment {
        sql: format!(
            "{op} (SELECT 1 FROM entrytag {et} JOIN tag t ON t.id = {et}.id_tag \
             WHERE {et}.id_entry = {a}.id AND {scope}({}))",
            clauses.join(" OR ")
        ),
        params,
    })
}

/// A date range against one of the four timestamp columns.
///
/// [`DateField::Ingestion`] compares against the entry **id**, which is a microsecond wall clock —
/// so the bounds are multiplied by a million. That is FreshRSS's long-standing behaviour (`date:`
/// has always meant "when it was added") and it is kept for fidelity; `pubdate:` is the publication
/// time. The reader's help dialog says so, because it is otherwise genuinely surprising.
fn date_condition(alias: &str, field: DateField, range: DateRange, negated: bool) -> Fragment {
    let (col, scale, nullable): (String, i64, bool) = match field {
        DateField::Ingestion => (format!("{alias}.id"), 1_000_000, false),
        DateField::Publication => (format!("{alias}.date"), 1, false),
        // The two edit timestamps are NULL on an article that was never edited.
        DateField::Modified => (format!("{alias}.lastModified"), 1, true),
        DateField::UserModified => (format!("{alias}.lastUserModified"), 1, true),
    };

    // An unbounded range matches every row that has a value at all, so its complement matches none.
    // This has to be settled before `negate()`: `DateRange` has no way to say "empty" (`min > max`
    // already means "outside the band"), so complementing an unbounded range would return it
    // unchanged and quietly invert the predicate.
    if range.is_unbounded() {
        return Fragment::new(if negated {
            "1=0".to_owned()
        } else {
            format!("{col} IS NOT NULL")
        });
    }

    // A negated range is complemented here rather than wrapped in `NOT (…)`, which keeps the
    // predicate index-usable: `NOT (id BETWEEN a AND b)` becomes the OR of the two disjoint halves.
    // The complement of a two-sided range has `min > max`, which the match below reads as exactly that.
    let range = if negated { range.negate() } else { range };

    let (min, max) = (
        range.min.map(|v| v.saturating_mul(scale)),
        range.max.map(|v| v.saturating_mul(scale)),
    );

    let mut f = match (min, max) {
        (Some(lo), Some(hi)) if lo > hi => Fragment {
            // The complement of [hi .. lo]: everything strictly below or above the original band.
            sql: format!("({col} < ? OR {col} > ?)"),
            params: vec![Param::Int(hi), Param::Int(lo)],
        },
        (Some(lo), Some(hi)) => Fragment {
            sql: format!("{col} BETWEEN ? AND ?"),
            params: vec![Param::Int(lo), Param::Int(hi)],
        },
        (Some(lo), None) => Fragment {
            sql: format!("{col} >= ?"),
            params: vec![Param::Int(lo)],
        },
        (None, Some(hi)) => Fragment {
            sql: format!("{col} <= ?"),
            params: vec![Param::Int(hi)],
        },
        (None, None) => unreachable!("settled by the is_unbounded arm above"),
    };

    // A NULL comparison is not `false` in SQL, it is NULL — and `NOT (NULL OR …)` is NULL again, which
    // a WHERE clause drops. So on its own `NULL BETWEEN …` would be harmless, but inside a negated
    // group `!(mdate:… OR …)` it would silently exclude every article that was never edited. The
    // matcher reports a missing timestamp as false in *both* polarities; this guard is what makes the
    // SQL say the same thing.
    if nullable {
        f.sql = format!("({col} IS NOT NULL AND {})", f.sql);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::regex::Pattern;

    const NOW: i64 = 1_136_214_245;

    fn ctx() -> SqlCtx<'static> {
        SqlCtx {
            alias: "e",
            entrytag_alias: "et",
            scoped_to_user: true,
        }
    }

    fn compile_query(q: &str) -> Fragment {
        compile(&parse(q, NOW).unwrap(), &ctx()).unwrap()
    }

    fn compile_err(q: &str) -> SearchError {
        compile(&parse(q, NOW).unwrap(), &ctx()).unwrap_err()
    }

    fn contains(needle: &str) -> TextPat {
        TextPat::contains(needle)
    }

    fn regex(source: &str) -> TextPat {
        TextPat::Regex(Pattern::parse(&format!("/{source}/")).unwrap())
    }

    // --- safety ------------------------------------------------------------

    #[test]
    fn no_value_is_ever_interpolated_into_the_sql() {
        // The single most important property of this module.
        // Each of these has to be *lexable* before it can be compiled: an unbalanced quote is a
        // parse error, which is the first line of defence. The quoted forms below get past the lexer
        // and are the ones that would reach the SQL builder.
        for hostile in [
            r#"intitle:"' OR 1=1 --""#,
            r#"intitle:"x'); DROP TABLE entry;--""#,
            "author:%_%",
            r#"#"tag'; --""#,
            r#"intext:"Robert'); DROP TABLE Students;--""#,
            r#"inurl:"x' UNION SELECT password FROM user--""#,
        ] {
            let f = compile_query(hostile);
            assert!(
                !f.sql.contains('\'')
                    || f.sql.contains("''")
                    || f.sql.contains("' #'")
                    || f.sql.contains("' AND ")
                    || f.sql.contains("' OR "),
                "a literal quote leaked into the SQL for {hostile:?}: {}",
                f.sql
            );
            assert!(
                !f.sql.to_uppercase().contains("DROP"),
                "input reached the SQL text: {}",
                f.sql
            );
            // Every placeholder has exactly one parameter.
            assert_eq!(
                f.sql.matches('?').count(),
                f.params.len(),
                "placeholder/parameter mismatch for {hostile:?}: {}",
                f.sql
            );
        }

        // An id list is parsed as numbers, so injection there is rejected rather than bound.
        assert!(matches!(
            parse("f:1;DELETE FROM entry", NOW).unwrap_err(),
            SearchError::BadOperand { .. }
        ));
        // `f:1 OR 1=1` *is* a valid boolean query: the second branch is the free text "1=1", which
        // gets bound as a parameter and searches for that literal string. It is not an injection, and
        // proving that is worth more than asserting a parse error.
        let f = compile_query("f:1 OR 1=1");
        assert!(
            !f.sql.contains("1=1"),
            "the text reached the SQL: {}",
            f.sql
        );
        assert!(
            f.params.contains(&Param::Text("1=1".into())),
            "{:?}",
            f.params
        );
    }

    #[test]
    fn placeholders_and_parameters_always_line_up() {
        for q in [
            "rust",
            "intitle:a intext:b author:c",
            "f:1,2,3 c:4,5",
            "a OR b OR c",
            "(a OR b) AND !c",
            "date:2006-01-02/2006-01-05 pubdate:P1W",
            "labels:x,y #t1 #t2",
            "l:1,2 l:*",
            "intitle:a #t labels:x,y",
        ] {
            let f = compile_query(q);
            assert_eq!(
                f.sql.matches('?').count(),
                f.params.len(),
                "{q} produced {}",
                f.sql
            );
        }
    }

    // --- text predicates ---------------------------------------------------

    #[test]
    fn a_substring_is_an_instr_over_the_folded_column() {
        let f = compile_query("intitle:rust");
        assert_eq!(f.sql, "instr(e.title_folded, ?) > 0");
        // Bound bare: `instr` is a literal substring test, so a user's `_` or `%` cannot become a
        // wildcard the way it would under LIKE.
        assert_eq!(f.params, vec![Param::Text("rust".into())]);
        assert!(
            !f.sql.contains("LIKE"),
            "LIKE is ASCII-only case-insensitive"
        );
    }

    #[test]
    fn a_regex_is_refused_in_a_listing() {
        // SQLite has no regex operator without a user-defined function, and the driver cannot
        // register one. Saying so is better than matching nothing: the reader can show the message,
        // and filter actions still evaluate regexes through the matcher.
        for q in [
            "intitle:/^ru.t$/i",
            "-intitle:/x/",
            "#/^ru/",
            "labels:/to.*/",
        ] {
            let e = compile_err(q);
            assert!(
                matches!(&e, SearchError::Unsupported(what) if what.contains("regular expression")),
                "{q}: {e}"
            );
        }
    }

    #[test]
    fn a_wildcard_looking_needle_stays_literal() {
        // FreshRSS binds `%a_b%` here, so `intitle:a_b` matches "axb" in a listing but not in a
        // filter action. Both backends treat the needle as text.
        let f = compile_query("intitle:a_b");
        assert_eq!(f.params, vec![Param::Text("a_b".into())]);
        assert!(
            !crate::normalize::ilike("axb", "a_b"),
            "`_` is not a single-character wildcard"
        );
    }

    #[test]
    fn every_text_operator_maps_to_its_column() {
        let cases = [
            ("intitle:x", "e.title"),
            ("intext:x", "e.content"),
            ("author:x", "e.author"),
            ("inurl:x", "e.link"),
        ];
        for (q, col) in cases {
            let f = compile_query(q);
            assert!(f.sql.contains(col), "{q} -> {}", f.sql);
        }
    }

    #[test]
    fn free_text_searches_title_or_content() {
        let f = compile_query("rust");
        assert_eq!(
            f.sql,
            "(instr(e.title_folded, ?) > 0 OR instr(e.content_folded, ?) > 0)"
        );
        assert_eq!(f.params.len(), 2, "the needle is bound twice");
        assert_eq!(f.params[0], f.params[1]);
    }

    #[test]
    fn negated_free_text_is_de_morganed() {
        let f = compile_query("-rust");
        assert_eq!(
            f.sql,
            "(instr(e.title_folded, ?) = 0 AND instr(e.content_folded, ?) = 0)"
        );
    }

    #[test]
    fn the_needle_is_folded_before_binding() {
        // Both backends fold; folding here means the database never sees an accented needle that its
        // collation could not match.
        let f = compile_query("intitle:Café");
        assert_eq!(f.params, vec![Param::Text("cafe".into())]);
    }

    // --- tags --------------------------------------------------------------

    #[test]
    fn a_tag_matches_whole_tags_only() {
        let f = compile_query("#rust");
        assert_eq!(f.sql, "instr(TRIM(e.tags_folded) || ' #', ?) > 0");
        // Both markers are load-bearing: the trailing " #" stops `#ru` matching the tag `rust`, and
        // the leading one stops `#st` matching it too.
        assert_eq!(f.params, vec![Param::Text("#rust #".into())]);
    }

    #[test]
    fn a_tag_prefix_or_suffix_does_not_match_a_longer_tag() {
        let column = "#rust #axum #";
        for q in ["#ru", "#st", "#rus"] {
            let f = compile_query(q);
            let needle = match &f.params[0] {
                Param::Text(t) => t.as_str(),
                other => panic!("{other:?}"),
            };
            assert!(
                !column.contains(needle),
                "{q} bound {needle:?}, which occurs in {column:?}"
            );
        }
        assert!(column.contains("#rust #"));
        assert!("#ru #axum #".contains("#ru #"));
    }

    #[test]
    fn a_negated_tag() {
        let f = compile_query("-#rust");
        assert_eq!(f.sql, "instr(TRIM(e.tags_folded) || ' #', ?) = 0");
    }

    #[test]
    fn a_tag_regex_is_refused_like_any_other() {
        // The one-tag-per-line rewrite existed only to give a regex something to anchor against; with
        // regexes out of listing SQL there is nothing left to rewrite.
        assert!(matches!(compile_err("#/^ru/"), SearchError::Unsupported(_)));
    }

    // --- ids ---------------------------------------------------------------

    #[test]
    fn an_id_list_becomes_in() {
        let f = compile_query("f:1,2,3");
        assert_eq!(f.sql, "e.id_feed IN (?,?,?)");
        assert_eq!(f.params, vec![Param::Int(1), Param::Int(2), Param::Int(3)]);
    }

    #[test]
    fn a_negated_id_list_becomes_not_in() {
        assert_eq!(compile_query("-f:1,2").sql, "e.id_feed NOT IN (?,?)");
        assert_eq!(compile_query("-e:1").sql, "e.id NOT IN (?)");
    }

    #[test]
    fn an_empty_id_list_matches_nothing_rather_than_erroring() {
        // `IN ()` is a SQLite syntax error, and "no ids" silently becoming "all ids" would be worse.
        assert_eq!(compile_query("f:").sql, "1=0");
        assert_eq!(compile_query("-f:").sql, "1=1");
        assert_eq!(compile_query("l:").sql, "1=0");
    }

    #[test]
    fn a_category_uses_a_subquery_so_the_row_count_cannot_change() {
        let f = compile_query("c:7");
        assert!(
            f.sql.starts_with("EXISTS (SELECT 1 FROM feed f2"),
            "{}",
            f.sql
        );
        assert!(f.sql.contains("f2.id = e.id_feed"), "{}", f.sql);
        assert_eq!(f.params, vec![Param::Int(7)]);
        assert_eq!(
            compile_query("-c:7").sql.split_whitespace().next(),
            Some("NOT")
        );
    }

    // --- labels ------------------------------------------------------------

    #[test]
    fn a_label_id_uses_an_exists_subquery() {
        let f = compile_query("l:3");
        assert!(f.sql.contains("EXISTS"), "{}", f.sql);
        assert!(f.sql.contains("entrytag et"), "{}", f.sql);
        assert!(f.sql.contains("et.id_entry = e.id"), "{}", f.sql);
        assert_eq!(f.params, vec![Param::Int(3)]);
    }

    #[test]
    fn the_label_wildcard_means_has_any_label() {
        let f = compile_query("l:*");
        assert!(f.sql.contains("EXISTS"), "{}", f.sql);
        assert!(f.params.is_empty(), "no ids to bind: {:?}", f.params);
        assert!(
            f.sql.contains("t.id_user = e.id_user"),
            "scoped per user: {}",
            f.sql
        );
    }

    #[test]
    fn a_negated_label_wildcard_means_has_no_label() {
        let f = compile_query("-l:*");
        assert!(f.sql.contains("NOT EXISTS"), "{}", f.sql);
    }

    #[test]
    fn a_label_name_matches_the_whole_name() {
        let f = compile_query("labels:todo");
        // `frss_ieq` is an accent-folded equality — one bound value per name, and the reason labels
        // do not use `frss_ilike`, which would let `labels:todo` match a label called "todos".
        assert_eq!(f.params, vec![Param::Text("todo".into())]);
        assert!(f.sql.contains("t.name_folded = ?"), "{}", f.sql);
        assert!(
            !crate::normalize::ieq("todos", "todo"),
            "a prefix must not match"
        );
        assert!(crate::normalize::ieq("Étiquette", "etiquette"));
    }

    #[test]
    fn several_label_names_are_ored() {
        let f = compile_query("labels:a,b");
        assert_eq!(f.sql.matches(" OR ").count(), 1, "{}", f.sql);
        assert_eq!(f.params.len(), 2, "one bound name each");
    }

    // --- dates -------------------------------------------------------------

    #[test]
    fn date_compares_against_the_id_in_microseconds() {
        // The surprising one, and the reason the scale factor exists: `date:` is ingestion time and
        // the id is a microsecond wall clock.
        let f = compile_query("date:2006-01-02");
        assert!(f.sql.contains("e.id"), "{}", f.sql);
        match &f.params[0] {
            Param::Int(v) => assert!(*v > 1_000_000_000_000_000, "{v} looks like seconds, not µs"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pubdate_compares_against_the_date_column_in_seconds() {
        let f = compile_query("pubdate:2006-01-02");
        assert!(f.sql.contains("e.date"), "{}", f.sql);
        match &f.params[0] {
            Param::Int(v) => assert!(*v < 10_000_000_000, "{v} looks like microseconds"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_date_field_maps_to_its_column() {
        for (q, col) in [
            ("date:2006", "e.id"),
            ("pubdate:2006", "e.date"),
            ("mdate:2006", "e.lastModified"),
            ("userdate:2006", "e.lastUserModified"),
        ] {
            let f = compile_query(q);
            assert!(f.sql.contains(col), "{q} -> {}", f.sql);
        }
    }

    #[test]
    fn a_two_sided_range_is_between() {
        let f = compile_query("date:2006-01-02/2006-01-05");
        assert!(f.sql.contains("BETWEEN ? AND ?"), "{}", f.sql);
        assert_eq!(f.params.len(), 2);
        assert!(f.params[0] < f.params[1]);
    }

    #[test]
    fn a_one_sided_range_is_an_inequality() {
        // `P1W` is open-ended into the future.
        let f = compile_query("date:P1W");
        assert_eq!(f.sql, "e.id >= ?");
        assert_eq!(f.params.len(), 1);
    }

    #[test]
    fn a_negated_two_sided_range_becomes_two_disjoint_halves() {
        let f = compile_query("-date:2006-01-02/2006-01-05");
        assert_eq!(f.sql, "(e.id < ? OR e.id > ?)");
        assert_eq!(f.params.len(), 2);
        assert!(
            f.params[0] < f.params[1],
            "the lower half bound comes first: {:?}",
            f.params
        );
        // And it must not also be wrapped in NOT, which would cancel the complement out.
        assert!(!f.sql.contains("NOT"), "{}", f.sql);
    }

    #[test]
    fn negating_a_band_reuses_the_bands_own_bounds() {
        // `< lo OR > hi`, with the very bounds the positive form uses. An off-by-one in
        // `DateRange::negate` shows up here as the day either side of the band going missing.
        let positive = compile_query("date:2006-01-02/2006-01-05");
        let negative = compile_query("-date:2006-01-02/2006-01-05");
        assert_eq!(positive.sql, "e.id BETWEEN ? AND ?");
        assert_eq!(negative.params, positive.params);
    }

    #[test]
    fn a_negated_one_sided_range_flips_the_comparison() {
        let f = compile_query("-date:P1W");
        assert_eq!(f.sql, "e.id <= ?", "NOT (>= min) is <= max");
    }

    /// An unbounded range cannot be reached from the grammar — `date:` always produces at least one
    /// bound — but a saved query or a programmatically built AST can hold one, and its complement is
    /// "nothing", which `DateRange` has no way to spell.
    fn unbounded(field: DateField, negated: bool) -> Fragment {
        compile(
            &Expr::pred(Pred::Date {
                field,
                range: DateRange::default(),
                negated,
            }),
            &ctx(),
        )
        .unwrap()
    }

    #[test]
    fn an_unbounded_range_is_not_null_rather_than_true() {
        // `mdate:` over a row that was never edited must not match: a NULL timestamp fails a range in
        // both polarities, and `1=1` would say otherwise.
        let f = unbounded(DateField::Modified, false);
        assert_eq!(f.sql, "e.lastModified IS NOT NULL");
        assert!(f.params.is_empty());
    }

    #[test]
    fn a_negated_unbounded_range_matches_nothing() {
        // Complementing an unbounded range through `DateRange::negate()` returns it unchanged, so
        // this arm has to be settled before the complement is taken.
        let f = unbounded(DateField::Publication, true);
        assert_eq!(f.sql, "1=0");
        assert!(f.params.is_empty());
    }

    #[test]
    fn a_nullable_timestamp_column_is_guarded_against_null() {
        // `NULL BETWEEN …` is NULL, not false, and `NOT (NULL OR …)` is NULL again — which a WHERE
        // clause drops. So without the guard, `!(mdate:2006 OR rust)` would exclude every article
        // that was never edited. The matcher reports a missing timestamp as false in both polarities.
        let f = compile_query("mdate:2006-01-02");
        assert_eq!(
            f.sql,
            "(e.lastModified IS NOT NULL AND e.lastModified BETWEEN ? AND ?)"
        );
        let f = compile_query("userdate:2006-01-02");
        assert!(
            f.sql.starts_with("(e.lastUserModified IS NOT NULL AND "),
            "{}",
            f.sql
        );
    }

    #[test]
    fn the_non_nullable_timestamp_columns_are_left_alone() {
        // `entry.id` and `entry.date` cannot be NULL, and `date:` is by far the most common operator —
        // paying for a guard there would be noise in every listing query.
        assert_eq!(compile_query("date:2006-01-02").sql, "e.id BETWEEN ? AND ?");
        assert_eq!(
            compile_query("pubdate:2006-01-02").sql,
            "e.date BETWEEN ? AND ?"
        );
    }

    // --- boolean structure -------------------------------------------------

    #[test]
    fn conjunctions_and_disjunctions_are_parenthesized() {
        let f = compile_query("a OR b");
        // Each branch is parenthesized by `Fragment::join`, and the free-text fragment parenthesizes
        // its own OR. The doubled brackets are redundant but harmless; what matters is that the two
        // branches cannot be re-associated by a surrounding operator.
        assert_eq!(f.sql.matches(" OR ").count(), 3, "{}", f.sql);
        assert_eq!(f.params.len(), 4);
        assert!(
            f.sql.starts_with("((") && f.sql.ends_with("))"),
            "{}",
            f.sql
        );
    }

    #[test]
    fn a_leaf_of_several_predicates_is_anded_without_extra_nesting() {
        let f = compile_query("intitle:a intext:b");
        assert_eq!(
            f.sql,
            "(instr(e.title_folded, ?) > 0) AND (instr(e.content_folded, ?) > 0)"
        );
    }

    #[test]
    fn a_negated_group_becomes_not() {
        let f = compile_query("!(a OR b)");
        assert!(f.sql.starts_with("NOT ("), "{}", f.sql);
    }

    #[test]
    fn an_empty_query_is_true() {
        assert_eq!(compile_query("").sql, "1=1");
        assert!(compile_query("").params.is_empty());
    }

    #[test]
    fn the_alias_is_honoured_throughout() {
        let ctx = SqlCtx {
            alias: "x",
            entrytag_alias: "xt",
            scoped_to_user: true,
        };
        let f = compile(&parse("intitle:a f:1 #t l:*", NOW).unwrap(), &ctx).unwrap();
        assert!(f.sql.contains("x.title"), "{}", f.sql);
        assert!(f.sql.contains("x.id_feed"), "{}", f.sql);
        assert!(f.sql.contains("x.tags"), "{}", f.sql);
        assert!(f.sql.contains("entrytag xt"), "{}", f.sql);
        assert!(
            !f.sql.contains("e."),
            "no leftover default alias: {}",
            f.sql
        );
    }

    #[test]
    fn an_unscoped_query_omits_the_user_predicate() {
        // An admin-wide query must not filter by `e.id_user`, which may not even be in scope.
        let ctx = SqlCtx {
            alias: "e",
            entrytag_alias: "et",
            scoped_to_user: false,
        };
        let f = compile(&parse("l:*", NOW).unwrap(), &ctx).unwrap();
        assert!(!f.sql.contains("id_user"), "{}", f.sql);
    }

    // --- saved queries -----------------------------------------------------

    #[test]
    fn an_unexpanded_saved_query_is_an_error_not_a_silent_pass() {
        // Falling back to `1=1` here would return every article the user owns.
        let e = compile(&parse("S:3", NOW).unwrap(), &ctx()).unwrap_err();
        assert!(matches!(e, SearchError::Unsupported(_)), "{e}");
        assert!(e.to_string().contains("S:"), "{e}");

        let e = compile(&parse("search:x", NOW).unwrap(), &ctx()).unwrap_err();
        assert!(e.to_string().contains("search:"), "{e}");
    }

    // --- determinism -------------------------------------------------------

    #[test]
    fn compiling_the_same_query_twice_is_identical() {
        for q in ["a OR b", "intitle:x f:1,2 date:P1W", "(a OR b) AND !c"] {
            assert_eq!(compile_query(q), compile_query(q), "{q}");
        }
    }

    #[test]
    fn a_long_query_does_not_produce_deeply_nested_sql() {
        // Flattening in `ast::combine` is what keeps this readable; twenty nested pairs would
        // otherwise emit twenty levels of parentheses.
        let q = (0..20)
            .map(|i| format!("intitle:t{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let f = compile_query(&q);
        assert_eq!(f.sql.matches(" AND ").count(), 19);
        assert!(!f.sql.contains("(("), "no redundant nesting: {}", f.sql);
    }

    #[test]
    fn helpers_are_reachable_for_the_golden_tests() {
        // Guards the constructors used by `crates/freshrss-search/tests/golden.rs`.
        let text = crate::normalize::Text::of("axb");
        assert!(contains("x").matches(&text));
        assert!(regex("x").matches(&text));
        assert!(!Fragment::new("1=1").is_empty());
    }
}
