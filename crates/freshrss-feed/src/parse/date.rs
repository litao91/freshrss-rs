//! Date parsing for syndication formats.
//!
//! SimplePie accepts a very wide range of date spellings and FreshRSS inherits that tolerance, so
//! this module has to be permissive rather than strict: a feed whose `<pubDate>` we reject is a feed
//! whose articles all get the same synthesized date, which collapses their ordering in the list and
//! changes the GUID of any `sha1:*published*` policy.
//!
//! The strategy is a chain of increasingly loose attempts, each one exact about what it accepts.
//! Only after all of them fail does a caller fall back to "no date".

use std::str::FromStr as _;

use jiff::fmt::rfc2822;
use jiff::{civil, Timestamp};

/// Parses a feed date into unix seconds.
///
/// Returns `None` only when the string genuinely carries no date. Callers must not substitute "now"
/// here: a missing date and a present one produce different GUIDs under the `sha1:*published*`
/// policies, and substituting would make them change on every refresh.
pub fn parse_date(raw: &str) -> Option<i64> {
    parse_millis(raw).map(|ms| ms.div_euclid(1000))
}

/// Like [`parse_date`] but keeps sub-second precision.
///
/// JSON Feed's `date_published` and `RFC3339_EXTENDED` both carry milliseconds, and `itemTimeFormat`
/// can name `%f` explicitly, so the scraper path needs the finer value.
pub fn parse_millis(raw: &str) -> Option<i64> {
    let text = normalize(raw);
    if text.is_empty() {
        return None;
    }

    if let Some(ms) = bare_epoch_millis(&text) {
        return Some(ms);
    }
    if let Some(ts) = parse_rfc2822(&text).map(|z| z.timestamp()) {
        return Some(ts.as_millisecond());
    }
    if let Ok(ts) = Timestamp::from_str(&text) {
        return Some(ts.as_millisecond());
    }
    if let Some(ms) = civil_datetime_utc(&text) {
        return Some(ms);
    }
    if let Some(ms) = civil_date_only(&text) {
        return Some(ms);
    }
    for pattern in LENIENT_PATTERNS {
        if let Some(ms) = strtime_millis(pattern, &text) {
            return Some(ms);
        }
    }
    partial_iso(&text)
}

/// Spellings that none of the strict parsers accept but that appear in real feeds.
///
/// `%d %b %Y …` is RSS 2.0's date with the weekday missing, which is the single most common
/// malformation; `%b %d, %Y` is the American long form scrapers run into.
const LENIENT_PATTERNS: [&str; 8] = [
    "%d %b %Y %H:%M:%S %z",
    "%d %b %Y %H:%M:%S %Z",
    "%d %b %Y %H:%M:%S",
    "%d %b %Y",
    "%b %d, %Y %H:%M:%S %z",
    "%b %d, %Y",
    "%Y/%m/%d %H:%M:%S",
    "%Y/%m/%d",
];

fn strtime_millis(pattern: &str, text: &str) -> Option<i64> {
    let parsed = jiff::fmt::strtime::parse(pattern, text).ok()?;
    datetime_to_millis(&parsed)
}

fn datetime_to_millis(parsed: &jiff::fmt::strtime::BrokenDownTime) -> Option<i64> {
    let utc = jiff::tz::TimeZone::UTC;
    if let Ok(ts) = parsed.to_timestamp() {
        return Some(ts.as_millisecond());
    }
    // A pattern with no offset yields a naive datetime; reading it as UTC is deliberate — see
    // `civil_datetime_utc`.
    if let Some(ms) = parsed
        .to_datetime()
        .ok()
        .and_then(|dt| dt.to_zoned(utc.clone()).ok())
        .map(|z| z.timestamp().as_millisecond())
    {
        return Some(ms);
    }
    parsed
        .to_date()
        .ok()
        .and_then(|d| d.to_zoned(utc).ok())
        .map(|z| z.timestamp().as_millisecond())
}

/// Collapses whitespace and drops a trailing RFC 2822 comment.
///
/// `Mon, 02 Jan 2006 15:04:05 +0000 (UTC)` is legal but rarely parsed, and feeds indent their date
/// elements freely, so both need handling before the strict parsers see the string.
fn normalize(raw: &str) -> String {
    let without_comment = match raw.find('(') {
        Some(at) if raw[at..].ends_with(')') => &raw[..at],
        _ => raw,
    };
    without_comment
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses an RFC 822/2822 date, tolerating a wrong or unrecognized weekday.
///
/// A strict parser rejects `Sat, 02 Jan 2206 15:04:05 +0000` because that day is a Thursday, and
/// rejects `Funday, 02 Jan 2006 …` because `Funday` is not an abbreviation. Both are common in real
/// feeds: the weekday is redundant information the generator computes separately from the date, and
/// plenty of them get it wrong. Losing every article's date over it means the whole feed sorts by
/// ingestion time and any `sha1:*published*` GUID policy silently changes.
///
/// The date, time and zone are what actually identify the instant, so on failure the leading
/// `weekday,` is dropped and the rest is parsed on its own.
fn parse_rfc2822(text: &str) -> Option<jiff::Zoned> {
    if let Ok(zoned) = rfc2822::parse(text) {
        return Some(zoned);
    }
    // Only a short alphabetic token followed by a comma qualifies as a weekday prefix.
    let comma = text.find(',')?;
    if comma > 16 {
        return None;
    }
    let prefix = &text[..comma];
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    rfc2822::parse(text[comma + 1..].trim_start()).ok()
}

/// A bare integer: a W3C year, unix seconds, or unix milliseconds.
///
/// The three cases are told apart by digit count, which is the only signal available:
/// - ≤ 4 digits is a **year**. `2006` is a valid W3C DTF date, and reading it as 2006 seconds
///   (1970-01-01T00:33:26Z) would put the article thirty-odd minutes after the epoch.
/// - 5–12 digits are **seconds**. A real feed timestamp is 9 or 10 digits for the foreseeable future.
/// - ≥ 13 digits are **milliseconds**, which is what JSON APIs emit.
fn bare_epoch_millis(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = text.parse().ok()?;
    match digits.len() {
        0..=4 => None, // A year: handled by `partial_iso` instead.
        5..=12 => Some(n * 1000),
        _ => Some(n),
    }
}

/// An ISO-8601 date-time with no offset, read as UTC.
///
/// Common in feeds that were generated by serializing a naive local datetime. Assuming UTC rather
/// than rejecting keeps such articles in a stable order; assuming the *server's* zone would make
/// the parsed value depend on where the reader runs.
fn civil_datetime_utc(text: &str) -> Option<i64> {
    let candidate = text.replace(' ', "T");
    let dt = civil::DateTime::from_str(&candidate).ok()?;
    Some(
        dt.to_zoned(jiff::tz::TimeZone::UTC)
            .ok()?
            .timestamp()
            .as_millisecond(),
    )
}

/// A bare `YYYY-MM-DD`.
fn civil_date_only(text: &str) -> Option<i64> {
    let date = civil::Date::from_str(text).ok()?;
    Some(
        date.to_zoned(jiff::tz::TimeZone::UTC)
            .ok()?
            .timestamp()
            .as_millisecond(),
    )
}

/// W3C DTF partial forms: `YYYY-MM` and `YYYY`.
///
/// Atom allows these. The floor of the period is the right reading — `2014-02` means "some time in
/// February 2014", and picking the first instant keeps `date:` range searches from excluding it.
///
/// The year must be exactly four digits, as the grammar requires. Without that, a bare `0` would
/// read as year zero and land an article in 1 BCE.
fn partial_iso(text: &str) -> Option<i64> {
    let mut parts = text.splitn(3, '-');
    let year_text = parts.next()?;
    if year_text.len() != 4 || !year_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i16 = year_text.parse().ok()?;
    match parts.next() {
        None => {
            let date = civil::Date::new(year, 1, 1).ok()?;
            Some(midnight_utc(date))
        }
        Some(month) => {
            let month: i8 = month.parse().ok()?;
            if parts.next().is_some() {
                return None;
            }
            let date = civil::Date::new(year, month, 1).ok()?;
            Some(midnight_utc(date))
        }
    }
}

fn midnight_utc(date: civil::Date) -> i64 {
    date.to_zoned(jiff::tz::TimeZone::UTC)
        .map(|z| z.timestamp().as_millisecond())
        .unwrap_or(0)
}

/// Formats unix seconds as RFC 3339 in UTC, for API output and JSON Feed re-serialization.
#[must_use]
pub fn to_rfc3339(unix_seconds: i64) -> String {
    let Ok(ts) = Timestamp::from_millisecond(unix_seconds * 1000) else {
        return String::new();
    };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::UTC);
    let formatted = zoned.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    // RFC 3339 allows either `Z` or `+00:00` for UTC; `Z` is what the JSON Feed and Atom specs
    // show in their examples and what every other feed emits.
    format!("{formatted}Z")
}

/// Formats unix seconds as an RFC 822 date, for OPML and RSS re-export.
#[must_use]
pub fn to_rfc822(unix_seconds: i64) -> String {
    Timestamp::from_millisecond(unix_seconds * 1000)
        .ok()
        .and_then(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%a, %d %b %Y %T %z")
                .to_string()
                .into()
        })
        .unwrap_or_default()
}

/// Translates a PHP `date()` format string into a `jiff::fmt::strtime` pattern.
///
/// The scraping kinds let a user supply `itemTimeFormat` in PHP's syntax, and JSON Feed's default is
/// `Y-m-d\TH:i:s.vP` (`DateTimeInterface::RFC3339_EXTENDED`), so this has to cover the letters that
/// actually appear in the wild rather than the whole of PHP's table.
///
/// The two syntaxes escape literals differently, which is the one real trap: PHP writes `\T` for a
/// literal `T`, but in `strtime` a bare character *is* the literal and `%T` is a directive (the
/// 24-hour clock). Translating `\T` to `%T` would silently parse `15:04:05` where the feed has `T`.
///
/// Returns `None` only when the format ends in a dangling backslash.
pub fn php_format_to_strtime(php: &str) -> Option<String> {
    let mut out = String::with_capacity(php.len() * 2);
    let mut chars = php.chars();

    while let Some(c) = chars.next() {
        match c {
            // PHP escape: the next character is a literal.
            '\\' => {
                let literal = chars.next()?;
                push_literal(&mut out, literal);
            }
            _ => match php_directive(c) {
                Some(directive) => out.push_str(directive),
                None => push_literal(&mut out, c),
            },
        }
    }
    Some(out)
}

/// Appends a literal character to a `strtime` pattern.
///
/// Only `%` needs escaping, since `strtime` reads every other non-directive byte literally.
fn push_literal(out: &mut String, c: char) {
    if c == '%' {
        out.push_str("%%");
    } else {
        out.push(c);
    }
}

/// One PHP format letter as a `strtime` directive, or `None` when it is a literal.
fn php_directive(c: char) -> Option<&'static str> {
    Some(match c {
        // Date
        'Y' => "%Y",
        'y' => "%y",
        'm' => "%m",
        'n' => "%_m",
        'd' => "%d",
        'j' => "%e",
        'M' => "%b",
        'F' => "%B",
        'D' => "%a",
        'l' => "%A",
        // Time
        'H' => "%H",
        'G' => "%_H",
        'h' => "%I",
        'g' => "%_I",
        'i' => "%M",
        's' => "%S",
        'v' => "%3f",
        'u' => "%6f",
        'a' => "%P",
        'A' => "%p",
        // Zone
        'e' | 'T' => "%Z",
        'P' => "%:z",
        'O' => "%z",
        // Unix timestamp
        'U' => "%s",
        // The remaining PHP letters (`S w N z W t L o B`) have no `strtime` equivalent and become
        // literals, which then fail to match — see [`parse_with_php_format`] for what happens next.
        _ => return None,
    })
}

/// Parses a timestamp scraped from a page or a JSON document.
///
/// With no format the value goes through the general parser, which handles RFC 822, RFC 3339, bare
/// unix seconds and a dozen more spellings. With a format, that format decides — and a format that
/// does not fit falls back to the general parser, because `itemTimeFormat` is free text that users
/// routinely get wrong and losing every date over a typo is worse than guessing.
///
/// `U` goes to the epoch parser rather than to a strptime pattern. PHP special-cases it — "if the
/// format is `U` and the value is longer than ten characters, drop the last three" — which is a
/// millisecond workaround; [`parse_millis`] already tells seconds from milliseconds, so the special
/// case is unnecessary here.
pub fn scraped_timestamp(raw: &str, php_format: &str) -> Option<i64> {
    match php_format {
        "" => parse_date(raw),
        "U" => parse_millis(raw).map(seconds_from_millis),
        format => parse_with_php_format(raw, format).map(seconds_from_millis),
    }
}

fn seconds_from_millis(millis: i64) -> i64 {
    millis.div_euclid(1000)
}

/// Parses a date using a PHP `date()` format string, as the scraping kinds require.
///
/// When the pattern does not match, this falls back to the general parser rather than returning
/// nothing. That is FreshRSS's own behaviour — `DateTime::createFromFormat` failing leaves the raw
/// string in place, which SimplePie then parses loosely — and it matters because `itemTimeFormat` is
/// a free-text field users routinely get wrong.
pub fn parse_with_php_format(raw: &str, php_format: &str) -> Option<i64> {
    let text = raw.trim();
    let pattern = php_format_to_strtime(php_format)?;
    if let Ok(parsed) = jiff::fmt::strtime::parse(&pattern, text) {
        if let Some(ms) = datetime_to_millis(&parsed) {
            return Some(ms);
        }
    }
    parse_millis(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2006-01-02T15:04:05Z, the reference date used throughout.
    const REF: i64 = 1_136_214_245;

    #[test]
    fn rfc822_with_numeric_offset() {
        assert_eq!(parse_date("Mon, 02 Jan 2006 15:04:05 +0000"), Some(REF));
        assert_eq!(parse_date("Mon, 02 Jan 2006 10:04:05 -0500"), Some(REF));
        assert_eq!(parse_date("Mon, 02 Jan 2006 20:34:05 +0530"), Some(REF));
    }

    #[test]
    fn rfc822_with_named_zones() {
        assert_eq!(parse_date("Mon, 02 Jan 2006 15:04:05 GMT"), Some(REF));
        assert_eq!(parse_date("Mon, 02 Jan 2006 15:04:05 UT"), Some(REF));
        assert_eq!(parse_date("Mon, 02 Jan 2006 15:04:05 UTC"), Some(REF));
    }

    #[test]
    fn rfc822_tolerates_a_missing_or_single_digit_weekday_and_day() {
        assert_eq!(parse_date("02 Jan 2006 15:04:05 GMT"), Some(REF));
        assert_eq!(parse_date("Mon, 2 Jan 2006 15:04:05 GMT"), Some(REF));
        assert_eq!(parse_date("2 Jan 2006 15:04:05 +0000"), Some(REF));
    }

    #[test]
    fn rfc822_tolerates_a_trailing_comment() {
        assert_eq!(
            parse_date("Mon, 02 Jan 2006 15:04:05 +0000 (UTC)"),
            Some(REF)
        );
    }

    #[test]
    fn a_wrong_weekday_does_not_cost_us_the_date() {
        // 2006-01-02 was a Monday. Generators that compute the weekday separately get this wrong,
        // and a strict parser then rejects the whole timestamp.
        assert_eq!(parse_date("Tue, 02 Jan 2006 15:04:05 +0000"), Some(REF));
        assert_eq!(parse_date("Sat, 02 Jan 2006 15:04:05 +0000"), Some(REF));
        assert_eq!(
            parse_date("Sun, 02 Jan 2206 15:04:05 +0000"),
            Some(7_447_561_445)
        );
    }

    #[test]
    fn an_unrecognized_weekday_does_not_cost_us_the_date() {
        assert_eq!(parse_date("Funday, 02 Jan 2006 15:04:05 +0000"), Some(REF));
        assert_eq!(parse_date("Monday, 02 Jan 2006 15:04:05 +0000"), Some(REF));
    }

    #[test]
    fn the_lenient_weekday_path_does_not_accept_garbage() {
        // The prefix must be a short alphabetic token before a comma, so a comma inside prose does
        // not turn the rest of the string into a date.
        assert_eq!(parse_date("hello, world"), None);
        assert_eq!(parse_date("Mon, not a date at all"), None);
        assert_eq!(
            parse_date("a-very-long-non-weekday-prefix, 02 Jan 2006"),
            None
        );
        // A genuinely impossible calendar date is still rejected.
        assert_eq!(parse_date("Mon, 32 Jan 2006 15:04:05 +0000"), None);
    }

    #[test]
    fn rfc822_tolerates_surrounding_and_inner_whitespace() {
        assert_eq!(
            parse_date("  Mon, 02 Jan 2006 15:04:05 +0000  \n"),
            Some(REF)
        );
        assert_eq!(
            parse_date("Mon,  02  Jan  2006  15:04:05  +0000"),
            Some(REF)
        );
    }

    #[test]
    fn rfc3339_forms() {
        assert_eq!(parse_date("2006-01-02T15:04:05Z"), Some(REF));
        assert_eq!(parse_date("2006-01-02T15:04:05+00:00"), Some(REF));
        assert_eq!(parse_date("2006-01-02T10:04:05-05:00"), Some(REF));
        assert_eq!(
            parse_date("2006-01-02t15:04:05z"),
            Some(REF),
            "case-insensitive"
        );
    }

    #[test]
    fn fractional_seconds_are_truncated_for_parse_date_but_kept_for_parse_millis() {
        assert_eq!(parse_date("2006-01-02T15:04:05.123Z"), Some(REF));
        assert_eq!(
            parse_millis("2006-01-02T15:04:05.123Z"),
            Some(REF * 1000 + 123)
        );
        // RFC3339_EXTENDED, which is JSON Feed's and FreshRSS's default `itemTimeFormat`.
        assert_eq!(
            parse_millis("2006-01-02T15:04:05.000+00:00"),
            Some(REF * 1000)
        );
    }

    #[test]
    fn a_space_instead_of_t_is_accepted() {
        assert_eq!(parse_date("2006-01-02 15:04:05"), Some(REF));
        assert_eq!(parse_date("2006-01-02 15:04:05Z"), Some(REF));
    }

    #[test]
    fn a_datetime_with_no_zone_is_read_as_utc() {
        // Not the server's local zone: the same feed must parse to the same instant everywhere.
        assert_eq!(parse_date("2006-01-02T15:04:05"), Some(REF));
    }

    #[test]
    fn date_only_forms() {
        assert_eq!(parse_date("2006-01-02"), Some(1_136_160_000));
        assert_eq!(parse_date("02 Jan 2006"), Some(1_136_160_000));
        assert_eq!(parse_date("2 Jan 2006"), Some(1_136_160_000));
    }

    #[test]
    fn w3c_partial_forms_floor_to_the_start_of_the_period() {
        assert_eq!(parse_date("2006-01"), Some(1_136_073_600));
        assert_eq!(parse_date("2006"), Some(1_136_073_600));
    }

    #[test]
    fn bare_unix_timestamps() {
        assert_eq!(parse_date("1136214245"), Some(REF));
        // A one-to-four digit number is a year, not a timestamp, and `0` is not a valid W3C year.
        assert_eq!(parse_date("0"), None);
        assert_eq!(parse_date("86400"), Some(86_400));
        // Epoch milliseconds are recognized rather than read as the year 55 000.
        assert_eq!(parse_date("1136214245000"), Some(REF));
        assert_eq!(parse_millis("1136214245123"), Some(REF * 1000 + 123));
    }

    #[test]
    fn unparseable_input_is_none_not_zero() {
        for bad in [
            "",
            "   ",
            "not a date",
            "yesterday",
            "Mon, 32 Jan 2006 15:04:05 GMT",
            "2006-13-02",
            "<![CDATA[]]>",
            "null",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?} must not become a date");
        }
    }

    #[test]
    fn dates_before_the_epoch_are_negative_and_still_parse() {
        assert_eq!(parse_date("1960-01-02T15:04:05Z"), Some(-315_478_555));
        assert_eq!(parse_date("Thu, 01 Jan 1970 00:00:00 +0000"), Some(0));
    }

    #[test]
    fn future_dates_are_kept_as_given() {
        // FreshRSS does not clamp them; ordering looks odd but the article is not silently dropped,
        // and clamping would change the GUID of a `sha1:*published*` feed on the next refresh.
        let future = parse_date("2206-01-02T15:04:05Z");
        assert!(future.is_some_and(|t| t > REF));
    }

    #[test]
    fn a_leap_second_does_not_panic_or_produce_nonsense() {
        // RFC 3339 allows `:60`, and jiff accepts it. Either reading is fine as long as the result
        // is within a second of the surrounding minute.
        let parsed = parse_date("2016-12-31T23:59:60Z");
        assert!(
            parsed.is_none_or(|t| (t - 1_483_228_800).abs() <= 1),
            "{parsed:?}"
        );
    }

    #[test]
    fn a_four_digit_number_is_a_year_not_a_timestamp() {
        assert_eq!(parse_date("2006"), Some(1_136_073_600));
        assert_eq!(parse_date("1999"), Some(915_148_800));
    }

    // --- formatting --------------------------------------------------------

    #[test]
    fn rfc3339_output_round_trips() {
        assert_eq!(to_rfc3339(REF), "2006-01-02T15:04:05Z");
        assert_eq!(parse_date(&to_rfc3339(REF)), Some(REF));
        assert_eq!(to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rfc822_output_round_trips() {
        assert_eq!(to_rfc822(REF), "Mon, 02 Jan 2006 15:04:05 +0000");
        assert_eq!(parse_date(&to_rfc822(REF)), Some(REF));
    }

    // --- PHP format strings ------------------------------------------------

    #[test]
    fn the_rfc3339_extended_default_translates() {
        // DateTimeInterface::RFC3339_EXTENDED, which is what FreshRSS uses for JSON Feed.
        assert_eq!(
            php_format_to_strtime("Y-m-d\\TH:i:s.vP").as_deref(),
            Some("%Y-%m-%dT%H:%M:%S.%3f%:z")
        );
        assert_eq!(
            parse_with_php_format("2006-01-02T15:04:05.123+00:00", "Y-m-d\\TH:i:s.vP"),
            Some(REF * 1000 + 123)
        );
    }

    #[test]
    fn common_php_formats_parse() {
        let cases = [
            ("Y-m-d H:i:s", "2006-01-02 15:04:05", REF * 1000),
            ("d/m/Y H:i", "02/01/2006 15:04", REF * 1000 - 5_000),
            ("U", "1136214245", REF * 1000),
            ("Y-m-d", "2006-01-02", 1_136_160_000_000),
            (
                "D, d M Y H:i:s O",
                "Mon, 02 Jan 2006 15:04:05 +0000",
                REF * 1000,
            ),
        ];
        for (php, raw, want) in cases {
            assert_eq!(parse_with_php_format(raw, php), Some(want), "{php} / {raw}");
        }
    }

    #[test]
    fn an_untranslatable_format_falls_back_to_the_general_parser() {
        // `%` has no PHP meaning as a directive here, and unknown letters stay literal, so the
        // pattern may simply fail to match — the input is then parsed best-effort instead.
        assert_eq!(
            parse_with_php_format("2006-01-02T15:04:05Z", "QQQ"),
            Some(REF * 1000)
        );
    }

    #[test]
    fn a_percent_sign_in_the_format_is_escaped() {
        assert_eq!(php_format_to_strtime("Y%").as_deref(), Some("%Y%%"));
    }

    #[test]
    fn literals_survive_translation() {
        assert_eq!(php_format_to_strtime("Y/m/d").as_deref(), Some("%Y/%m/%d"));
        assert_eq!(
            parse_with_php_format("2006/01/02", "Y/m/d"),
            Some(1_136_160_000_000)
        );
    }
}
