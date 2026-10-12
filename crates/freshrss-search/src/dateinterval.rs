//! ISO 8601 intervals for the `date:`-family operators.
//!
//! Port of `lib/lib_date.php::parseDateInterval`, including its two non-obvious behaviours:
//!
//! - **Prefix borrowing.** In `2014-02/04` the second operand is not a full date; it *replaces the
//!   tail* of the first. FreshRSS does this by chopping `strlen($d2)` characters off the end of the
//!   normalized `$d1` and appending `$d2`, so `201402` + `04` yields `201404`. It looks like string
//!   surgery because it is, but it is also the only rule that makes `2014-02/04`, `2014/06` and
//!   `2014-02-01/15` all mean what a reader expects.
//! - **Floor and ceiling rounding.** A partial date is a *period*, so its lower bound is the first
//!   instant and its upper bound the last. `2014` spans the whole year and `2014-02` the whole
//!   month, with `T235959` (or the month's real last day) rather than midnight — otherwise
//!   `date:2014-02` would match only February 1st.
//!
//! Durations are relative to **now**, or to the other endpoint when the query gives one, so `P1W` is
//! "the last week" and `2014-01-01/P1M` is "January 2014".

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp, Zoned};

use crate::ast::DateRange;
use crate::error::SearchError;

/// Parses an interval into an inclusive range of unix seconds.
///
/// `now` is injected rather than read from the clock so a query is reproducible within one request
/// and testable at all.
///
/// # Errors
///
/// [`SearchError::BadDateInterval`] when the text is not a recognizable date, period or duration.
pub fn parse(text: &str, now: i64) -> Result<DateRange, SearchError> {
    parse_inner(text, now).map_err(|e| match e {
        // Internal failures name the normalized, padded operand (`"BANANA01T000000"`), which is not
        // what the user typed and not actionable. Re-attach the original text.
        SearchError::BadDateInterval { reason, .. } => SearchError::BadDateInterval {
            value: text.trim().to_owned(),
            reason,
        },
        other => other,
    })
}

fn parse_inner(text: &str, now: i64) -> Result<DateRange, SearchError> {
    let normalized = normalize(text);
    if normalized.is_empty() {
        return Ok(DateRange::default());
    }

    let (raw1, raw2) = match normalized.split_once('/') {
        Some((a, b)) => (a.to_owned(), Some(b.to_owned())),
        None => (normalized.clone(), None),
    };

    let d1 = no_delimit(&raw1);
    let d2 = date_relative(d1.as_deref(), raw2.as_deref());

    let mut min: Option<i64> = None;
    let mut max: Option<i64> = None;

    if let Some(d1) = &d1 {
        if !d1.starts_with('P') {
            min = Some(instant(&date_floor(d1))?);
        }
    }

    if let Some(d2) = &d2 {
        if d2.starts_with('P') {
            let base = min.unwrap_or(now);
            max = Some(add_duration(base, d2, now)?.saturating_sub(1));
        } else if d1.as_deref().is_none_or(|d| !d.starts_with('P')) {
            max = Some(instant(&date_ceiling(d2))?);
        } else {
            max = Some(instant(d2)?);
        }
    }

    if let Some(d1) = &d1 {
        if d1.starts_with('P') {
            let base = max.unwrap_or(now);
            min = Some(sub_duration(base, d1, now)?.saturating_add(1));
        }
    }

    if min.is_none() && max.is_none() {
        return Err(bad(text, "no date or duration could be read"));
    }
    Ok(DateRange { min, max })
}

fn bad(value: &str, reason: &str) -> SearchError {
    SearchError::BadDateInterval {
        value: value.to_owned(),
        reason: reason.to_owned(),
    }
}

/// `str_replace(['--', ' '], ['/', 'T'], $s)` then `strtoupper`.
///
/// `--` is the ISO 8601 "open start" spelling and reads as a separator; a space between the date and
/// time becomes the `T` the rest of this module expects.
fn normalize(text: &str) -> String {
    text.trim()
        .replace("--", "/")
        .replace(' ', "T")
        .to_ascii_uppercase()
}

/// Strips `-` and `:` so the rest of the module works on fixed-width digit runs.
///
/// A trailing zone designator (`Z`, `+0100`, `+01:00`) keeps its shape minus the colon, because it
/// has to survive into the timestamp conversion.
fn no_delimit(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (body, zone) = split_zone(trimmed);
    Some(format!(
        "{}{}",
        body.replace(['-', ':'], ""),
        zone.replace(':', "")
    ))
}

/// Splits a trailing `Z` or `±HH[:]MM` from the rest.
///
/// The offset must have **four digits** after the sign. Accepting two would read the `-02` at the end
/// of `2006-01-02` as a zone offset, which turns every plain date into garbage — the single most
/// damaging mistake available in this module, because `date:2006-01-02` is the most common spelling.
fn split_zone(text: &str) -> (&str, &str) {
    if text.ends_with('Z') && text.len() > 1 {
        return (&text[..text.len() - 1], "Z");
    }
    // `±HHMM` (5 bytes) or `±HH:MM` (6 bytes), and nothing else.
    for len in [6usize, 5] {
        if text.len() <= len {
            continue;
        }
        let candidate = &text[text.len() - len..];
        if is_offset(candidate) {
            return (&text[..text.len() - len], candidate);
        }
    }
    (text, "")
}

/// True for `±HHMM` or `±HH:MM`.
fn is_offset(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if !matches!(bytes[0], b'+' | b'-') {
        return false;
    }
    match bytes.len() {
        5 => bytes[1..].iter().all(u8::is_ascii_digit),
        6 => bytes[1..].iter().enumerate().all(|(i, b)| {
            if i == 2 {
                *b == b':'
            } else {
                b.is_ascii_digit()
            }
        }),
        _ => false,
    }
}

/// `_dateRelative`: borrows the missing high-order part of the second operand from the first.
fn date_relative(d1: Option<&str>, d2: Option<&str>) -> Option<String> {
    let Some(d2) = d2 else {
        // No second operand: a bare date bounds both ends, a duration bounds only the far one.
        return match d1 {
            Some(d) if !d.starts_with('P') => Some(d.to_owned()),
            _ => None,
        };
    };
    if d2.is_empty() {
        return None;
    }
    if !d2.starts_with('P') {
        if let Some(d1) = d1 {
            if !d1.starts_with('P') && !starts_with_a_year(d2) {
                // Replace the tail of d1 with d2, so `201402` + `04` becomes `201404`.
                let strip = d2.len().min(d1.len());
                return Some(format!("{}{}", &d1[..d1.len() - strip], d2));
            }
        }
    }
    no_delimit(d2)
}

/// True when the operand already carries a four-digit year and so needs no prefix.
fn starts_with_a_year(d: &str) -> bool {
    let digits: String = d.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.len() >= 4
}

/// `_dateFloor`: the first instant of the period the operand names.
///
/// The date part is padded to eight digits with `01` and the time part to six with `0`, so `2014`
/// becomes `20140101T000000` and `201402` becomes `20140201T000000`.
fn date_floor(d: &str) -> String {
    let (date, time, zone) = split_parts(d);
    let date = pad_repeating(date, 8, "01");
    let time = pad_repeating(time, 6, "00");
    format!("{date}T{time}{zone}")
}

/// `_dateCeiling`: the last instant of the period the operand names.
///
/// A year becomes `1231`, a month becomes its real last day (28, 29, 30 or 31), and the time part
/// becomes `235959` when it was absent or `59`-padded when it was present but partial.
fn date_ceiling(d: &str) -> String {
    let (date, time, zone) = split_parts(d);
    // PHP: `strlen($t) > 1 ? str_pad($t, 6, '59') : '235959'`. A full `HHMMSS` is left exactly as
    // given — the ceiling of a precise instant *is* that instant — while a partial one is completed
    // with 59s.
    let time = if time.len() > 1 {
        pad_repeating(time, 6, "59")
    } else {
        "235959".to_owned()
    };
    let date = match date.len() {
        4 => format!("{date}1231"),
        6 => format!("{date}{:02}", last_day_of_month(date)),
        _ => pad_repeating(date, 8, "01"),
    };
    format!("{date}T{time}{zone}")
}

/// The number of days in the month named by a `YYYYMM` string.
fn last_day_of_month(yyyymm: &str) -> u32 {
    let (Ok(year), Ok(month)) = (yyyymm[..4].parse::<i16>(), yyyymm[4..6].parse::<u32>()) else {
        return 31;
    };
    if month == 0 || month > 12 {
        return 31;
    }
    // The 1st of the next month, minus one day. Works for February in leap years without a table.
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_next = Date::new(next_year, next_month as i8, 1).ok();
    let first_this = Date::new(year, month as i8, 1).ok();
    match (first_next, first_this) {
        (Some(next), Some(this)) => {
            let days = next
                .since(this)
                .ok()
                .and_then(|span| u32::try_from(span.get_days()).ok());
            days.unwrap_or(31)
        }
        _ => 31,
    }
}

/// Pads a digit-run to `width` by repeating a two-character pad string, as PHP's `str_pad` does.
///
/// `str_pad("2006", 8, "01")` yields `20060101` — the pad string is repeated, not a single character
/// appended — which is what makes a bare year default to January 1st in one step. Likewise
/// `str_pad("1030", 6, "59")` yields `103059`, so a partial time is floored to the minute and its
/// seconds set to 59 on the way to the ceiling.
fn pad_repeating(text: &str, width: usize, pad: &str) -> String {
    let mut out = text.to_owned();
    while out.len() < width {
        out.push_str(pad);
    }
    out.truncate(out.len().min(width.max(text.len())));
    out
}

/// Splits a normalized operand into `(date_digits, time_digits, zone)`.
fn split_parts(d: &str) -> (&str, &str, &str) {
    let (body, zone) = split_zone(d);
    match body.split_once('T') {
        Some((date, time)) => (date, time, zone),
        None => (body, "", zone),
    }
}

/// Converts a normalized `YYYYMMDDTHHMMSS[zone]` string to unix seconds.
fn instant(normalized: &str) -> Result<i64, SearchError> {
    let (date, time, zone) = split_parts(normalized);
    let date = pad_repeating(date, 8, "01");
    let time = pad_repeating(time, 6, "00");

    let y: i16 = date[..4].parse().map_err(|_| bad(normalized, "no year"))?;
    let mo: i8 = date[4..6]
        .parse()
        .map_err(|_| bad(normalized, "no month"))?;
    let d: i8 = date[6..8].parse().map_err(|_| bad(normalized, "no day"))?;
    let h: i8 = time[..2].parse().map_err(|_| bad(normalized, "no hour"))?;
    let mi: i8 = time[2..4]
        .parse()
        .map_err(|_| bad(normalized, "no minute"))?;
    let s: i8 = time[4..6]
        .parse()
        .map_err(|_| bad(normalized, "no second"))?;

    let civil = jiff::civil::DateTime::new(y, mo, d, h, mi, s, 0)
        .map_err(|e| bad(normalized, &e.to_string()))?;
    let tz = match zone {
        "" | "Z" => TimeZone::UTC,
        z => offset_zone(z).map_err(|e| bad(normalized, &e.to_string()))?,
    };
    let zoned = civil
        .to_zoned(tz)
        .map_err(|e| bad(normalized, &e.to_string()))?;
    Ok(zoned.timestamp().as_second())
}

/// Builds a fixed-offset zone from `±HHMM`.
fn offset_zone(zone: &str) -> Result<TimeZone, jiff::Error> {
    let (sign, digits) = zone.split_at(1);
    let hours: i32 = digits[..2].parse().unwrap_or(0);
    let minutes: i32 = digits.get(2..4).and_then(|m| m.parse().ok()).unwrap_or(0);
    let total = (hours * 3600 + minutes * 60) * if sign == "-" { -1 } else { 1 };
    Ok(TimeZone::fixed(jiff::tz::Offset::from_seconds(total)?))
}

/// Adds an ISO 8601 duration to a unix timestamp.
fn add_duration(base: i64, duration: &str, now: i64) -> Result<i64, SearchError> {
    let span = parse_span(duration)?;
    let zoned = to_utc(base, now)?;
    let result = zoned
        .checked_add(span)
        .map_err(|e| bad(duration, &e.to_string()))?;
    Ok(result.timestamp().as_second())
}

/// Subtracts an ISO 8601 duration from a unix timestamp.
fn sub_duration(base: i64, duration: &str, now: i64) -> Result<i64, SearchError> {
    let span = parse_span(duration)?;
    let zoned = to_utc(base, now)?;
    let result = zoned
        .checked_sub(span)
        .map_err(|e| bad(duration, &e.to_string()))?;
    Ok(result.timestamp().as_second())
}

fn to_utc(secs: i64, now: i64) -> Result<Zoned, SearchError> {
    Timestamp::from_second(secs)
        .or_else(|_| Timestamp::from_second(now))
        .map(|ts| ts.to_zoned(TimeZone::UTC))
        .map_err(|e| bad(&secs.to_string(), &e.to_string()))
}

/// Parses `P1W`, `PT5H`, `P1Y2M3DT4H5M6S` and the rest of the ISO 8601 duration grammar.
///
/// Upper-cased first, because [`normalize`] already did that to the whole query and `Span::from_str`
/// wants the canonical spelling.
fn parse_span(duration: &str) -> Result<Span, SearchError> {
    duration
        .parse::<Span>()
        .map_err(|e| bad(duration, &e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2006-01-02T15:04:05Z, used as "now" throughout.
    const NOW: i64 = 1_136_214_245;

    fn range(text: &str) -> DateRange {
        parse(text, NOW).unwrap()
    }

    /// An RFC 3339 instant as unix seconds, for readable expectations.
    fn at(iso: &str) -> i64 {
        iso.parse::<Timestamp>().unwrap().as_second()
    }

    // --- single absolute dates ---------------------------------------------

    #[test]
    fn a_full_date_bounds_that_day() {
        let r = range("2006-01-02");
        assert_eq!(r.min, Some(at("2006-01-02T00:00:00Z")));
        assert_eq!(r.max, Some(at("2006-01-02T23:59:59Z")));
    }

    #[test]
    fn a_year_bounds_the_whole_year() {
        let r = range("2006");
        assert_eq!(r.min, Some(at("2006-01-01T00:00:00Z")));
        assert_eq!(r.max, Some(at("2006-12-31T23:59:59Z")));
    }

    #[test]
    fn a_month_bounds_the_whole_month() {
        let r = range("2006-02");
        assert_eq!(r.min, Some(at("2006-02-01T00:00:00Z")));
        assert_eq!(r.max, Some(at("2006-02-28T23:59:59Z")));
    }

    #[test]
    fn a_leap_february_gets_twenty_nine_days() {
        let r = range("2008-02");
        assert_eq!(r.max, Some(at("2008-02-29T23:59:59Z")));
    }

    #[test]
    fn a_thirty_one_day_month_is_not_shortened() {
        assert_eq!(range("2006-01").max, Some(at("2006-01-31T23:59:59Z")));
        assert_eq!(range("2006-12").max, Some(at("2006-12-31T23:59:59Z")));
    }

    #[test]
    fn a_date_and_time_is_used_as_given() {
        let r = range("2006-01-02T10:30:00");
        assert_eq!(r.min, Some(at("2006-01-02T10:30:00Z")));
        // The time part is already complete, so the ceiling is that same instant: `str_pad` with
        // "59" only fills what is missing. A *partial* time does get 59s appended.
        assert_eq!(r.max, Some(at("2006-01-02T10:30:00Z")));
        assert_eq!(
            range("2006-01-02T10:30").max,
            Some(at("2006-01-02T10:30:59Z"))
        );
    }

    #[test]
    fn a_space_separating_date_and_time_is_accepted() {
        assert_eq!(range("2006-01-02 10:30:00"), range("2006-01-02T10:30:00"));
    }

    #[test]
    fn a_utc_designator_is_honoured() {
        assert_eq!(range("2006-01-02Z").min, Some(at("2006-01-02T00:00:00Z")));
    }

    #[test]
    fn a_numeric_offset_shifts_the_instant() {
        // 2006-01-02T00:00:00+05:00 is 2006-01-01T19:00:00Z.
        assert_eq!(
            range("2006-01-02T00:00:00+0500").min,
            Some(at("2006-01-01T19:00:00Z"))
        );
        assert_eq!(
            range("2006-01-02T00:00:00+05:00").min,
            Some(at("2006-01-01T19:00:00Z"))
        );
        assert_eq!(
            range("2006-01-02T00:00:00-0500").min,
            Some(at("2006-01-02T05:00:00Z"))
        );
    }

    #[test]
    fn delimiters_are_optional() {
        assert_eq!(range("20060102"), range("2006-01-02"));
        assert_eq!(range("20060102T103000"), range("2006-01-02T10:30:00"));
    }

    // --- explicit ranges ---------------------------------------------------

    #[test]
    fn a_two_date_range_is_inclusive_at_both_ends() {
        let r = range("2006-01-02/2006-01-05");
        assert_eq!(r.min, Some(at("2006-01-02T00:00:00Z")));
        assert_eq!(r.max, Some(at("2006-01-05T23:59:59Z")));
    }

    #[test]
    fn the_second_operand_borrows_the_firsts_prefix() {
        // The signature behaviour: `2014-02/04` is February through April 2014, not "2014-02" and
        // "the 4th of something".
        let r = range("2014-02/04");
        assert_eq!(r.min, Some(at("2014-02-01T00:00:00Z")));
        assert_eq!(r.max, Some(at("2014-04-30T23:59:59Z")));
    }

    #[test]
    fn prefix_borrowing_works_at_day_granularity() {
        let r = range("2006-01-02/15");
        assert_eq!(r.min, Some(at("2006-01-02T00:00:00Z")));
        assert_eq!(r.max, Some(at("2006-01-15T23:59:59Z")));
    }

    #[test]
    fn prefix_borrowing_does_not_apply_when_the_second_operand_has_a_year() {
        let r = range("2006-01-02/2007-03-04");
        assert_eq!(r.min, Some(at("2006-01-02T00:00:00Z")));
        assert_eq!(r.max, Some(at("2007-03-04T23:59:59Z")));
    }

    #[test]
    fn double_dash_is_a_separator() {
        assert_eq!(
            range("2006-01-02--2006-01-05"),
            range("2006-01-02/2006-01-05")
        );
    }

    // --- durations ---------------------------------------------------------

    #[test]
    fn a_bare_duration_looks_back_from_now() {
        let r = range("P1W");
        assert_eq!(r.max, None, "open-ended into the future");
        assert_eq!(r.min, Some(NOW - 7 * 86_400 + 1));
    }

    #[test]
    fn a_duration_in_hours() {
        let r = range("PT5H");
        assert_eq!(r.min, Some(NOW - 5 * 3600 + 1));
        assert_eq!(r.max, None);
    }

    #[test]
    fn a_duration_in_days_months_and_years() {
        assert_eq!(range("P1D").min, Some(NOW - 86_400 + 1));
        // A month is calendar-aware: 2006-01-02 minus one month is 2005-12-02.
        assert_eq!(range("P1M").min, Some(at("2005-12-02T15:04:06Z")));
        assert_eq!(range("P1Y").min, Some(at("2005-01-02T15:04:06Z")));
    }

    #[test]
    fn a_compound_duration() {
        let r = range("P1DT2H3M4S");
        let want = NOW - (86_400 + 2 * 3600 + 3 * 60 + 4) + 1;
        assert_eq!(r.min, Some(want));
    }

    #[test]
    fn a_duration_after_a_date_runs_forward_from_it() {
        let r = range("2006-01-01/P1M");
        assert_eq!(r.min, Some(at("2006-01-01T00:00:00Z")));
        // `min + 1 month - 1 second`: the last instant of January.
        assert_eq!(r.max, Some(at("2006-01-31T23:59:59Z")));
    }

    #[test]
    fn a_duration_before_a_date_runs_backward_from_it() {
        let r = range("P1M/2006-03-31");
        // PHP takes this branch with a plain `strtotime($d2)`, not the ceiling: when the first
        // operand is a duration the second is read as an exact instant. So the upper bound is
        // midnight on the 31st, and the lower bound is one month earlier plus a second.
        assert_eq!(r.max, Some(at("2006-03-31T00:00:00Z")));
        assert_eq!(r.min, Some(at("2006-02-28T00:00:00Z").saturating_add(1)));
    }

    #[test]
    fn the_inclusive_adjustments_are_one_second() {
        // `- 1` on the upper bound and `+ 1` on the lower: PHP does both, so an interval measured
        // from `now` includes the present second exactly once.
        assert_eq!(range("PT1S").min, Some(NOW));
        assert_eq!(
            range("2006-01-01/PT1S").max,
            Some(at("2006-01-01T00:00:00Z"))
        );
    }

    // --- errors ------------------------------------------------------------

    #[test]
    fn an_empty_interval_is_an_open_range_not_an_error() {
        let r = range("");
        assert!(r.is_unbounded());
        assert!(range("   ").is_unbounded());
    }

    #[test]
    fn nonsense_is_an_error() {
        for bad in [
            "not-a-date",
            "P",
            "PX",
            "2006-13-45",
            "2006/01/02",
            "yesterday",
        ] {
            let err = parse(bad, NOW).unwrap_err();
            assert!(
                matches!(err, SearchError::BadDateInterval { .. }),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn the_error_names_the_offending_value() {
        let err = parse("banana", NOW).unwrap_err();
        assert!(err.to_string().contains("banana"), "{err}");
    }

    // --- internals ---------------------------------------------------------

    #[test]
    fn no_delimit_strips_separators_but_keeps_the_zone() {
        assert_eq!(no_delimit("2006-01-02").as_deref(), Some("20060102"));
        assert_eq!(
            no_delimit("2006-01-02T10:30:00Z").as_deref(),
            Some("20060102T103000Z")
        );
        assert_eq!(
            no_delimit("2006-01-02T10:30:00+05:00").as_deref(),
            Some("20060102T103000+0500")
        );
        assert_eq!(no_delimit(""), None);
    }

    #[test]
    fn date_floor_pads_with_ones() {
        assert_eq!(date_floor("2006"), "20060101T000000");
        assert_eq!(date_floor("200602"), "20060201T000000");
        assert_eq!(date_floor("20060102"), "20060102T000000");
        assert_eq!(date_floor("20060102T10"), "20060102T100000");
        assert_eq!(date_floor("20060102T1030"), "20060102T103000");
    }

    #[test]
    fn date_ceiling_pads_with_nines_and_ends_the_month() {
        assert_eq!(date_ceiling("2006"), "20061231T235959");
        assert_eq!(date_ceiling("200602"), "20060228T235959");
        assert_eq!(date_ceiling("200802"), "20080229T235959");
        assert_eq!(date_ceiling("20060102"), "20060102T235959");
        assert_eq!(date_ceiling("20060102T10"), "20060102T105959");
        assert_eq!(date_ceiling("20060102T103000"), "20060102T103000");
    }

    #[test]
    fn prefix_borrowing_is_tail_replacement() {
        assert_eq!(
            date_relative(Some("201402"), Some("04")).as_deref(),
            Some("201404")
        );
        assert_eq!(
            date_relative(Some("20060102"), Some("15")).as_deref(),
            Some("20060115")
        );
        // A duration as the second operand is not borrowed from.
        assert_eq!(
            date_relative(Some("20060102"), Some("P1M")).as_deref(),
            Some("P1M")
        );
        // No second operand: a date bounds both ends, a duration bounds one.
        assert_eq!(
            date_relative(Some("20060102"), None).as_deref(),
            Some("20060102")
        );
        assert_eq!(date_relative(Some("P1M"), None), None);
        assert_eq!(date_relative(None, None), None);
    }

    #[test]
    fn parsing_is_deterministic_for_a_fixed_now() {
        assert_eq!(parse("P1W", NOW).unwrap(), parse("P1W", NOW).unwrap());
    }
}
