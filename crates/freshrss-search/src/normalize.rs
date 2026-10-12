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

/// Case-insensitive, accent-insensitive **literal substring** test — the `frss_ilike(subject, needle)`
/// semantics.
///
/// The needle is literal: `%` and `_` are *not* wildcards. Someone typing `100%` or `a_b` into a
/// search box means those characters, and the only way to express a pattern is a `/regex/`.
///
/// This is a deliberate break with FreshRSS, whose SQL path builds `LIKE "%$value%"` (so `a_b`
/// silently matches any three-character run) while its in-memory matcher does a plain substring test.
/// Honouring wildcards on one side and not the other is exactly the disagreement this module exists
/// to make impossible.
pub fn ilike(haystack: &str, needle: &str) -> bool {
    fold(haystack).contains(&fold(needle))
}

/// Case-insensitive, accent-insensitive **whole-value** comparison — the `frss_ieq(a, b)` semantics.
///
/// Label names use this rather than [`ilike`], because `labels:todo` must not match a label called
/// `todos`.
pub fn ieq(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

/// One text value, as both backends see it: the stored bytes and their accent-folded form.
///
/// Containment compares `folded`; a regex compares `raw`. Folding the haystack would make a pattern
/// the user wrote with an accent unmatchable, and folding the *pattern* is worse than useless — it
/// lowercases character classes, turning `[A-Z]` into `[a-z]`. [`crate::sql`] follows the same split
/// (`frss_ilike` folds inside, `frss_regexp` does not), which is what keeps the two backends in
/// agreement about *which* text they are looking at.
#[derive(Debug, Clone)]
pub struct Text<'a> {
    pub raw: &'a str,
    pub folded: String,
}

impl<'a> Text<'a> {
    /// Folds `raw` eagerly. Cheap enough to do unconditionally: the matcher folds each field once per
    /// article rather than once per predicate, and a query over a thousand articles with twenty
    /// predicates would otherwise normalize twenty thousand times.
    pub fn of(raw: &'a str) -> Self {
        Self {
            raw,
            folded: fold(raw),
        }
    }
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
        assert!(ilike("École Normale", "ecole"));
        assert!(ilike("ecole", "École"));
        assert!(!ilike("École", "lycee"));
    }

    #[test]
    fn ilike_treats_the_needle_as_a_literal() {
        // The two characters a user is most likely to type without meaning them as wildcards.
        assert!(ilike("100% sure", "100%"));
        assert!(!ilike("100 things", "100%"), "`%` is not a run-of-anything");
        assert!(ilike("a_b underscore", "a_b"));
        assert!(
            !ilike("axb", "a_b"),
            "`_` is not a single-character wildcard"
        );
    }

    #[test]
    fn ilike_is_a_substring_test_not_an_equality() {
        assert!(ilike("hello", "ell"));
        assert!(ilike("hello", "hello"));
        assert!(!ilike("hello", "hellp"));
        assert!(!ilike("hell", "hello"));
        assert!(
            ilike("anything", ""),
            "the empty needle is contained everywhere"
        );
    }

    #[test]
    fn ieq_compares_whole_values() {
        assert!(ieq("todo", "TODO"));
        assert!(ieq("Étiquette", "etiquette"));
        assert!(!ieq("todos", "todo"), "a label name is not a prefix search");
        assert!(!ieq("todo", "todos"));
        assert!(ieq("", ""));
    }

    #[test]
    fn text_holds_both_forms() {
        let t = Text::of("Café");
        assert_eq!(t.raw, "Café");
        assert_eq!(t.folded, "cafe");
    }
}
