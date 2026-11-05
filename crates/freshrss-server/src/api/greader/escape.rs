//! `escapeToUnicodeAlternative`, ported from `lib/lib_rss.php`.
//!
//! A lossy, non-reversible substitution of punctuation with fullwidth Unicode forms, applied to
//! every title and name this API emits. It exists because Google Reader stream ids embed names
//! and the characters `& < > / , ;` would break either the id grammar or the XML some clients
//! still produce; FreshRSS chose replacement over escaping, and clients compare the strings they
//! get here, so byte parity is interop.

/// Decodes the entities our storage no longer carries but exported names may still contain, then
/// substitutes the punctuation. `extended` adds the eight characters used in stream-id positions
/// (feed and category names); item titles and authors get only the three XML-significant ones.
pub fn escape_to_unicode_alternative(text: &str, extended: bool) -> String {
    let decoded = html_entity_decode(text);
    let mut out = String::with_capacity(decoded.len());
    for ch in decoded.chars() {
        out.push(match ch {
            '&' => '＆',
            '<' => '＜',
            '>' => '＞',
            '\'' if extended => '’',
            '"' if extended => '＂',
            '^' if extended => '＾',
            '?' if extended => '？',
            '\\' if extended => '＼',
            '/' if extended => '／',
            ',' if extended => '，',
            ';' if extended => '；',
            other => other,
        });
    }
    out.trim().to_owned()
}

/// `htmlspecialchars_decode(…, ENT_QUOTES)`: the five entities PHP's `ENT_QUOTES` encoding produces.
///
/// Our own storage is clean UTF-8, so this only matters for names that arrived already encoded
/// from an OPML import or a feed that publishes entities in its title.
fn html_entity_decode(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// `htmlspecialchars_decode(…, ENT_QUOTES)` on its own, for URLs: links must come back as the bytes
/// a browser can open, not as the escaped form some feeds publish.
pub fn decode_url(text: &str) -> String {
    html_entity_decode(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_xml_characters_are_always_substituted() {
        assert_eq!(
            escape_to_unicode_alternative("a & b < c > d", false),
            "a ＆ b ＜ c ＞ d"
        );
    }

    #[test]
    fn the_extended_set_only_applies_where_stream_ids_allow_it() {
        assert_eq!(escape_to_unicode_alternative("a/b;c", false), "a/b;c");
        assert_eq!(escape_to_unicode_alternative("a/b;c", true), "a／b；c");
    }

    #[test]
    fn entities_are_decoded_before_substitution() {
        assert_eq!(escape_to_unicode_alternative("AT&amp;T", false), "AT＆T");
        assert_eq!(escape_to_unicode_alternative("&amp;", true), "＆");
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        assert_eq!(escape_to_unicode_alternative("  padded  ", false), "padded");
    }
}
