//! Entry ids and the microsecond clock they are derived from.
//!
//! FreshRSS's `uTimeString()` builds an entry id by string-concatenating the unix seconds with a
//! zero-padded 6-digit microsecond component, e.g. `1728656123` + `000123`. That is numerically
//! identical to `secs * 1_000_000 + micros`, which is what we use.
//!
//! Two properties of this scheme are load-bearing elsewhere and must not be broken:
//!
//! * ids are **time-ordered**, so `ORDER BY id` is `ORDER BY ingestion time` and keyset
//!   pagination can use `id` alone;
//! * `date:` searches compare against `id` using `"{unix_seconds}000000"`, i.e. ingestion time,
//!   not the publication date in the `date` column (`pubdate:` is the one that uses that).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Microseconds per second — the scale factor between an entry id and a unix timestamp.
pub const MICROS_PER_SEC: i64 = 1_000_000;

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// Current time as a microsecond-scale entry id.
pub fn now_micros() -> i64 {
    timestamp_to_micros(jiff::Timestamp::now())
}

fn timestamp_to_micros(ts: jiff::Timestamp) -> i64 {
    // `as_microsecond` would overflow i64 for dates far outside the useful range, so compose
    // from seconds and the sub-second remainder instead.
    ts.as_second()
        .saturating_mul(MICROS_PER_SEC)
        .saturating_add(i64::from(ts.subsec_microsecond()))
}

/// An entry identifier: a microsecond-scale, time-ordered integer.
///
/// Serialized as a **string** in JSON. The value exceeds 2^53, so a JSON number would lose
/// precision in JavaScript; the frontend treats ids as opaque cursors anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntryId(pub i64);

impl EntryId {
    /// The id to allocate for an entry created right now.
    pub fn now() -> Self {
        Self(now_micros())
    }

    /// Wraps an already-computed microsecond value.
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// The id corresponding to a whole-second unix timestamp, as `date:` searches compare
    /// against. FreshRSS spells this `"{secs}000000"`.
    pub const fn from_unix(secs: i64) -> Self {
        Self(secs.saturating_mul(MICROS_PER_SEC))
    }

    /// The raw microsecond value stored in the `entry.id` column.
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// Truncates to unix seconds.
    pub const fn unix(self) -> i64 {
        self.0 / MICROS_PER_SEC
    }

    /// The sub-second part, in microseconds.
    pub const fn subsec_micros(self) -> i64 {
        self.0 % MICROS_PER_SEC
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for EntryId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim().parse::<i64>().map(Self)
    }
}

impl From<i64> for EntryId {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

impl From<EntryId> for i64 {
    fn from(v: EntryId) -> Self {
        v.0
    }
}

impl Serialize for EntryId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Accept both a JSON string and a number so hand-written API calls and imports work.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Num(i64),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Num(n) => Self(n),
            Repr::Str(s) => Self::from_str(&s).map_err(serde::de::Error::custom)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_composition_matches_freshrss_string_concatenation() {
        // FreshRSS: ((string)1728656123) . str_pad("123", 6, "0", LEFT) === "1728656123000123"
        let ts = jiff::Timestamp::from_microsecond(1_728_656_123_000_123).expect("valid ts");
        assert_eq!(timestamp_to_micros(ts), 1_728_656_123_000_123);
    }

    #[test]
    fn ids_are_time_ordered() {
        let a = EntryId::from_unix(1_700_000_000);
        let b = EntryId::from_unix(1_700_000_001);
        assert!(a < b);
    }

    #[test]
    fn from_unix_appends_six_zeroes() {
        assert_eq!(
            EntryId::from_unix(1_728_656_123).micros(),
            1_728_656_123_000_000
        );
        assert_eq!(EntryId::from_unix(0).micros(), 0);
    }

    #[test]
    fn unix_truncates_the_subsecond_part() {
        let id = EntryId::from_micros(1_728_656_123_456_789);
        assert_eq!(id.unix(), 1_728_656_123);
        assert_eq!(id.subsec_micros(), 456_789);
    }

    #[test]
    fn now_is_microsecond_scale_and_monotonic_enough() {
        let a = EntryId::now();
        let b = EntryId::now();
        assert!(
            a.micros() > 1_600_000_000_000_000,
            "id looks like seconds, not micros: {a}"
        );
        assert!(b >= a, "ids must not go backwards within a process");
    }

    #[test]
    fn serializes_as_a_string_so_javascript_does_not_lose_precision() {
        let id = EntryId::from_micros(1_728_656_123_456_789);
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"1728656123456789\"");
        // Far beyond what a 32-bit int holds, but still under 2^53 — the largest integer an IEEE-754
        // double (and therefore a JS number) represents exactly.
        assert!(id.micros() > i64::from(i32::MAX));
        assert!(id.micros() < 2_i64.pow(53), "still exactly representable");
        // A JSON *number* of this size would survive, but a client doing arithmetic on it would
        // not; the string form removes the question.
        assert!(id.micros() > 2_i64.pow(32));
    }

    #[test]
    fn deserializes_from_string_or_number() {
        let from_str: EntryId = serde_json::from_str("\"1728656123456789\"").unwrap();
        let from_num: EntryId = serde_json::from_str("1728656123456789").unwrap();
        assert_eq!(from_str, from_num);
        assert_eq!(from_str.micros(), 1_728_656_123_456_789);
    }

    #[test]
    fn rejects_a_malformed_id() {
        assert!(serde_json::from_str::<EntryId>("\"not-a-number\"").is_err());
        assert!("abc".parse::<EntryId>().is_err());
        assert!(
            "  42  ".parse::<EntryId>().is_ok(),
            "surrounding space is tolerated"
        );
    }

    #[test]
    fn round_trips_through_display_and_fromstr() {
        let id = EntryId::from_micros(1_728_656_123_456_789);
        assert_eq!(id.to_string().parse::<EntryId>().unwrap(), id);
    }
}
