//! Text shaping: turning fragments of feed markup into displayable plain text.
//!
//! Three primitives here are direct ports, because getting them subtly wrong is visible:
//! [`strip_tags`] matches PHP's `strip_tags`, [`strcut`] matches `mb_strcut` (byte budget, never
//! splitting a character), and [`drop_trailing_fragment`] is the regex FreshRSS runs between
//! truncate-and-sanitize passes so a cut never leaves a dangling `<img src="…`.

use std::borrow::Cow;

/// Removes markup, leaving the text it contained.
///
/// Port of PHP `strip_tags`, which is what FreshRSS applies to titles, author names and category
/// labels. One deliberate difference: the *contents* of `<script>`, `<style>`, `<template>`,
/// `<title>` and `<textarea>` are dropped along with the tags. PHP keeps them, so
/// `strip_tags("<script>alert(1)</script>")` returns `alert(1)`; since every call site here wants
/// human-readable text, keeping it would put a JavaScript payload into a feed title.
///
/// Malformed input degrades rather than panics: an unterminated `<` swallows the rest of the
/// string, matching PHP.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    // Depth of a raw-text element we are skipping (`script`, `style`, …).
    let mut skip_until: Option<&str> = None;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            let text = &html[start..i];
            if skip_until.is_none() {
                out.push_str(text);
            }
            continue;
        }

        // Inside a raw-text element, only its own end tag matters.
        if let Some(name) = skip_until {
            if let Some(close_end) = match_close_tag(html, i, name) {
                i = close_end;
                skip_until = None;
            } else {
                // Unclosed `<script>`: everything after it is script body, as in HTML.
                return out;
            }
            continue;
        }

        // `<!-- comment -->`
        if html[i..].starts_with("<!--") {
            i = html[i..]
                .find("-->")
                .map(|p| i + p + 3)
                .unwrap_or(bytes.len());
            continue;
        }
        // `<![CDATA[ ... ]]>` — keep the contents, since in XML that *is* text.
        if html[i..].starts_with("<![CDATA[") {
            let body = i + "<![CDATA[".len();
            match html[body..].find("]]>") {
                Some(p) => {
                    out.push_str(&html[body..body + p]);
                    i = body + p + 3;
                }
                None => {
                    out.push_str(&html[body..]);
                    return out;
                }
            }
            continue;
        }
        // `<?pi?>`, `<!DOCTYPE …>`
        if html[i..].starts_with("<?") {
            i = html[i..]
                .find("?>")
                .map(|p| i + p + 2)
                .unwrap_or(bytes.len());
            continue;
        }
        if html[i..].starts_with("<!") {
            i = html[i..]
                .find('>')
                .map(|p| i + p + 1)
                .unwrap_or(bytes.len());
            continue;
        }

        // A real tag. Find its end, respecting quoted attribute values so `<a title=">">` works.
        let Some(tag_end) = find_tag_end(html, i) else {
            // PHP's strip_tags treats a stray `<` followed by a non-name character as text.
            out.push('<');
            i += 1;
            continue;
        };
        let tag = &html[i..tag_end];
        if let Some(name) = raw_text_element(tag) {
            skip_until = Some(name);
        }
        i = tag_end;
    }

    out
}

/// Finds the byte just past a tag's closing `>`, or `None` if `<` isn't followed by a name.
fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let after = bytes.get(start + 1).copied()?;
    if !(after.is_ascii_alphabetic() || after == b'/' || after == b'!') {
        return None;
    }
    let mut quote: Option<u8> = None;
    let mut i = start + 1;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'"') => quote = Some(b'"'),
            (None, b'\'') => quote = Some(b'\''),
            (Some(q), b) if b == q => quote = None,
            (None, b'>') => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// If `html[i..]` starts with `</name>` matching `name` case-insensitively, returns the index past it.
fn match_close_tag(html: &str, i: usize, name: &str) -> Option<usize> {
    let closing = html.get(i..)?.strip_prefix("</")?;
    let tag_name = closing
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    if !tag_name.eq_ignore_ascii_case(name) {
        return None;
    }
    Some(find_tag_end(html, i).unwrap_or(html.len()))
}

/// Returns the element name when a tag opens an HTML "raw text" element whose body is not markup.
fn raw_text_element(tag: &str) -> Option<&'static str> {
    if tag.starts_with("</") {
        return None;
    }
    let name_start = tag[1..]
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(tag.len() - 1);
    let name = &tag[1..1 + name_start];
    ["script", "style", "template", "title", "textarea"]
        .into_iter()
        .find(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Truncates to at most `max_bytes` without splitting a UTF-8 sequence.
///
/// Port of PHP `mb_strcut`, which is what FreshRSS uses for content-length caps: the budget is in
/// *bytes* (that is what a database column and an HTTP response care about) but the cut must land
/// on a character boundary.
pub fn strcut(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    // Step back over any continuation bytes (0b10xx_xxxx).
    while end > 0 && (text.as_bytes()[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    &text[..end]
}

/// Truncates to at most `max_chars` characters.
///
/// Port of `mb_substr($s, 0, $n, 'UTF-8')`. Distinct from [`strcut`]: FreshRSS uses this one for
/// `MAX_CHARS_EMPTY_FEED_TITLE`, where "75" means characters a reader will see.
pub fn substr_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

/// Removes a tag or entity that was cut in half at the end of a string.
///
/// Port of FreshRSS's `preg_replace('%(<[^>]{0,99}|&[^;]{0,32})$%', '', $s)`. Runs between each
/// truncate-and-sanitize attempt, so the sanitizer never sees `<img src="htt` and invents a
/// repaired tag that then overflows the budget again.
///
/// The 99/32 caps matter: a `<` far from the end is legitimate prose ("a < b"), and an `&` with no
/// `;` nearby is a bare ampersand. Without them, truncating a paragraph containing "x < y" would
/// delete everything after it.
pub fn drop_trailing_fragment(text: &str) -> &str {
    const MAX_TAG: usize = 99;
    const MAX_ENTITY: usize = 32;

    let bytes = text.as_bytes();
    let mut cut = text.len();

    // Incomplete tag: the last `<` with no `>` after it, within MAX_TAG bytes of the end.
    if let Some(rel) = bytes.iter().rposition(|&b| b == b'<') {
        if !bytes[rel..].contains(&b'>') && text.len() - rel <= MAX_TAG {
            cut = cut.min(rel);
        }
    }
    // Incomplete entity: the last `&` with no `;` after it, within MAX_ENTITY bytes of the end.
    if let Some(rel) = bytes.iter().rposition(|&b| b == b'&') {
        if !bytes[rel..].contains(&b';') && text.len() - rel <= MAX_ENTITY {
            cut = cut.min(rel);
        }
    }
    &text[..cut]
}

/// Collapses whitespace runs to single spaces and trims — XPath's `normalize-space()`.
///
/// Feeds frequently indent and newline their `<title>` content; without this, two parses of the
/// same document can produce titles that differ only in whitespace and therefore different hashes.
pub fn normalize_space(text: &str) -> Cow<'_, str> {
    let needs = text.chars().any(|c| c.is_whitespace() && c != ' ')
        || text.starts_with(' ')
        || text.ends_with(' ')
        || text.contains("  ");
    if !needs {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true; // suppresses leading whitespace
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    Cow::Owned(out)
}

/// Strips markup, decodes entities, collapses whitespace and caps the length: the display text
/// for a title or byline.
///
/// Decoding happens *before* the cap. FreshRSS truncates a body that has already been through
/// `html_only_entity_decode`, and the order matters: cutting first can slice an entity in half
/// (`Caf&eacute;` → `Caf&eacute`), which no longer decodes and so puts literal `&eacute` in front
/// of the reader. Named entities only ever shrink the string, so decoding first still lands the
/// cap exactly.
pub fn plain_text(html: &str, max_chars: usize) -> String {
    let stripped = strip_tags(html);
    let decoded = crate::entities::html_only_entity_decode(&stripped);
    let normalized = normalize_space(&decoded);
    substr_chars(&normalized, max_chars).to_owned()
}

/// Maps an empty-or-whitespace string to `None`, which is how feed fields are stored.
///
/// FreshRSS checks `=== ''` in some places and `empty()` in others; a feed with `<title> </title>`
/// must not become a title, or the fallback chain in `crate::guid` picks a different GUID.
pub fn trim_to_none(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_simple_markup() {
        assert_eq!(strip_tags("<p>Hello <b>world</b></p>"), "Hello world");
        assert_eq!(strip_tags("no markup at all"), "no markup at all");
    }

    #[test]
    fn keeps_cdata_contents_because_in_xml_that_is_text() {
        assert_eq!(strip_tags("<![CDATA[a < b]]>"), "a < b");
    }

    #[test]
    fn drops_comments_pis_and_doctypes() {
        assert_eq!(strip_tags("a<!-- c -->b"), "ab");
        assert_eq!(strip_tags("<?xml version='1.0'?>x"), "x");
        assert_eq!(strip_tags("<!DOCTYPE html>y"), "y");
    }

    #[test]
    fn drops_script_and_style_bodies_unlike_php() {
        assert_eq!(strip_tags("<script>alert(1)</script>safe"), "safe");
        assert_eq!(strip_tags("<style>.a{color:red}</style>text"), "text");
        // PHP's strip_tags would return "alert(1)"; a feed title must not contain it.
        assert_eq!(strip_tags("<SCRIPT>x()</SCRIPT>ok"), "ok");
    }

    #[test]
    fn an_unclosed_script_swallows_the_rest() {
        assert_eq!(strip_tags("before<script>never ends"), "before");
    }

    #[test]
    fn respects_quotes_inside_attributes() {
        assert_eq!(strip_tags(r#"<a title=">">link</a>"#), "link");
        assert_eq!(strip_tags(r#"<a title='>'>link</a>"#), "link");
    }

    #[test]
    fn a_stray_less_than_is_text() {
        assert_eq!(strip_tags("a < b"), "a < b");
        // PHP's strip_tags only treats `<` as a tag start when a letter, `/` or `!` follows.
        assert_eq!(strip_tags("5<6 and 7>8"), "5<6 and 7>8");
        assert_eq!(strip_tags("<3 the web"), "<3 the web");
    }

    #[test]
    fn self_closing_and_void_tags() {
        assert_eq!(strip_tags("a<br/>b<br>c"), "abc");
        assert_eq!(strip_tags("<img src='x'/>text"), "text");
    }

    #[test]
    fn strcut_never_splits_a_character() {
        let text = "café"; // 'é' is 2 bytes, total 5
        assert_eq!(strcut(text, 5), "café");
        assert_eq!(strcut(text, 4), "caf");
        assert_eq!(strcut(text, 100), "café");
        assert_eq!(strcut("日本語", 4), "日");
        assert_eq!(strcut("日本語", 3), "日");
        assert_eq!(strcut("abc", 0), "");
    }

    #[test]
    fn substr_chars_counts_characters_not_bytes() {
        assert_eq!(substr_chars("日本語です", 3), "日本語");
        assert_eq!(substr_chars("abc", 10), "abc");
        assert_eq!(substr_chars("café", 4), "café");
    }

    #[test]
    fn drop_trailing_fragment_removes_a_cut_tag() {
        assert_eq!(drop_trailing_fragment(r#"hello <img src="x"#), "hello ");
        assert_eq!(drop_trailing_fragment("hello <b"), "hello ");
        assert_eq!(
            drop_trailing_fragment("complete <b>tag</b>"),
            "complete <b>tag</b>"
        );
    }

    #[test]
    fn drop_trailing_fragment_only_removes_a_dangling_open_bracket() {
        // Matches FreshRSS's `%(<[^>]{0,99}|&[^;]{0,32})$%`: the pattern requires no `>` between
        // the `<` and the end. `<b>world` is not dangling — the sanitizer will auto-close it — so
        // it survives, and the exponential-shrink retry loop handles the overflow instead.
        assert_eq!(drop_trailing_fragment("hello <b>world"), "hello <b>world");
        assert_eq!(
            drop_trailing_fragment("<p>truncated mid-text"),
            "<p>truncated mid-text"
        );
    }

    #[test]
    fn drop_trailing_fragment_removes_a_cut_entity() {
        assert_eq!(drop_trailing_fragment("caf&eacute"), "caf");
        assert_eq!(drop_trailing_fragment("caf&eacute;"), "caf&eacute;");
        assert_eq!(drop_trailing_fragment("Tom & Jerry"), "Tom ");
    }

    #[test]
    fn drop_trailing_fragment_ignores_distant_prose() {
        // A `<` more than 99 bytes from the end is legitimate text, not a truncated tag.
        let prose = format!("a < b{}tail", "x".repeat(120));
        assert_eq!(drop_trailing_fragment(&prose), prose);
        // A `&` more than 32 bytes from the end is a bare ampersand.
        let amp = format!("Tom & Jerry{}tail", "x".repeat(40));
        assert_eq!(drop_trailing_fragment(&amp), amp);
    }

    #[test]
    fn normalize_space_matches_xpath() {
        assert_eq!(normalize_space("  a \n\t b  "), "a b");
        assert_eq!(normalize_space("already clean"), "already clean");
        assert!(matches!(normalize_space("clean"), Cow::Borrowed(_)));
        assert_eq!(normalize_space(""), "");
        assert_eq!(normalize_space("\n\n"), "");
    }

    #[test]
    fn plain_text_combines_the_three() {
        assert_eq!(
            plain_text("<p>  Hello\n <b>world</b>  </p>", 100),
            "Hello world"
        );
        assert_eq!(plain_text("<p>abcdefghij</p>", 5), "abcde");
        assert_eq!(plain_text("Caf&eacute;", 10), "Café");
    }

    #[test]
    fn the_character_cap_counts_decoded_characters() {
        // One entity is one character to the reader, so the cap applies after decoding.
        assert_eq!(plain_text("&eacute;&eacute;&eacute;", 2), "éé");
        assert_eq!(plain_text("&amp;&amp;&amp;", 4), "&amp");
        assert_eq!(plain_text("日本語です", 3), "日本語");
    }

    #[test]
    fn trim_to_none_distinguishes_blank_from_present() {
        assert_eq!(trim_to_none(""), None);
        assert_eq!(trim_to_none("   \n "), None);
        assert_eq!(trim_to_none("  x  ").as_deref(), Some("x"));
    }
}
