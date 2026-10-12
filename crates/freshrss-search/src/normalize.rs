//! The one text-normalization primitive shared by the SQL compiler and the in-memory matcher.
//!
//! FreshRSS emulates MySQL's `utf8mb4_unicode_ci` collation with
//! `DatabaseDAO::removeAccentsLower()`, which runs the ICU transform
//! `NFD; [:Nonspacing Mark:] Remove; NFC; Lower`. We reproduce that exactly, because
//! `frss_ilike` (the SQLite UDF) and [`crate::matcher`] must agree by construction —
//! FreshRSS itself does not, which is why `intitle:ecole` matches "École" for filter
//! actions but not for listing queries there.

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Accent-folds and lowercases `s`: NFD → drop nonspacing marks → NFC → lowercase.
///
/// ```
/// # use freshrss_search::normalize::fold;
/// assert_eq!(fold("Café"), "cafe");
/// assert_eq!(fold("Ångström"), "angstrom");
/// assert_eq!(fold("İstanbul"), "istanbul");
/// ```
pub fn fold(s: &str) -> String {
    let decomposed: String = s
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .nfc()
        .collect();
    decomposed.to_lowercase()
}

/// Case-insensitive, accent-insensitive substring test — the SQL `frss_ilike(col, ?)`
/// semantics, where `needle` arrives already wrapped in `%…%` by the caller.
///
/// The `%` and `_` wildcards of SQL `LIKE` are honoured so that one implementation serves
/// both the UDF and the matcher.
pub fn ilike(haystack: &str, needle: &str) -> bool {
    let h = fold(haystack);
    let n = fold(needle);
    like_match(&h, &n)
}

/// Wildcard match over already-folded strings: `%` = any run, `_` = any single char.
fn like_match(haystack: &str, pattern: &str) -> bool {
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();

    // Classic two-pointer backtracking wildcard match; iterative so that long article
    // bodies cannot blow the stack.
    let (mut hi, mut pi) = (0usize, 0usize);
    let (mut star, mut star_h) = (usize::MAX, 0usize);

    while hi < h.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == h[hi]) {
            hi += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = pi;
            star_h = hi;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            star_h += 1;
            hi = star_h;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_accents_and_case() {
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("CAFÉ"), "cafe");
        assert_eq!(fold("École"), "ecole");
        assert_eq!(fold("Ångström"), "angstrom");
        assert_eq!(fold("naïve"), "naive");
        assert_eq!(fold("ÅÄÖ"), "aao");
    }

    #[test]
    fn folds_turkish_dotted_capital_i() {
        // ICU's NFD splits İ into I + U+0307; the mark is dropped before lowering.
        assert_eq!(fold("İstanbul"), "istanbul");
    }

    #[test]
    fn leaves_plain_ascii_and_symbols_alone() {
        assert_eq!(fold("Hello, World! 123"), "hello, world! 123");
        assert_eq!(fold(""), "");
    }

    #[test]
    fn eszett_is_not_expanded() {
        // The ICU path FreshRSS uses does not map ß → ss (only its Latin fallback table
        // does), and to_lowercase leaves ß intact. Matching that keeps SQL and the
        // in-memory matcher identical.
        assert_eq!(fold("Straße"), "straße");
    }

    #[test]
    fn ilike_ignores_accents_and_case_in_both_directions() {
        assert!(ilike("École Normale", "%ecole%"));
        assert!(ilike("ecole", "%École%"));
        assert!(!ilike("École", "%lycee%"));
    }

    #[test]
    fn ilike_honours_sql_wildcards() {
        assert!(ilike("hello", "%ll%"));
        assert!(ilike("hello", "h_llo"));
        assert!(ilike("hello", "h__lo"));
        assert!(ilike("hello", "%"));
        assert!(ilike("hello", "hello"));
        assert!(!ilike("hello", "hell"));
        assert!(!ilike("hello", "hello_"));
        assert!(!ilike("hello", "hx%"));
    }

    #[test]
    fn ilike_handles_multibyte_single_char_wildcard() {
        // `_` must consume one *character*, not one byte.
        assert!(ilike("café", "caf_"));
        assert!(!ilike("café", "café_"));
    }

    #[test]
    fn star_backtracking_is_correct() {
        assert!(ilike("aXbXc", "a%b%c"));
        assert!(ilike("abc", "a%"));
        assert!(ilike("abc", "%c"));
        assert!(!ilike("abc", "a%c%d"));
    }

    #[test]
    fn long_input_does_not_recurse() {
        let hay = "x".repeat(200_000);
        assert!(ilike(&hay, "%x"));
        assert!(!ilike(&hay, "%y"));
    }
}
