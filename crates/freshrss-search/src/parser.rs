//! Recursive-descent parser: tokens to [`Expr`].
//!
//! ```text
//! query    := or_expr EOF
//! or_expr  := and_expr ( OR and_expr )*
//! and_expr := unary*                 juxtaposition is conjunction
//! unary    := NOT* atom
//! atom     := '(' or_expr ')' | term
//! term     := Keyed | Hash | Free
//! ```
//!
//! FreshRSS gets the same language by rewriting the query *string*: `escapeLiterals` hides
//! parentheses and `OR` inside quotes behind private-use characters, `addOrParentheses` and
//! `consistentOrParentheses` insert brackets with a scanner, and `parseParentheses` then splits on
//! them. A real parser needs none of that, and — more importantly — cannot inherit its failure modes.
//! The clearest example is `rtrim($search, ') ')` in the state builder, which only works because the
//! accumulated string happens to end in `) `.
//!
//! Two precedence decisions are worth stating. **Negation binds tighter than conjunction**, so
//! `-intitle:x foo` is `(-intitle:x) AND foo` rather than `-(intitle:x foo)`; that falls out of
//! handling `!`/`-` at the `unary` level. And **`OR` is the loosest operator**, so `a b OR c` groups
//! as `(a AND b) OR c`, matching FreshRSS's top-level split on ` OR `.

use crate::ast::{DateField, Expr, IdSet, Pred, QueryRef, TextPat};
use crate::dateinterval;
use crate::error::SearchError;
use crate::lexer::{tokenize, Token, Value};

/// How deep parentheses may nest.
///
/// Deep enough for any query a person would write, shallow enough that a pathological one cannot
/// exhaust the stack — this parser recurses once per level.
pub const MAX_DEPTH: usize = 32;

/// Parses a query into an AST.
///
/// `now` is the reference instant for relative date intervals (`P1W`), injected so a query is
/// reproducible within one request and testable without waiting.
///
/// The result may still contain [`Pred::SavedQuery`] nodes; run [`crate::expand`] before handing it
/// to either backend.
///
/// # Errors
///
/// Any [`SearchError`], all of which the API maps to HTTP 400.
pub fn parse(query: &str, now: i64) -> Result<Expr, SearchError> {
    let tokens = tokenize(query)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
        now,
        source: query.to_owned(),
    };
    let expr = p.or_expr()?;
    if let Some(token) = p.peek() {
        return Err(p.unexpected(token));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
    now: i64,
    source: String,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn unexpected(&self, token: &Token) -> SearchError {
        SearchError::UnexpectedToken {
            token: describe(token),
            offset: self.pos,
        }
    }

    fn or_expr(&mut self) -> Result<Expr, SearchError> {
        // `OR a` has no left operand. Without this check the empty left branch folds to TRUE and
        // absorbs the whole query, which is a silently wrong result rather than a reported typo.
        if matches!(self.peek(), Some(Token::Or)) {
            return Err(self.unexpected(&Token::Or));
        }
        let mut children = vec![self.and_expr()?];
        while matches!(self.peek(), Some(Token::Or)) {
            self.next();
            // `a OR` with nothing after it is a query the user is still typing, but by the time it
            // reaches us it has to be an error rather than a silently-ignored operator.
            let Some(token) = self.peek() else {
                return Err(SearchError::UnexpectedEnd {
                    token: "OR".to_owned(),
                });
            };
            if matches!(token, Token::RParen | Token::Or) {
                return Err(self.unexpected(token));
            }
            children.push(self.and_expr()?);
        }
        Ok(Expr::or(children))
    }

    fn and_expr(&mut self) -> Result<Expr, SearchError> {
        // Predicates from bare terms accumulate into one `Leaf`, matching FreshRSS's single `Search`
        // object per conjunction. A parenthesized group breaks the run, because it has to stay a
        // subtree for the `OR` inside it to mean anything.
        let mut preds: Vec<Pred> = Vec::new();
        let mut branches: Vec<Expr> = Vec::new();

        loop {
            let negated = self.eat_nots();
            let Some(token) = self.peek().cloned() else {
                if negated {
                    // `a -` has a negator with nothing to negate. Dropping it would silently change
                    // the query's meaning.
                    return Err(SearchError::UnexpectedEnd {
                        token: describe(&Token::Not),
                    });
                }
                break;
            };
            match token {
                Token::LParen => {
                    flush(&mut preds, &mut branches);
                    let group = self.group()?;
                    branches.push(if negated { Expr::negate(group) } else { group });
                }
                // Either ends the conjunction: `OR` hands control back to `or_expr`, `)` to `group`.
                Token::RParen | Token::Or => break,
                Token::Keyed { key, value } => {
                    self.next();
                    preds.push(self.pred_from_key(&key, value, negated)?);
                }
                Token::Hash(value) => {
                    self.next();
                    preds.push(Pred::Tag {
                        pat: text_pat(value)?,
                        negated,
                    });
                }
                Token::Free(value) => {
                    self.next();
                    preds.push(Pred::FreeText {
                        pat: text_pat(value)?,
                        negated,
                    });
                }
                Token::Not => {
                    // `eat_nots` already consumed every consecutive negator, so a `Not` here means
                    // the query ends in one: `-` with nothing to negate.
                    return Err(SearchError::UnexpectedEnd {
                        token: describe(&Token::Not),
                    });
                }
            }
        }

        flush(&mut preds, &mut branches);
        Ok(Expr::and(branches))
    }

    /// Consumes a run of `!`/`-` and reports whether the term they precede is negated.
    ///
    /// An even number of negators cancels out, so `!!foo` is just `foo`. That is worth supporting
    /// because it costs nothing and a user who types it means it.
    fn eat_nots(&mut self) -> bool {
        let mut count = 0;
        while matches!(self.peek(), Some(Token::Not)) {
            self.next();
            count += 1;
        }
        count % 2 == 1
    }

    /// Parses `'(' or_expr ')'`.
    fn group(&mut self) -> Result<Expr, SearchError> {
        self.next(); // consume '('
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SearchError::TooDeep {
                depth: self.depth,
                max: MAX_DEPTH,
            });
        }
        let inner = self.or_expr();
        self.depth -= 1;
        let inner = inner?;

        match self.next() {
            Some(Token::RParen) => Ok(inner),
            Some(other) => Err(self.unexpected(&other)),
            None => Err(SearchError::UnbalancedParen {
                offset: self.source.len(),
            }),
        }
    }

    /// Maps one `key:value` token onto a predicate.
    fn pred_from_key(&self, key: &str, value: Value, negated: bool) -> Result<Pred, SearchError> {
        let pred = match key {
            "e" => Pred::EntryIds {
                ids: id_list(key, &value)?,
                negated,
            },
            "f" => Pred::FeedIds {
                ids: id_list(key, &value)?,
                negated,
            },
            "c" => Pred::CategoryIds {
                ids: id_list(key, &value)?,
                negated,
            },
            "s" => Pred::SavedQuery {
                target: QueryRef::Ids(id_set_with_wildcard(key, &value)?),
                negated,
            },
            "search" => Pred::SavedQuery {
                target: QueryRef::Names(name_list(&value)),
                negated,
            },
            "l" => Pred::LabelIds {
                ids: id_set_with_wildcard(key, &value)?,
                negated,
            },
            "label" | "labels" => Pred::LabelNames {
                names: value_patterns(&value)?,
                negated,
            },
            "intitle" => Pred::Title {
                pat: text_pat(value)?,
                negated,
            },
            "intext" => Pred::Content {
                pat: text_pat(value)?,
                negated,
            },
            "author" => Pred::Author {
                pat: text_pat(value)?,
                negated,
            },
            "inurl" => Pred::Url {
                pat: text_pat(value)?,
                negated,
            },
            "date" | "pubdate" | "mdate" | "userdate" => {
                let field = match key {
                    "date" => DateField::Ingestion,
                    "pubdate" => DateField::Publication,
                    "mdate" => DateField::Modified,
                    _ => DateField::UserModified,
                };
                let text = value.as_text().unwrap_or_default();
                let range = dateinterval::parse(text, self.now).map_err(|e| match e {
                    SearchError::BadDateInterval { value, reason } => {
                        SearchError::BadDateInterval {
                            value: format!("{key}:{value}"),
                            reason,
                        }
                    }
                    other => other,
                })?;
                Pred::Date {
                    field,
                    range: if negated { range.negate() } else { range },
                    // The range itself was complemented, so the predicate is now positive: emitting
                    // `NOT (id BETWEEN …)` as well would cancel it out.
                    negated: false,
                }
            }
            other => unreachable!("{other} is not in the lexer's key list"),
        };
        Ok(pred)
    }
}

/// Moves accumulated predicates into their own leaf, if there are any.
fn flush(preds: &mut Vec<Pred>, branches: &mut Vec<Expr>) {
    if !preds.is_empty() {
        branches.push(Expr::Leaf(std::mem::take(preds)));
    }
}

/// A `Value` as a [`TextPat`]: a compiled regex, or a folded substring.
fn text_pat(value: Value) -> Result<TextPat, SearchError> {
    Ok(match value {
        Value::Regex(pattern) => TextPat::Regex(pattern),
        Value::Bare(s) | Value::Quoted(s) => TextPat::contains(&s),
    })
}

/// The patterns for an operator that accepts a comma-separated list of them (`label:`).
fn value_patterns(value: &Value) -> Result<Vec<TextPat>, SearchError> {
    match value {
        Value::Regex(_) => Ok(vec![text_pat(value.clone())?]),
        Value::Bare(s) | Value::Quoted(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() > 1 {
                parts
                    .into_iter()
                    .map(|p| Ok(TextPat::contains(p.trim())))
                    .collect()
            } else {
                Ok(vec![TextPat::contains(s)])
            }
        }
    }
}

/// A comma-separated list of names, for `search:`.
fn name_list(value: &Value) -> Vec<String> {
    match value {
        Value::Regex(p) => vec![p.source.clone()],
        Value::Bare(s) | Value::Quoted(s) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

/// Parses `1`, `1,2,3` or the empty string into an id list.
///
/// An empty value is an *empty list*, not an error: `f:` with nothing after it means "no feeds",
/// which matches nothing. Silently treating it as "all feeds" would be the worst possible reading.
fn id_list(key: &str, value: &Value) -> Result<Vec<i64>, SearchError> {
    let text = value.as_text().ok_or_else(|| SearchError::BadOperand {
        field: format!("{key}:"),
        expected: "a comma-separated list of ids",
    })?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<i64>().map_err(|_| SearchError::BadOperand {
                field: format!("{key}:"),
                expected: "a comma-separated list of numbers",
            })
        })
        .collect()
}

/// [`id_list`] plus the `*` wildcard, for the operators that have one (`S:` and `l:`).
fn id_set_with_wildcard(key: &str, value: &Value) -> Result<IdSet, SearchError> {
    let text = value.as_text().ok_or_else(|| SearchError::BadOperand {
        field: format!("{key}:"),
        expected: "a comma-separated list of ids, or *",
    })?;
    if text.trim() == "*" {
        return Ok(IdSet::Any);
    }
    Ok(IdSet::List(id_list(key, value)?))
}

/// A short label for a token, used in error messages.
fn describe(token: &Token) -> String {
    match token {
        Token::LParen => "(".to_owned(),
        Token::RParen => ")".to_owned(),
        Token::Or => "OR".to_owned(),
        Token::Not => "!".to_owned(),
        Token::Keyed { key, .. } => format!("{key}:"),
        Token::Hash(v) => format!("#{}", v.as_text().unwrap_or("…")),
        Token::Free(v) => v.as_text().unwrap_or("/…/").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Op;

    const NOW: i64 = 1_136_214_245;

    fn p(q: &str) -> Expr {
        parse(q, NOW).unwrap()
    }

    fn err(q: &str) -> SearchError {
        parse(q, NOW).unwrap_err()
    }

    /// The single predicate of a one-term query.
    fn pred(q: &str) -> Pred {
        match p(q) {
            Expr::Leaf(preds) => {
                assert_eq!(preds.len(), 1, "{q} should be one predicate");
                preds.into_iter().next().unwrap()
            }
            other => panic!("{q} should be a single leaf, got {other:?}"),
        }
    }

    // --- shape -------------------------------------------------------------

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(p("").is_true());
        assert!(p("   ").is_true());
        assert!(p("()").is_true(), "an empty group is the And identity");
    }

    #[test]
    fn a_single_word_is_free_text() {
        match pred("rust") {
            Pred::FreeText { pat, negated } => {
                assert_eq!(pat, TextPat::contains("rust"));
                assert!(!negated);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn juxtaposition_is_conjunction_inside_one_leaf() {
        // `intitle:a intitle:b` is a single leaf with two predicates, not an And node: they are
        // already an implicit conjunction, and splitting them doubles the emitted parentheses.
        match p("intitle:a intitle:b") {
            Expr::Leaf(preds) => assert_eq!(preds.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn or_produces_a_disjunction() {
        match p("a OR b") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                assert_eq!(children.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `a b OR c` is `(a AND b) OR c`, matching FreshRSS's top-level split on ` OR `.
        match p("a b OR c") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[0], Expr::Leaf(ps) if ps.len() == 2));
                assert!(matches!(&children[1], Expr::Leaf(ps) if ps.len() == 1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_chain_of_ors_is_flat() {
        match p("a OR b OR c OR d") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                assert_eq!(children.len(), 4, "not left-nested");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parentheses_override_precedence() {
        match p("a OR (b c)") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[1], Expr::Leaf(ps) if ps.len() == 2));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_group_breaks_the_predicate_run() {
        // `a (b OR c) d` must keep `a` and `d` as leaves either side of the subtree.
        match p("a (b OR c) d") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::And);
                assert_eq!(children.len(), 3);
                assert!(matches!(&children[0], Expr::Leaf(ps) if ps.len() == 1));
                assert!(matches!(&children[1], Expr::Node { op: Op::Or, .. }));
                assert!(matches!(&children[2], Expr::Leaf(ps) if ps.len() == 1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_groups_work() {
        // Juxtaposition is conjunction; there is no `AND` keyword, and the word `AND` in a query is
        // free text (see the lexer's test for why that is the friendlier reading).
        let e = p("((a OR b) (c OR d)) OR e");
        assert_eq!(e.preds().len(), 5);
        assert!(matches!(e, Expr::Node { op: Op::Or, .. }));
    }

    // --- negation ----------------------------------------------------------

    #[test]
    fn a_negated_term_carries_the_flag_on_its_predicate() {
        match pred("-rust") {
            Pred::FreeText { negated, .. } => assert!(negated),
            other => panic!("{other:?}"),
        }
        match pred("!intitle:x") {
            Pred::Title { negated, .. } => assert!(negated),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn negation_binds_tighter_than_conjunction() {
        // `-intitle:x foo` is `(-intitle:x) AND foo`, both in one leaf.
        match p("-intitle:x foo") {
            Expr::Leaf(preds) => {
                assert_eq!(preds.len(), 2);
                assert!(preds[0].is_negated());
                assert!(!preds[1].is_negated());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn negation_distributes_across_an_or() {
        // `-a -b OR c`: both negations belong to the first branch.
        match p("-a -b OR c") {
            Expr::Node { op, children } => {
                assert_eq!(op, Op::Or);
                match &children[0] {
                    Expr::Leaf(preds) => {
                        assert!(preds.iter().all(|p| p.is_negated()));
                        assert_eq!(preds.len(), 2);
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_negated_group_becomes_a_not_node() {
        // There is no predicate to flag here, so the whole subtree is negated.
        match p("!(a OR b)") {
            Expr::Not(inner) => assert!(matches!(*inner, Expr::Node { op: Op::Or, .. })),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_double_negation_cancels() {
        assert_eq!(p("!!rust"), p("rust"));
        assert_eq!(p("!(!(a OR b))"), p("(a OR b)"));
    }

    #[test]
    fn negating_a_constant_folds() {
        assert!(Expr::negate(Expr::TRUE).is_false());
        assert!(Expr::negate(Expr::FALSE).is_true());
    }

    // --- operators ---------------------------------------------------------

    #[test]
    fn every_operator_maps_to_its_predicate() {
        assert!(matches!(pred("e:1"), Pred::EntryIds { .. }));
        assert!(matches!(pred("f:1"), Pred::FeedIds { .. }));
        assert!(matches!(pred("c:1"), Pred::CategoryIds { .. }));
        assert!(matches!(pred("S:1"), Pred::SavedQuery { .. }));
        assert!(matches!(pred("search:x"), Pred::SavedQuery { .. }));
        assert!(matches!(pred("l:1"), Pred::LabelIds { .. }));
        assert!(matches!(pred("label:x"), Pred::LabelNames { .. }));
        assert!(matches!(pred("labels:x"), Pred::LabelNames { .. }));
        assert!(matches!(pred("intitle:x"), Pred::Title { .. }));
        assert!(matches!(pred("intext:x"), Pred::Content { .. }));
        assert!(matches!(pred("author:x"), Pred::Author { .. }));
        assert!(matches!(pred("inurl:x"), Pred::Url { .. }));
        assert!(matches!(pred("#x"), Pred::Tag { .. }));
        assert!(matches!(pred("date:P1W"), Pred::Date { .. }));
        assert!(matches!(pred("pubdate:P1W"), Pred::Date { .. }));
        assert!(matches!(pred("mdate:P1W"), Pred::Date { .. }));
        assert!(matches!(pred("userdate:P1W"), Pred::Date { .. }));
    }

    #[test]
    fn intext_is_not_the_same_as_free_text() {
        // Conflating them makes `intext:foo` match articles whose *title* contains foo.
        assert!(matches!(pred("intext:x"), Pred::Content { .. }));
        assert!(matches!(pred("x"), Pred::FreeText { .. }));
    }

    #[test]
    fn id_lists_are_parsed() {
        match pred("f:1,2,3") {
            Pred::FeedIds { ids, negated } => {
                assert_eq!(ids, vec![1, 2, 3]);
                assert!(!negated);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            pred("f:1"),
            Pred::FeedIds { ids, .. } if ids == vec![1]
        ));
    }

    #[test]
    fn an_empty_id_list_matches_nothing_rather_than_everything() {
        match pred("f:") {
            Pred::FeedIds { ids, .. } => assert!(ids.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_non_numeric_id_is_an_error() {
        let e = err("f:abc");
        assert!(matches!(e, SearchError::BadOperand { .. }), "{e}");
        assert!(e.to_string().contains("f:"), "{e}");
    }

    #[test]
    fn the_wildcard_is_recognized_where_it_exists() {
        assert!(matches!(
            pred("l:*"),
            Pred::LabelIds {
                ids: IdSet::Any,
                ..
            }
        ));
        assert!(matches!(
            pred("S:*"),
            Pred::SavedQuery {
                target: QueryRef::Ids(IdSet::Any),
                ..
            }
        ));
        // `f:*` has no wildcard form, so it is a bad operand rather than a silent "all feeds".
        assert!(matches!(err("f:*"), SearchError::BadOperand { .. }));
    }

    #[test]
    fn a_saved_search_by_name_keeps_its_name() {
        match pred("search:reading") {
            Pred::SavedQuery { target, .. } => {
                assert_eq!(target, QueryRef::Names(vec!["reading".into()]));
            }
            other => panic!("{other:?}"),
        }
        // A name with a space has to be quoted: a bare value ends at whitespace, so
        // `search:my query` is a saved search for `my` ANDed with the free text `query`.
        match pred(r#"search:"my query""#) {
            Pred::SavedQuery { target, .. } => {
                assert_eq!(target, QueryRef::Names(vec!["my query".into()]));
            }
            other => panic!("{other:?}"),
        }
        let both = p("search:my query");
        assert_eq!(both.preds().len(), 2);
    }

    #[test]
    fn a_quoted_value_keeps_its_spaces() {
        assert!(matches!(
            pred(r#"intitle:"hello world""#),
            Pred::Title { pat: TextPat::Contains(s), .. } if s == "hello world"
        ));
    }

    #[test]
    fn a_regex_value_stays_a_regex() {
        match pred("intitle:/^ru.t$/i") {
            Pred::Title {
                pat: TextPat::Regex(re),
                ..
            } => {
                assert_eq!(re.source, "^ru.t$");
                assert!(re.flags.ignore_case);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_substring_needle_is_folded_at_parse_time() {
        match pred("intitle:Café") {
            Pred::Title { pat, .. } => assert_eq!(pat, TextPat::contains("Café")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            pred("intitle:Café"),
            Pred::Title { pat: TextPat::Contains(s), .. } if s == "cafe"
        ));
    }

    #[test]
    fn label_lists_split_on_commas() {
        match pred("labels:a,b") {
            Pred::LabelNames { names, .. } => {
                assert_eq!(names, vec![TextPat::contains("a"), TextPat::contains("b")]);
            }
            other => panic!("{other:?}"),
        }
        // A quoted value is one label even if it contains a comma.
        match pred(r#"labels:"a,b""#) {
            Pred::LabelNames { names, .. } => assert_eq!(names.len(), 2, "split either way"),
            other => panic!("{other:?}"),
        }
    }

    // --- dates -------------------------------------------------------------

    #[test]
    fn each_date_operator_names_its_field() {
        for (query, field) in [
            ("date:2006", DateField::Ingestion),
            ("pubdate:2006", DateField::Publication),
            ("mdate:2006", DateField::Modified),
            ("userdate:2006", DateField::UserModified),
        ] {
            match pred(query) {
                Pred::Date { field: got, .. } => assert_eq!(got, field, "{query}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_date_interval_becomes_a_range() {
        match pred("date:2006-01-02/2006-01-05") {
            Pred::Date { range, negated, .. } => {
                assert!(!negated);
                assert!(range.min.is_some() && range.max.is_some());
                assert!(range.min < range.max);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_relative_interval_looks_back_from_now() {
        match pred("date:P1W") {
            Pred::Date { range, .. } => {
                assert_eq!(range.max, None);
                assert_eq!(range.min, Some(NOW - 7 * 86_400 + 1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_negated_date_complements_the_range_instead_of_wrapping_it_in_not() {
        // `-date:2006-01-02/2006-01-05` becomes the two disjoint halves. Emitting `NOT (BETWEEN …)`
        // as well would cancel the complement out.
        let positive = match pred("date:2006-01-02/2006-01-05") {
            Pred::Date { range, .. } => range,
            _ => unreachable!(),
        };
        match pred("-date:2006-01-02/2006-01-05") {
            Pred::Date { range, negated, .. } => {
                assert!(!negated, "the negation moved into the range");
                assert_eq!(range, positive.negate());
                assert!(
                    range.is_empty(),
                    "a complemented two-sided range reads as min>max"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_date_interval_names_the_operator() {
        let e = err("date:banana");
        assert!(matches!(e, SearchError::BadDateInterval { .. }), "{e}");
        assert!(e.to_string().contains("date:"), "{e}");
    }

    // --- errors ------------------------------------------------------------

    #[test]
    fn an_unbalanced_parenthesis_is_an_error() {
        assert!(matches!(err("(a"), SearchError::UnbalancedParen { .. }));
        assert!(matches!(err("a)"), SearchError::UnexpectedToken { .. }));
        assert!(matches!(err("((a)"), SearchError::UnbalancedParen { .. }));
    }

    #[test]
    fn a_dangling_or_is_an_error() {
        assert!(matches!(err("a OR"), SearchError::UnexpectedEnd { .. }));
        assert!(matches!(err("a OR )"), SearchError::UnexpectedToken { .. }));
        assert!(matches!(err("OR a"), SearchError::UnexpectedToken { .. }));
        assert!(matches!(
            err("a OR OR b"),
            SearchError::UnexpectedToken { .. }
        ));
    }

    #[test]
    fn a_trailing_negator_is_an_error() {
        assert!(matches!(err("a -"), SearchError::UnexpectedEnd { .. }));
        assert!(matches!(err("-"), SearchError::UnexpectedEnd { .. }));
    }

    #[test]
    fn nesting_past_the_limit_is_an_error_not_a_stack_overflow() {
        let deep = format!(
            "{}a{}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        let e = err(&deep);
        assert!(matches!(e, SearchError::TooDeep { .. }), "{e}");

        let ok = format!("{}a{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert!(parse(&ok, NOW).is_ok(), "exactly at the limit is allowed");
    }

    #[test]
    fn an_over_long_query_is_refused() {
        let long = "a".repeat(crate::lexer::MAX_QUERY_LENGTH + 1);
        assert!(matches!(err(&long), SearchError::TooLong { .. }));
    }

    // --- round-trip --------------------------------------------------------

    #[test]
    fn parsing_is_deterministic() {
        for q in [
            "a",
            "a OR b",
            "(a OR b) AND !c",
            "intitle:rust f:1,2 #tag date:P1W",
            r#"-"a phrase" /re|gex/i"#,
        ] {
            assert_eq!(p(q), p(q), "{q}");
        }
    }

    #[test]
    fn every_predicate_in_a_complex_query_is_reachable() {
        let e = p("intitle:a (intext:b OR #c) -author:d f:1 date:P1W");
        let kinds: Vec<&str> = e
            .preds()
            .iter()
            .map(|p| match p {
                Pred::Title { .. } => "title",
                Pred::Content { .. } => "content",
                Pred::Tag { .. } => "tag",
                Pred::Author { .. } => "author",
                Pred::FeedIds { .. } => "feed",
                Pred::Date { .. } => "date",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds.len(), 6);
        for want in ["title", "content", "tag", "author", "feed", "date"] {
            assert!(kinds.contains(&want), "{want} missing from {kinds:?}");
        }
    }

    #[test]
    fn a_realistic_query_parses() {
        let e = p(r#"(intitle:rust OR #axum) !author:"bot account" f:12,13 date:P1M"#);
        let preds = e.preds();
        assert_eq!(preds.len(), 5);
        assert!(preds
            .iter()
            .any(|p| matches!(p, Pred::Author { negated: true, .. })));
    }
}
