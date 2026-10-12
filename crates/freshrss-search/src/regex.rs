//! User-supplied regular expressions: `/pattern/im` with PHP delimiters.
//!
//! Two decisions here are load-bearing.
//!
//! **`fancy-regex`, not `regex`.** Saved searches in the wild use lookahead, lookbehind and
//! backreferences, and the `regex` crate supports none of them. Rejecting those patterns would break
//! queries that work in FreshRSS today. `fancy-regex` falls back to the `regex` engine for the
//! patterns that do not need backtracking, so the common case stays fast.
//!
//! **Compiled at parse time.** A malformed pattern must be a `400` from `POST /search/parse`, not a
//! runtime failure halfway through listing a page of articles. So [`Pattern::parse`] compiles eagerly
//! and discards the result; the stored form is the source text plus flags, which is cheap to clone,
//! serialize and compare, and is re-compiled through a thread-local cache on use.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::SearchError;

/// PHP's default backtrack limit, so a pathological pattern cannot hang a request.
const BACKTRACK_LIMIT: u32 = 1_000_000;

/// Builds the error every rejection path here shares, so the message always names the pattern the
/// user actually typed.
fn bad_regex(pattern: &str, reason: &str) -> SearchError {
    SearchError::BadRegex {
        pattern: pattern.to_owned(),
        reason: reason.to_owned(),
    }
}

/// How many compiled patterns one thread remembers.
///
/// Small on purpose: a reader re-runs the same handful of queries, and an unbounded cache would keep
/// every pattern any user ever typed alive for the life of the process.
const CACHE_SIZE: usize = 128;

thread_local! {
    static CACHE: RefCell<HashMap<CacheKey, fancy_regex::Regex>> =
        RefCell::new(HashMap::with_capacity(16));
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    source: String,
    flags: Flags,
}

/// The `i` and `m` modifiers PHP allows after the closing delimiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Flags {
    /// Case-insensitive.
    ///
    /// Load-bearing for regexes and only for regexes: a pattern is matched against the stored text
    /// rather than its folded form, so `/CAFÉ/` without this flag finds nothing in "café au lait".
    pub ignore_case: bool,
    /// `^` and `$` match at line boundaries rather than only at the ends of the string.
    pub multiline: bool,
}

/// A compiled, validated regular expression.
///
/// Clone is cheap: this holds the source text, not the compiled form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    /// The pattern body, without delimiters or flags.
    pub source: String,
    pub flags: Flags,
}

impl Pattern {
    /// Parses a PHP-style delimited pattern such as `/^foo$/i`.
    ///
    /// # Errors
    ///
    /// [`SearchError::BadRegex`] when the delimiters are missing or unbalanced, when an unknown flag
    /// follows the closing delimiter, or when the pattern does not compile. All three become a `400`.
    pub fn parse(text: &str) -> Result<Self, SearchError> {
        let bytes = text.as_bytes();
        let Some(&delim) = bytes.first() else {
            return Err(bad_regex(
                text,
                "a regular expression must start with a delimiter",
            ));
        };
        // A delimiter that cannot be escaped would make the pattern unparseable, and `/` is the only
        // one FreshRSS documents; accepting an alphanumeric one here would eat the first character of
        // a bare word.
        if delim.is_ascii_alphanumeric() || delim.is_ascii_whitespace() || delim == b'\\' {
            return Err(bad_regex(
                text,
                &format!("{delim:?} is not a usable regex delimiter"),
            ));
        }

        // Find the closing delimiter, honouring backslash escapes.
        let mut end = None;
        let mut escaped = false;
        for (i, &b) in bytes.iter().enumerate().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if b == b'\\' {
                escaped = true;
                continue;
            }
            if b == delim {
                end = Some(i);
                break;
            }
        }
        let Some(end) = end else {
            return Err(bad_regex(
                text,
                &format!("unterminated regular expression, expected a closing {delim:?}"),
            ));
        };

        let source = &text[1..end];
        let flag_text = &text[end + 1..];
        let flags = parse_flags(flag_text, text)?;

        let pattern = Pattern {
            source: unescape_delimiter(source, delim),
            flags,
        };
        // Compile now, discard the result: a pattern that does not build must fail the request rather
        // than every article it is later matched against.
        pattern.compile()?;
        Ok(pattern)
    }

    /// Builds a pattern from an already-delimiter-free source, with no flags.
    ///
    /// Used by the AST rewriters, which construct predicates programmatically rather than parsing
    /// user text.
    ///
    /// # Errors
    ///
    /// [`SearchError::BadRegex`] when the source does not compile.
    pub fn from_source(source: impl Into<String>) -> Result<Self, SearchError> {
        let pattern = Pattern {
            source: source.into(),
            flags: Flags::default(),
        };
        pattern.compile()?;
        Ok(pattern)
    }

    /// Compiles, using the thread-local cache.
    fn compile(&self) -> Result<fancy_regex::Regex, SearchError> {
        let key = CacheKey {
            source: self.source.clone(),
            flags: self.flags,
        };
        if let Some(re) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
            return Ok(re);
        }

        let mut builder = fancy_regex::RegexBuilder::new(&self.source);
        builder
            .case_insensitive(self.flags.ignore_case)
            .multi_line(self.flags.multiline)
            .backtrack_limit(BACKTRACK_LIMIT as usize);
        let re = builder
            .build()
            .map_err(|e| bad_regex(&self.render(), &e.to_string()))?;

        CACHE.with(|c| {
            let mut map = c.borrow_mut();
            if map.len() >= CACHE_SIZE {
                // Clearing rather than evicting one entry: the cache is tiny, a reader's working set
                // is smaller, and an LRU here would cost more than the recompile it saves.
                map.clear();
            }
            map.insert(key, re.clone());
        });
        Ok(re)
    }

    /// True when `haystack` matches.
    ///
    /// The haystack is the **stored** text, not its folded form: a pattern is matched against what
    /// the article actually says, and the user's own flags decide about case. See [`normalize::Text`]
    /// for why the containment operator differs.
    ///
    /// A pattern that exceeds the backtrack limit counts as **no match** rather than an error. A
    /// pathological regex typed by one user must not fail the listing for that page; reporting "no
    /// articles" is wrong but recoverable, and the error was already surfaced when the query was
    /// saved.
    pub fn is_match(&self, haystack: &str) -> bool {
        match self.compile() {
            Ok(re) => re.is_match(haystack).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// The pattern with its flags spelled inline, for an engine that takes one string and nothing
    /// else — which is all a SQLite UDF gets.
    ///
    /// [`Pattern::render`] is the PHP spelling used for display and round-tripping; this is the one
    /// that gets bound. Confusing the two binds `/foo/i` as a pattern that matches the literal text
    /// `/foo/i`.
    pub fn inline(&self) -> String {
        let mut out = String::with_capacity(self.source.len() + 8);
        if self.flags.ignore_case {
            out.push_str("(?i)");
        }
        if self.flags.multiline {
            out.push_str("(?m)");
        }
        out.push_str(&self.source);
        out
    }

    /// The PHP-style spelling, for round-tripping back to text.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.source.len() + 4);
        out.push('/');
        out.push_str(&self.source);
        out.push('/');
        if self.flags.ignore_case {
            out.push('i');
        }
        if self.flags.multiline {
            out.push('m');
        }
        out
    }
}

/// Reads the flag characters after the closing delimiter.
fn parse_flags(flag_text: &str, whole: &str) -> Result<Flags, SearchError> {
    let mut flags = Flags::default();
    for c in flag_text.chars() {
        match c {
            'i' | 'I' => flags.ignore_case = true,
            'm' | 'M' => flags.multiline = true,
            // PHP's `s` (dot matches newline) is fancy-regex's default, and `u` (UTF-8) is assumed,
            // so both are accepted and ignored rather than rejected.
            's' | 'S' | 'u' | 'U' | 'x' | 'X' => {}
            other => {
                return Err(bad_regex(
                    whole,
                    &format!("unsupported regex flag {other:?}"),
                ));
            }
        }
    }
    Ok(flags)
}

/// Removes the backslash that escaped the delimiter itself.
///
/// `/\//` is a pattern matching a single `/`; the escape exists only to keep the parser from ending
/// the pattern early, and `fancy-regex` would read `\/` as an unknown escape.
fn unescape_delimiter(source: &str, delim: u8) -> String {
    let needle = format!("\\{}", delim as char);
    if !source.contains(&needle) {
        return source.to_owned();
    }
    source.replace(&needle, &(delim as char).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_pattern() {
        let p = Pattern::parse("/foo/").unwrap();
        assert_eq!(p.source, "foo");
        assert_eq!(p.flags, Flags::default());
    }

    #[test]
    fn parses_flags() {
        let p = Pattern::parse("/foo/im").unwrap();
        assert!(p.flags.ignore_case && p.flags.multiline);

        let p = Pattern::parse("/foo/mi").unwrap();
        assert!(
            p.flags.ignore_case && p.flags.multiline,
            "flag order is free"
        );

        assert!(Pattern::parse("/foo/i").unwrap().flags.ignore_case);
        assert!(!Pattern::parse("/foo/i").unwrap().flags.multiline);
        assert!(Pattern::parse("/foo/I").unwrap().flags.ignore_case);
    }

    #[test]
    fn accepted_but_ignored_flags_do_not_error() {
        // `s` and `u` are already fancy-regex's behaviour; rejecting them would break saved queries
        // that carry them.
        for f in ["s", "u", "x", "su", "ims"] {
            assert!(Pattern::parse(&format!("/foo/{f}")).is_ok(), "flag {f:?}");
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        let err = Pattern::parse("/foo/z").unwrap_err();
        assert!(matches!(err, SearchError::BadRegex { .. }), "{err}");
        assert!(err.to_string().contains('z'), "{err}");
    }

    #[test]
    fn a_missing_or_unbalanced_delimiter_is_an_error() {
        for bad in ["", "foo/", "/foo", "'foo'", "/foo'", "//foo/"] {
            let result = Pattern::parse(bad);
            if bad == "//foo/" {
                // An empty pattern is legal; the trailing `foo/` is read as flags and rejected.
                assert!(result.is_err(), "{bad:?}");
            } else if bad == "'foo'" {
                assert!(result.is_ok(), "a quote is a valid delimiter: {bad:?}");
            } else {
                assert!(result.is_err(), "{bad:?} must be rejected");
            }
        }
    }

    #[test]
    fn an_alphanumeric_delimiter_is_rejected() {
        // Otherwise `afoo` would read `a` as a delimiter and `foo` as the pattern.
        assert!(Pattern::parse("afooa").is_err());
        assert!(Pattern::parse("1foo1").is_err());
    }

    #[test]
    fn an_escaped_delimiter_inside_the_pattern_is_honoured() {
        let p = Pattern::parse(r"/a\/b/").unwrap();
        assert_eq!(p.source, "a/b", "the escape existed only for the parser");
        assert!(p.is_match("xa/by"));
    }

    #[test]
    fn a_backslash_at_the_end_of_a_pattern_does_not_consume_the_delimiter() {
        // `\\` is an escaped backslash, so the following `/` really does close the pattern.
        let p = Pattern::parse(r"/a\\/").unwrap();
        assert_eq!(p.source, r"a\\");
    }

    #[test]
    fn a_non_compiling_pattern_fails_at_parse_time() {
        for bad in ["/(/", "/[/", "/*/"] {
            let err = Pattern::parse(bad).unwrap_err();
            assert!(
                matches!(err, SearchError::BadRegex { .. }),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn lookahead_and_backreferences_work_which_is_why_fancy_regex_is_here() {
        // The plain `regex` crate rejects both, and saved queries in the wild use both.
        let p = Pattern::parse("/foo(?=bar)/").unwrap();
        assert!(p.is_match("foobar"));
        assert!(!p.is_match("foobaz"));

        let p = Pattern::parse(r"/(a)b\1/").unwrap();
        assert!(p.is_match("aba"));
        assert!(!p.is_match("abb"));

        let p = Pattern::parse("/(?<=a)b/").unwrap();
        assert!(p.is_match("ab"));
        assert!(!p.is_match("cb"));
    }

    #[test]
    fn the_multiline_flag_changes_what_anchors_mean() {
        let single = Pattern::parse("/^b/").unwrap();
        let multi = Pattern::parse("/^b/m").unwrap();
        assert!(
            !single.is_match("a\nb"),
            "without m, ^ is the string start"
        );
        assert!(
            multi.is_match("a\nb"),
            "with m, ^ matches a line start"
        );
    }

    #[test]
    fn a_regex_sees_the_stored_text_not_its_folded_form() {
        // The containment operator folds both sides; a regex cannot, because folding the pattern
        // would lowercase its character classes. So an accented pattern needs `/i` for case and
        // spells its own accents.
        let plain = Pattern::parse("/École/").unwrap();
        assert!(plain.is_match("l'École des chiens"));
        assert!(!plain.is_match("l'école des chiens"));

        let insensitive = Pattern::parse("/École/i").unwrap();
        assert!(insensitive.is_match("l'école des chiens"));

        assert!(
            !Pattern::parse("/ecole/").unwrap().is_match("l'École"),
            "accents are not folded away for a regex the way they are for a plain needle"
        );
    }

    #[test]
    fn inline_spells_the_flags_out_for_a_one_string_engine() {
        assert_eq!(Pattern::parse("/foo/").unwrap().inline(), "foo");
        assert_eq!(Pattern::parse("/foo/i").unwrap().inline(), "(?i)foo");
        assert_eq!(Pattern::parse("/foo/m").unwrap().inline(), "(?m)foo");
        assert_eq!(Pattern::parse("/foo/im").unwrap().inline(), "(?i)(?m)foo");
        // The point of the method: `render()` is the PHP spelling and would be bound as a pattern
        // matching the literal text `/foo/i`.
        assert_eq!(Pattern::parse("/foo/i").unwrap().render(), "/foo/i");
    }

    #[test]
    fn render_round_trips() {
        for text in ["/foo/", "/foo/i", "/foo/m", "/foo/im"] {
            assert_eq!(Pattern::parse(text).unwrap().render(), text);
        }
    }

    #[test]
    fn render_uses_a_slash_whatever_delimiter_was_parsed() {
        // Canonical output is always `/…/`, so a saved query reads the same however it was typed.
        assert_eq!(Pattern::parse("'foo'").unwrap().render(), "/foo/");
    }

    #[test]
    fn from_source_compiles_eagerly() {
        assert!(Pattern::from_source("foo").is_ok());
        assert!(Pattern::from_source("(").is_err());
    }

    #[test]
    fn a_pattern_survives_a_json_round_trip() {
        // `POST /api/v1/search/parse` returns the AST, which contains patterns.
        let p = Pattern::parse("/foo/im").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Pattern>(&json).unwrap(), p);
        assert!(serde_json::from_str::<Pattern>(&json)
            .unwrap()
            .is_match("xFOOy"));
    }

    #[test]
    fn a_pathological_pattern_does_not_hang() {
        // Catastrophic backtracking hits the backtrack limit and reports no match rather than
        // burning the request's whole time budget.
        let p = Pattern::parse("/(a+)+b/").unwrap();
        let start = std::time::Instant::now();
        assert!(!p.is_match(&format!("{}c", "a".repeat(64))));
        assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    }

    #[test]
    fn the_cache_serves_repeated_compilations() {
        let p = Pattern::parse("/cached_pattern_xyz/").unwrap();
        for _ in 0..1000 {
            assert!(p.is_match("cached_pattern_xyz"));
        }
        // The cache is bounded; exceeding it clears rather than growing without limit.
        for i in 0..(CACHE_SIZE + 50) {
            let q = Pattern::from_source(format!("filler_{i}")).unwrap();
            assert!(q.is_match(&format!("filler_{i}")));
        }
        assert!(
            p.is_match("cached_pattern_xyz"),
            "still works after eviction"
        );
    }
}
