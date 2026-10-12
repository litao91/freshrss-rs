//! The tokenizer.
//!
//! FreshRSS has no lexer. `BooleanSearch` works by rewriting the query *string* — `escapeLiterals`
//! protects parentheses and `OR` inside quotes by substituting private-use characters, then
//! `addOrParentheses` and `consistentOrParentheses` insert brackets with a hand-rolled scanner, and
//! `Search` extracts each operator with a regex that *removes* what it matched from the string so the
//! next operator's regex sees what is left. That works, but every step depends on the exact shape of
//! the text the previous step produced, and `rtrim($search, ') ')` in the state builder is the clearest
//! example of the fragility.
//!
//! A real tokenizer removes the whole class of problem: quoting, escaping and operator boundaries are
//! decided once, here, and the parser sees a clean token stream.

use crate::error::SearchError;
use crate::regex::Pattern;

/// One lexical unit.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    LParen,
    RParen,
    /// The `OR` operator, recognized case-insensitively as a standalone word.
    Or,
    /// A leading `!` or `-`, applying to the atom that follows it.
    Not,
    /// `key:value` — `intitle:rust`, `f:1,2`, `date:P1W`.
    Keyed {
        key: String,
        value: Value,
    },
    /// `#tag`.
    Hash(Value),
    /// A bare word, matched against title and content.
    Free(Value),
}

/// The right-hand side of a keyed term, a tag, or a free word.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Unquoted text, terminated by whitespace or a parenthesis.
    Bare(String),
    /// `"…"` or `'…'`, with `\\` and the matching quote escapable.
    Quoted(String),
    /// `/…/im`, compiled during lexing so a bad pattern is a parse error and not a runtime one.
    Regex(Pattern),
}

impl Value {
    /// The text to search for, or `None` for a regex (which has no single plaintext form).
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Bare(s) | Value::Quoted(s) => Some(s),
            Value::Regex(_) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Value::Bare(s) | Value::Quoted(s) => s.is_empty(),
            Value::Regex(p) => p.source.is_empty(),
        }
    }
}

/// Operator keys, lowercased. Everything else is treated as free text.
///
/// `searc:` is deliberately absent. FreshRSS's pattern is `\bsearch?:`, where `h?` makes the `h`
/// optional — so `searc:foo` is accepted there. That is a copy-paste artifact of the neighbouring
/// `labels?` (where the optional `s` is intended), and honouring it would mean a typo silently
/// becomes a saved-search lookup.
const KEYS: &[&str] = &[
    "e", "f", "c", "s", "search", "l", "label", "labels", "intitle", "intext", "author", "inurl",
    "date", "pubdate", "mdate", "userdate",
];

/// Longest query length accepted, in bytes.
pub const MAX_QUERY_LENGTH: usize = 16_384;

/// Splits a query into tokens.
///
/// # Errors
///
/// [`SearchError::TooLong`] past [`MAX_QUERY_LENGTH`], [`SearchError::BadRegex`] for a malformed
/// `/…/` value, and [`SearchError::UnexpectedEnd`] for an unterminated quote. Unbalanced parentheses
/// are *not* an error here — the parser reports those, because only it knows whether a `)` had a
/// matching `(`.
pub fn tokenize(input: &str) -> Result<Vec<Token>, SearchError> {
    if input.len() > MAX_QUERY_LENGTH {
        return Err(SearchError::TooLong {
            len: input.len(),
            max: MAX_QUERY_LENGTH,
        });
    }

    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' => {
                out.push(Token::LParen);
                i += 1;
            }
            b')' => {
                out.push(Token::RParen);
                i += 1;
            }
            b'!' | b'-' => {
                // Always a negator. A hyphen *inside* a word never reaches here: `word_end` and
                // `bare_end` both treat `-` as an ordinary character, so `e-mail` and `2006-01-02`
                // are consumed whole by `read_word`. Getting here means the previous token already
                // ended at whitespace or a parenthesis, which is exactly term position.
                out.push(Token::Not);
                i += 1;
            }
            b'#' => {
                let (value, next) = read_value(input, i + 1, bytes)?;
                out.push(Token::Hash(value));
                i = next;
            }
            b'"' | b'\'' => {
                let (value, next) = read_quoted(input, i)?;
                out.push(Token::Free(value));
                i = next;
            }
            b'/' => {
                // A leading `/` may start a regex, or may just be a path. `read_regex` decides by
                // looking for a closing slash before the next whitespace.
                match read_regex(input, i)? {
                    Some((pattern, next)) => {
                        out.push(Token::Free(Value::Regex(pattern)));
                        i = next;
                    }
                    None => {
                        let end = bare_end(bytes, i);
                        out.push(Token::Free(Value::Bare(input[i..end].to_owned())));
                        i = end;
                    }
                }
            }
            _ => {
                let (token, next) = read_word(input, i, bytes)?;
                out.push(token);
                i = next;
            }
        }
    }

    Ok(out)
}

/// Reads a word, which may be `key:value`, `key:"quoted value"`, `key:/regex/`, `OR`, or bare text.
fn read_word(input: &str, start: usize, bytes: &[u8]) -> Result<(Token, usize), SearchError> {
    let end = word_end(bytes, start);
    let word = &input[start..end];

    // `OR` as a standalone word, case-insensitive.
    if word.eq_ignore_ascii_case("or") {
        return Ok((Token::Or, end));
    }

    // `key:` followed by one of the three value forms.
    if let Some(colon) = word.find(':') {
        let key = &word[..colon];
        if is_key(key) {
            let value_start = start + colon + 1;
            let (value, next) = read_value(input, value_start, bytes)?;
            return Ok((
                Token::Keyed {
                    key: key.to_ascii_lowercase(),
                    value,
                },
                next,
            ));
        }
    }

    // Not an operator: the whole run is free text. `read_value` would stop at a quote or a `/` that
    // is meaningful only in operator position, so take the word verbatim.
    Ok((Token::Free(Value::Bare(word.to_owned())), end))
}

/// True when `key` is one of the search operators.
///
/// Comparison is case-insensitive, which covers both of FreshRSS's spellings at once: `S:` is
/// written `[S]:` (upper only) while labels are `[lL]:` (either). No operator has a meaningful case
/// distinction, so folding is safe and saves the user a surprise.
fn is_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    KEYS.iter().any(|k| k.eq_ignore_ascii_case(key))
}

/// Finds the end of a bare word: whitespace, a parenthesis, or a quote.
fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' => break,
            // A quote ends the bare run so `intitle:"a b"` can be re-read as a quoted value.
            b'"' | b'\'' => break,
            _ => i += 1,
        }
    }
    // A word is never empty at this point: the caller only gets here on a non-delimiter byte.
    i.max(start + 1).min(bytes.len())
}

/// Reads one of the three value forms at `start`.
fn read_value(input: &str, start: usize, bytes: &[u8]) -> Result<(Value, usize), SearchError> {
    match bytes.get(start) {
        Some(b'"') | Some(b'\'') => {
            let (v, next) = read_quoted(input, start)?;
            Ok((v, next))
        }
        Some(b'/') => match read_regex(input, start)? {
            Some((p, next)) => Ok((Value::Regex(p), next)),
            None => {
                let end = bare_end(bytes, start);
                Ok((Value::Bare(input[start..end].to_owned()), end))
            }
        },
        _ => {
            let end = bare_end(bytes, start);
            Ok((Value::Bare(input[start..end].to_owned()), end))
        }
    }
}

/// Finds the end of a bare value: whitespace or a parenthesis, but *not* a colon or a hyphen.
///
/// Distinct from [`word_end`] because a value may legitimately contain characters that would
/// otherwise start a new token — `date:2006-01/2006-03` and `f:1,2` both do.
fn bare_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' | b'"' | b'\'' => break,
            _ => i += 1,
        }
    }
    i
}

/// Reads a single- or double-quoted value, honouring backslash escapes.
fn read_quoted(input: &str, start: usize) -> Result<(Value, usize), SearchError> {
    let quote = input.as_bytes()[start];
    let mut out = String::new();
    let mut escaped = false;
    let mut i = start + 1;

    for c in input[start + 1..].chars() {
        let len = c.len_utf8();
        if escaped {
            // Only the quote and the backslash are special; `\n` stays two characters, because a
            // user typing `\n` into a search box means the letters.
            if c == quote as char || c == '\\' {
                out.push(c);
            } else {
                out.push('\\');
                out.push(c);
            }
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote as char {
            return Ok((Value::Quoted(out), i + len));
        } else {
            out.push(c);
        }
        i += len;
    }

    Err(SearchError::UnexpectedEnd {
        token: (quote as char).to_string(),
    })
}

/// Reads a `/…/flags` regex value, or `None` when this is not a regex after all.
///
/// The closing delimiter is the first unescaped `/` *before any whitespace*. That whitespace rule is
/// what tells `/foo|bar/` from `/usr/local/bin`: without it, a path would be reported as an
/// unterminated regex and the query would fail to parse.
///
/// Whatever follows the closing slash up to the next delimiter is flags, and [`Pattern::parse`]
/// rejects an unknown one — so `/foo/z` is a parse error rather than a silently-ignored modifier.
fn read_regex(input: &str, start: usize) -> Result<Option<(Pattern, usize)>, SearchError> {
    let bytes = input.as_bytes();
    let mut end = None;
    let mut escaped = false;
    // Whitespace inside the pattern is allowed, so `/hello world/` is a regex. That is PHP's
    // behaviour too (`.` matches a space), and a literal space in a pattern is legitimate.
    for (i, &b) in bytes.iter().enumerate().skip(start + 1) {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            continue;
        }
        if b == b'/' {
            end = Some(i);
            break;
        }
        // A newline still ends it: a query is one line, and an unterminated `/` should not swallow
        // the rest of a pasted multi-line string.
        if b == b'\n' || b == b'\r' {
            break;
        }
    }
    let Some(end) = end else {
        return Ok(None);
    };

    // Flags are the maximal run of letters directly after the closing slash, and whatever follows
    // them must be a delimiter. That single rule is what separates `/foo/i` (a regex) from
    // `/usr/local/bin` (a path, whose would-be flags are `local` followed by `/`).
    let mut flag_end = end + 1;
    while flag_end < bytes.len() && bytes[flag_end].is_ascii_alphabetic() {
        flag_end += 1;
    }
    match bytes.get(flag_end) {
        None | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'(') | Some(b')') => {}
        Some(_) => return Ok(None),
    }

    let pattern = Pattern::parse(&input[start..flag_end])?;
    Ok(Some((pattern, flag_end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(s: &str) -> Token {
        Token::Free(Value::Bare(s.to_owned()))
    }

    fn keyed(key: &str, value: &str) -> Token {
        Token::Keyed {
            key: key.to_owned(),
            value: Value::Bare(value.to_owned()),
        }
    }

    #[test]
    fn a_single_word_is_free_text() {
        assert_eq!(tokenize("rust").unwrap(), vec![bare("rust")]);
    }

    #[test]
    fn whitespace_separates_words() {
        assert_eq!(
            tokenize("  rust   axum\t\n").unwrap(),
            vec![bare("rust"), bare("axum")]
        );
    }

    #[test]
    fn operators_are_recognized() {
        assert_eq!(
            tokenize("intitle:rust").unwrap(),
            vec![keyed("intitle", "rust")]
        );
        assert_eq!(tokenize("f:1,2").unwrap(), vec![keyed("f", "1,2")]);
        assert_eq!(tokenize("date:P1W").unwrap(), vec![keyed("date", "P1W")]);
        assert_eq!(tokenize("S:3").unwrap(), vec![keyed("s", "3")]);
        assert_eq!(
            tokenize("labels:todo").unwrap(),
            vec![keyed("labels", "todo")]
        );
        assert_eq!(
            tokenize("label:todo").unwrap(),
            vec![keyed("label", "todo")]
        );
        assert_eq!(
            tokenize("search:foo").unwrap(),
            vec![keyed("search", "foo")]
        );
    }

    #[test]
    fn every_documented_operator_is_a_key() {
        for key in [
            "e", "f", "c", "S", "search", "l", "L", "label", "labels", "intitle", "intext",
            "author", "inurl", "date", "pubdate", "mdate", "userdate",
        ] {
            let tokens = tokenize(&format!("{key}:x")).unwrap();
            assert!(
                matches!(&tokens[0], Token::Keyed { .. }),
                "{key}: should be an operator, got {tokens:?}"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_free_text() {
        // `foo:bar` is a legitimate search phrase, not a malformed operator.
        assert_eq!(tokenize("foo:bar").unwrap(), vec![bare("foo:bar")]);
        assert_eq!(tokenize("http://x").unwrap(), vec![bare("http://x")]);
        assert_eq!(tokenize("searc:foo").unwrap(), vec![bare("searc:foo")]);
    }

    #[test]
    fn a_key_with_no_value_is_still_a_key() {
        assert_eq!(tokenize("intitle:").unwrap(), vec![keyed("intitle", "")]);
        assert_eq!(tokenize("f:").unwrap(), vec![keyed("f", "")]);
    }

    #[test]
    fn a_hash_starts_a_tag() {
        assert_eq!(
            tokenize("#rust").unwrap(),
            vec![Token::Hash(Value::Bare("rust".into()))]
        );
        assert_eq!(
            tokenize("#a #b").unwrap(),
            vec![
                Token::Hash(Value::Bare("a".into())),
                Token::Hash(Value::Bare("b".into()))
            ]
        );
    }

    #[test]
    fn a_bare_hash_is_an_empty_tag() {
        assert_eq!(
            tokenize("#").unwrap(),
            vec![Token::Hash(Value::Bare("".into()))]
        );
        assert_eq!(
            tokenize("# x").unwrap(),
            vec![Token::Hash(Value::Bare("".into())), bare("x")]
        );
    }

    #[test]
    fn parentheses_are_their_own_tokens() {
        assert_eq!(
            tokenize("(a OR b)").unwrap(),
            vec![
                Token::LParen,
                bare("a"),
                Token::Or,
                bare("b"),
                Token::RParen
            ]
        );
        assert_eq!(
            tokenize("(a)OR(b)").unwrap(),
            vec![
                Token::LParen,
                bare("a"),
                Token::RParen,
                Token::Or,
                Token::LParen,
                bare("b"),
                Token::RParen
            ],
            "no whitespace needed around parentheses"
        );
    }

    #[test]
    fn or_is_recognized_case_insensitively_as_a_standalone_word() {
        for spelling in ["OR", "or", "Or", "oR"] {
            assert_eq!(
                tokenize(&format!("a {spelling} b")).unwrap(),
                vec![bare("a"), Token::Or, bare("b")],
                "{spelling}"
            );
        }
    }

    #[test]
    fn or_inside_a_word_is_not_an_operator() {
        assert_eq!(tokenize("orange").unwrap(), vec![bare("orange")]);
        assert_eq!(tokenize("for").unwrap(), vec![bare("for")]);
        assert_eq!(tokenize("a-OR-b").unwrap(), vec![bare("a-OR-b")]);
        assert_eq!(
            tokenize("intitle:OR").unwrap(),
            vec![keyed("intitle", "OR")]
        );
    }

    #[test]
    fn a_leading_bang_or_minus_negates() {
        assert_eq!(tokenize("!rust").unwrap(), vec![Token::Not, bare("rust")]);
        assert_eq!(tokenize("-rust").unwrap(), vec![Token::Not, bare("rust")]);
        assert_eq!(
            tokenize("-intitle:x").unwrap(),
            vec![Token::Not, keyed("intitle", "x")],
            "the negator must not be swallowed by the operator"
        );
        assert_eq!(
            tokenize("a -b").unwrap(),
            vec![bare("a"), Token::Not, bare("b")]
        );
        assert_eq!(
            tokenize("(-b)").unwrap(),
            vec![Token::LParen, Token::Not, bare("b"), Token::RParen]
        );
        assert_eq!(
            tokenize("a OR -b").unwrap(),
            vec![bare("a"), Token::Or, Token::Not, bare("b")]
        );
        assert_eq!(
            tokenize("! ! x").unwrap(),
            vec![Token::Not, Token::Not, bare("x")],
            "double negation is the parser's problem, not the lexer's"
        );
    }

    #[test]
    fn a_hyphen_inside_or_after_a_word_is_literal() {
        assert_eq!(tokenize("e-mail").unwrap(), vec![bare("e-mail")]);
        assert_eq!(tokenize("well-known").unwrap(), vec![bare("well-known")]);
        assert_eq!(tokenize("2006-01-02").unwrap(), vec![bare("2006-01-02")]);
        assert_eq!(
            tokenize("date:2006-01/2006-03").unwrap(),
            vec![keyed("date", "2006-01/2006-03")]
        );
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        assert_eq!(
            tokenize(r#"intitle:"hello world""#).unwrap(),
            vec![Token::Keyed {
                key: "intitle".into(),
                value: Value::Quoted("hello world".into()),
            }]
        );
        assert_eq!(
            tokenize(r#"'single quoted'"#).unwrap(),
            vec![Token::Free(Value::Quoted("single quoted".into()))]
        );
        assert_eq!(
            tokenize(r#"#"a b""#).unwrap(),
            vec![Token::Hash(Value::Quoted("a b".into()))]
        );
    }

    #[test]
    fn a_bare_quoted_string_is_free_text() {
        assert_eq!(
            tokenize(r#""quoted phrase""#).unwrap(),
            vec![Token::Free(Value::Quoted("quoted phrase".into()))]
        );
    }

    #[test]
    fn escapes_inside_quotes() {
        assert_eq!(
            tokenize(r#""a\"b""#).unwrap(),
            vec![Token::Free(Value::Quoted(r#"a"b"#.into()))]
        );
        assert_eq!(
            tokenize(r#""a\\b""#).unwrap(),
            vec![Token::Free(Value::Quoted(r"a\b".into()))]
        );
        assert_eq!(
            tokenize(r#""a\nb""#).unwrap(),
            vec![Token::Free(Value::Quoted(r"a\nb".into()))],
            r"\n is two characters, because that is what a user typing it means"
        );
        assert_eq!(
            tokenize(r#"'a\'b'"#).unwrap(),
            vec![Token::Free(Value::Quoted("a'b".into()))]
        );
    }

    #[test]
    fn an_unterminated_quote_is_an_error() {
        let err = tokenize(r#""unclosed"#).unwrap_err();
        assert!(matches!(err, SearchError::UnexpectedEnd { .. }), "{err}");
        assert!(tokenize(r#"'unclosed"#).is_err());
    }

    #[test]
    fn an_empty_quoted_string_is_allowed() {
        assert_eq!(
            tokenize(r#""""#).unwrap(),
            vec![Token::Free(Value::Quoted(String::new()))]
        );
    }

    #[test]
    fn a_regex_value_is_compiled_during_lexing() {
        let tokens = tokenize("intitle:/^ru.t$/i").unwrap();
        match &tokens[0] {
            Token::Keyed {
                key,
                value: Value::Regex(p),
            } => {
                assert_eq!(key, "intitle");
                assert_eq!(p.source, "^ru.t$");
                assert!(p.flags.ignore_case);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_standalone_regex_is_free_text() {
        let tokens = tokenize("/foo|bar/").unwrap();
        assert!(matches!(&tokens[0], Token::Free(Value::Regex(p)) if p.source == "foo|bar"));
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn a_regex_may_contain_a_space() {
        // Unlike a bare word, a regex is delimited by its slashes, so whitespace inside is literal.
        let tokens = tokenize(r#"/hello world/"#).unwrap();
        assert!(matches!(&tokens[0], Token::Free(Value::Regex(p)) if p.source == "hello world"));
    }

    #[test]
    fn a_bad_regex_is_a_parse_error_not_a_runtime_one() {
        let err = tokenize("intitle:/(/").unwrap_err();
        assert!(matches!(err, SearchError::BadRegex { .. }), "{err}");
    }

    #[test]
    fn an_unsupported_regex_flag_is_an_error() {
        assert!(matches!(
            tokenize("/foo/z").unwrap_err(),
            SearchError::BadRegex { .. }
        ));
    }

    #[test]
    fn something_that_looks_like_a_regex_but_is_not_stays_literal() {
        // No closing slash before whitespace, so this is a path.
        assert_eq!(
            tokenize("/usr/local/bin").unwrap(),
            vec![bare("/usr/local/bin")]
        );
        assert_eq!(
            tokenize("a / b").unwrap(),
            vec![bare("a"), bare("/"), bare("b")]
        );
    }

    #[test]
    fn an_escaped_slash_does_not_end_a_regex() {
        let tokens = tokenize(r"intitle:/a\/b/").unwrap();
        assert!(matches!(
            &tokens[0],
            Token::Keyed { value: Value::Regex(p), .. } if p.source == "a/b"
        ));
    }

    #[test]
    fn a_full_boolean_query_tokenizes() {
        let tokens =
            tokenize(r#"(intitle:rust OR #axum) AND !author:"bot account" f:1,2"#).unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::LParen,
                keyed("intitle", "rust"),
                Token::Or,
                Token::Hash(Value::Bare("axum".into())),
                Token::RParen,
                bare("AND"),
                Token::Not,
                Token::Keyed {
                    key: "author".into(),
                    value: Value::Quoted("bot account".into()),
                },
                keyed("f", "1,2"),
            ]
        );
    }

    #[test]
    fn and_is_not_an_operator_so_it_lands_in_free_text() {
        // FreshRSS has no `AND` keyword: juxtaposition *is* conjunction. Keeping `AND` as a word
        // means `black AND white` searches for the three words rather than failing.
        let tokens = tokenize("black AND white").unwrap();
        assert_eq!(tokens, vec![bare("black"), bare("AND"), bare("white")]);
        assert!(!tokens.contains(&Token::Or));
    }

    #[test]
    fn an_over_long_query_is_refused() {
        let long = "a".repeat(MAX_QUERY_LENGTH + 1);
        assert!(matches!(
            tokenize(&long).unwrap_err(),
            SearchError::TooLong { .. }
        ));
        assert!(tokenize(&"a".repeat(MAX_QUERY_LENGTH)).is_ok());
    }

    #[test]
    fn an_empty_query_has_no_tokens() {
        assert!(tokenize("").unwrap().is_empty());
        assert!(tokenize("   \n\t ").unwrap().is_empty());
    }

    #[test]
    fn unbalanced_parentheses_reach_the_parser_rather_than_failing_here() {
        // Only the parser knows whether a `)` had a matching `(`.
        assert_eq!(
            tokenize(")a(").unwrap(),
            vec![Token::RParen, bare("a"), Token::LParen]
        );
    }

    #[test]
    fn value_helpers() {
        assert_eq!(Value::Bare("x".into()).as_text(), Some("x"));
        assert_eq!(Value::Quoted("y".into()).as_text(), Some("y"));
        assert!(Value::Bare(String::new()).is_empty());
        assert!(!Value::Bare("x".into()).is_empty());
    }

    #[test]
    fn tokenizing_is_deterministic() {
        let q = r#"intitle:"a b" OR -#tag (f:1 date:P1W)"#;
        let a = tokenize(q).unwrap();
        let b = tokenize(q).unwrap();
        assert_eq!(a, b);
    }
}
