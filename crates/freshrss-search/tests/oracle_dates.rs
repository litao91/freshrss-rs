//! Oracle test: `tests/lib/LibDateTest.php` from the PHP FreshRSS, row by row.
//!
//! That file is the specification for [`freshrss_search::dateinterval`], so this one mirrors it: same
//! rows, same order, same section comments, and the same expected values *spelled the same way* —
//! PHP's `strtotime('2014-01-01 00:00:00')` becomes [`Bound::At("2014-01-01 00:00:00")`]. No numeric
//! timestamp is hardcoded anywhere, so the two files can be diffed line by line and a change in the
//! PHP suite shows up here as a string edit rather than as arithmetic to redo.
//!
//! Two translation notes:
//!
//! - **Timezone.** PHP's `strtotime` resolves a bare `YYYY-MM-DD HH:MM:SS` in the process's default
//!   timezone; the port is UTC-only, so [`at`] reads those expectations as UTC.
//! - **`false`.** PHP returns `array{int|null|false, int|null|false}`. `null` is an open end and maps
//!   to `None`; `false` means the interval could not be read at all, which in Rust is a whole-row
//!   `Err(SearchError::BadDateInterval)` — there is no per-bound error to carry. [`Bound::Invalid`]
//!   is that marker. `provideDateIntervals` has no `false` rows, so [`PHP_FALSE_ROWS`] documents the
//!   shapes that produce them instead of quietly dropping the third arm of PHP's return type.
//!
//! The PHP file holds a single data provider and no relative-only cases: every row either is the
//! empty string or anchors its duration to an absolute date, so `NOW` is never load-bearing here.
//! [`absolute_rows_ignore_now`] proves that rather than asserting it.
//!
//! **No row carries a divergence note** — all 33 agree with PHP as written, including the one that
//! pins a non-UTC offset (an explicit offset is timezone-independent, so PHP's `strtotime` and the
//! UTC-only port compute the same instant). One row did not agree until the port was fixed:
//! `2014-02-03T22:00/22:15` measured the borrowed tail *before* stripping its `:`, so the cut landed
//! on the `T` and the interval silently widened to the whole day. That was a bug in
//! `src/dateinterval.rs`, not a divergence, and the shape is pinned there too by
//! `prefix_borrowing_measures_the_operand_after_its_delimiters_are_stripped`.

use std::str::FromStr;

use freshrss_search::dateinterval::parse;
use freshrss_search::SearchError;

/// The reference instant passed to [`parse`] for relative intervals.
///
/// Fixed, so a failure reproduces. Irrelevant for every row below — see [`absolute_rows_ignore_now`].
const NOW: i64 = 1_700_000_000;

/// One expected bound, mirroring PHP's `int|null|false`.
#[derive(Clone, Copy, Debug)]
enum Bound {
    /// `strtotime('...')` in the PHP row: an exact instant, written here as the same ISO string.
    At(&'static str),
    /// PHP `null`: an open end, `None` in Rust.
    Null,
    /// PHP `false`: unparseable. Rust fails the whole parse, so this marks the row, not the bound.
    Invalid,
}

/// The rows of `provideDateIntervals`, verbatim and in order.
const ORACLE_ROWS: &[(&str, Bound, Bound)] = &[
    ("", Bound::Null, Bound::Null), // Empty string
    // Year intervals
    (
        "2014",
        Bound::At("2014-01-01 00:00:00"),
        Bound::At("2014-12-31 23:59:59"),
    ),
    (
        " 2015 ",
        Bound::At("2015-01-01 00:00:00"),
        Bound::At("2015-12-31 23:59:59"),
    ), // With whitespace to be trimmed
    // Year-month intervals
    (
        "2014-03",
        Bound::At("2014-03-01 00:00:00"),
        Bound::At("2014-03-31 23:59:59"),
    ),
    (
        "2016-02",
        Bound::At("2016-02-01 00:00:00"),
        Bound::At("2016-02-29 23:59:59"),
    ), // Leap year
    (
        "2014-02",
        Bound::At("2014-02-01 00:00:00"),
        Bound::At("2014-02-28 23:59:59"),
    ), // Non-leap year
    (
        "201404",
        Bound::At("2014-04-01 00:00:00"),
        Bound::At("2014-04-30 23:59:59"),
    ), // Without hyphen
    // Specific dates
    (
        "2014-03-30",
        Bound::At("2014-03-30 00:00:00"),
        Bound::At("2014-03-30 23:59:59"),
    ),
    (
        "2014-05-30T13",
        Bound::At("2014-05-30 13:00:00"),
        Bound::At("2014-05-30 13:59:59"),
    ),
    (
        "2014-05-30T13:30",
        Bound::At("2014-05-30 13:30:00"),
        Bound::At("2014-05-30 13:30:59"),
    ),
    // With negative timezone offset. The expectation pins the same offset the input does, so PHP's
    // `strtotime` and the UTC-only port compute the identical instant: no divergence here.
    (
        "2014-05-30T13:30-05:00",
        Bound::At("2014-05-30T13:30:00-05:00"),
        Bound::At("2014-05-30T13:30:59-05:00"),
    ),
    // Date ranges with explicit end dates
    (
        "2014-02/2014-04",
        Bound::At("2014-02-01 00:00:00"),
        Bound::At("2014-04-30 23:59:59"),
    ),
    (
        "2014-02--2014-04",
        Bound::At("2014-02-01 00:00:00"),
        Bound::At("2014-04-30 23:59:59"),
    ), // Same with -- separator
    (
        "2014-02/04",
        Bound::At("2014-02-01 00:00:00"),
        Bound::At("2014-04-30 23:59:59"),
    ),
    (
        "2014-02-03/05",
        Bound::At("2014-02-03 00:00:00"),
        Bound::At("2014-02-05 23:59:59"),
    ),
    // Time ranges within same day
    (
        "2014-02-03T22:00/22:15",
        Bound::At("2014-02-03 22:00:00"),
        Bound::At("2014-02-03 22:15:59"),
    ),
    (
        "2014-02-03T22:00/15",
        Bound::At("2014-02-03 22:00:00"),
        Bound::At("2014-02-03 22:15:59"),
    ),
    // Open intervals
    ("2014-03/", Bound::At("2014-03-01 00:00:00"), Bound::Null),
    ("/2014-03", Bound::Null, Bound::At("2014-03-31 23:59:59")),
    // Period-based intervals
    (
        "2014-03/P1W",
        Bound::At("2014-03-01 00:00:00"),
        Bound::At("2014-03-07 23:59:59"),
    ),
    (
        "P1W/2014-05-25T23:59:59",
        Bound::At("2014-05-19 00:00:00"),
        Bound::At("2014-05-25 23:59:59"),
    ),
    // Fixed date periods with known anchors
    (
        "2014-01-01/P1Y",
        Bound::At("2014-01-01 00:00:00"),
        Bound::At("2014-12-31 23:59:59"),
    ),
    (
        "2014-06-15/P6M",
        Bound::At("2014-06-15 00:00:00"),
        Bound::At("2014-12-14 23:59:59"),
    ),
    (
        "2014-03-01/P2W",
        Bound::At("2014-03-01 00:00:00"),
        Bound::At("2014-03-14 23:59:59"),
    ),
    (
        "2014-12-25/P10D",
        Bound::At("2014-12-25 00:00:00"),
        Bound::At("2015-01-03 23:59:59"),
    ),
    (
        "2014-01-01T12:00/PT6H",
        Bound::At("2014-01-01 12:00:00"),
        Bound::At("2014-01-01 17:59:59"),
    ),
    (
        "2014-01-01 12:00/PT6H",
        Bound::At("2014-01-01 12:00:00"),
        Bound::At("2014-01-01 17:59:59"),
    ), // Space instead of T
    (
        "2014-01-01T12:00/PT6h",
        Bound::At("2014-01-01 12:00:00"),
        Bound::At("2014-01-01 17:59:59"),
    ), // Lowercase h
    (
        "2014-05-01T10:30/PT90M",
        Bound::At("2014-05-01 10:30:00"),
        Bound::At("2014-05-01 11:59:59"),
    ),
    (
        "2014-07-04T14:15:30/PT45S",
        Bound::At("2014-07-04 14:15:30"),
        Bound::At("2014-07-04 14:16:14"),
    ),
    // Reverse periods. PHP reads the second operand as an exact instant rather than a ceiling when
    // the first is a duration, which is why two of these end at midnight and start a second later.
    (
        "P1M/2014-02-28",
        Bound::At("2014-01-28 00:00:01"),
        Bound::At("2014-02-28 00:00:00"),
    ),
    (
        "P3D/2014-01-10T23:59:59",
        Bound::At("2014-01-08 00:00:00"),
        Bound::At("2014-01-10 23:59:59"),
    ),
    (
        "PT12H/2014-06-01T12:00",
        Bound::At("2014-06-01 00:00:01"),
        Bound::At("2014-06-01 12:00:00"),
    ),
];

/// Rows the PHP provider does *not* contain, covering the `false` arm of its return type.
///
/// Each is unparseable in PHP for the reason given, and `parseDateInterval` returns `false` in one or
/// both slots. Rust has no per-bound error, so the whole parse fails with
/// [`SearchError::BadDateInterval`] — the caller learns the interval is unusable either way, which is
/// the only part of `false` that matters: PHP's `false` is not a bound and must never be compared
/// against one.
const PHP_FALSE_ROWS: &[(&str, Bound, Bound)] = &[
    // `@strtotime("BANANAT000000")` fails on both ends: `[false, false]`.
    ("banana", Bound::Invalid, Bound::Invalid),
    // `new DateInterval("PX")` throws, so `min` is false; a leading duration yields no `max`.
    ("PX", Bound::Invalid, Bound::Null),
    // Same, with the shortest possible malformed duration.
    ("P", Bound::Invalid, Bound::Null),
    // Month 13, day 45: `_dateFloor` and `_dateCeiling` both hand `strtotime` nonsense.
    ("2006-13-45", Bound::Invalid, Bound::Invalid),
    // PHP does read a `min` here and only `false` for the `max` (`strtotime("201402éT235959")`
    // fails), but that is still a row it cannot answer, and Rust has no half-answer to give. It is
    // also why the ASCII guard in `parse_inner` exists: PHP's byte-wise `substr` cannot split a
    // character, a Rust slice can, and this input used to panic instead of failing.
    ("2014-02-03/é", Bound::Invalid, Bound::Invalid),
];

/// `strtotime('...')` for the expectation column.
///
/// The offset form must be tried first: `jiff::civil::DateTime` also parses
/// `2014-05-30T13:30:00-05:00`, and throws the offset away. A bare civil datetime is read as UTC —
/// see the timezone note in the module docs.
fn at(iso: &str) -> i64 {
    if let Ok(ts) = jiff::Timestamp::from_str(iso) {
        return ts.as_second();
    }
    let civil = jiff::civil::DateTime::from_str(iso)
        .unwrap_or_else(|e| panic!("{iso:?} is not a date: {e}"));
    civil
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap_or_else(|e| panic!("{iso:?} does not exist in UTC: {e}"))
        .timestamp()
        .as_second()
}

/// An expected bound, as it reads in a failure message.
fn show_expected(bound: Bound) -> String {
    match bound {
        Bound::At(iso) => format!("{iso} = {}", at(iso)),
        Bound::Null => "null".to_owned(),
        Bound::Invalid => "false".to_owned(),
    }
}

/// An actual bound, as it reads in a failure message: the instant a human checks, and the raw second
/// count to paste into a debugger.
fn show_actual(secs: Option<i64>) -> String {
    let Some(secs) = secs else {
        return "null".to_owned();
    };
    let utc = jiff::Timestamp::from_second(secs)
        .map(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| "out of range".to_owned());
    format!("{utc} = {secs}")
}

/// Checks one row, returning every way it disagrees so a run reports the whole delta at once.
fn check(interval: &str, want_min: Bound, want_max: Bound) -> Vec<String> {
    let got = parse(interval, NOW);

    // PHP `false` is not a bound: the row is rejected outright, or it is not.
    if matches!(want_min, Bound::Invalid) || matches!(want_max, Bound::Invalid) {
        return match got {
            Err(SearchError::BadDateInterval { .. }) => Vec::new(),
            Err(other) => vec![format!(
                "  {interval:?}: expected a bad-interval error, got {other}"
            )],
            Ok(range) => vec![format!(
                "  {interval:?}: expected an unparseable interval, got min={} max={}",
                show_actual(range.min),
                show_actual(range.max)
            )],
        };
    }

    let range = match got {
        Ok(range) => range,
        Err(e) => return vec![format!("  {interval:?}: expected a range, got error {e}")],
    };

    let mut problems = Vec::new();
    for (end, want, got) in [("min", want_min, range.min), ("max", want_max, range.max)] {
        // `Bound::Invalid` was handled above and never reaches here.
        let want_secs = match want {
            Bound::At(iso) => Some(at(iso)),
            Bound::Null => None,
            Bound::Invalid => continue,
        };
        if want_secs != got {
            problems.push(format!(
                "  {interval:?} {end}: php expects {}, rust gives {}",
                show_expected(want),
                show_actual(got)
            ));
        }
    }
    problems
}

fn check_all(rows: &[(&str, Bound, Bound)]) {
    let mut failures: Vec<String> = rows
        .iter()
        .flat_map(|(interval, min, max)| check(interval, *min, *max))
        .collect();
    if failures.is_empty() {
        return;
    }
    failures.insert(
        0,
        format!(
            "{} of {} rows disagree with the php oracle:",
            failures.len(),
            rows.len()
        ),
    );
    panic!("{}", failures.join("\n"));
}

#[test]
fn parse_date_interval_matches_the_php_oracle() {
    // `provideDateIntervals` has exactly 33 rows. If this fires, a row was dropped or added here
    // without the PHP file changing to match — the whole point of the port is that they agree.
    assert_eq!(
        ORACLE_ROWS.len(),
        33,
        "row count drifted from tests/lib/LibDateTest.php"
    );
    check_all(ORACLE_ROWS);
}

#[test]
fn php_false_rows_are_rejected() {
    check_all(PHP_FALSE_ROWS);
}

#[test]
fn absolute_rows_ignore_now() {
    // The oracle's expectations are absolute, so none of them may move with `now`. Running the table
    // at two instants a decade apart and in both directions catches a duration that quietly fell
    // back to `now` instead of to its anchor — the failure mode where `2014-03/P1W` starts measuring
    // from today and the test still passes for whoever runs it first.
    for now in [0i64, NOW, 2_000_000_000] {
        for (interval, want_min, want_max) in ORACLE_ROWS.iter().chain(PHP_FALSE_ROWS) {
            let moved = parse(interval, now);
            let baseline = parse(interval, NOW);
            assert_eq!(
                moved.as_ref().map(|r| (r.min, r.max)),
                baseline.as_ref().map(|r| (r.min, r.max)),
                "{interval:?} depends on `now` (tried {now}); php expects {} / {}",
                show_expected(*want_min),
                show_expected(*want_max)
            );
        }
    }
}
