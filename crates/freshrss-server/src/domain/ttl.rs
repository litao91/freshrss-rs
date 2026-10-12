//! Feed TTL, including FreshRSS's sign-encoded mute flag.
//!
//! The stored value is `ttl * (muted ? -1 : 1)`, so **a negative ttl means the feed is muted**
//! and a zero ttl means "use the owner's `ttl_default`". That convention is a landmine in
//! `WHERE` clauses — `ttl >= 0` reads like a range check but is actually the mute filter in
/// `listFeedsOrderUpdate` — so all ttl arithmetic lives here and nowhere else.

use serde::{Deserialize, Serialize};

/// FreshRSS clamps the magnitude to this before storing.
pub const MAX_TTL: i32 = 100_000_000;

/// Absolute value clamped to [`MAX_TTL`], without ever overflowing on `i32::MIN`.
const fn clamped_magnitude(value: i32) -> i32 {
    let magnitude = value.unsigned_abs();
    if magnitude > MAX_TTL as u32 {
        MAX_TTL
    } else {
        magnitude as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ttl(i32);

impl Ttl {
    /// `0`, meaning "defer to the user's default".
    pub const DEFAULT: Self = Self(0);

    /// Wraps a raw stored value without re-clamping. Use when reading a column.
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Builds a ttl from an interval, muted or not.
    ///
    /// The magnitude is taken with `unsigned_abs` rather than `abs` because `i32::MIN.abs()`
    /// overflows and panics in a debug build — and a garbage value from an import or a hand-edited
    /// row must not be able to take the server down.
    pub fn new(seconds: i32, muted: bool) -> Self {
        let magnitude = clamped_magnitude(seconds);
        Self(if muted { -magnitude } else { magnitude })
    }

    /// The value stored in the column.
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// The interval in seconds, ignoring the mute sign.
    pub const fn seconds(self) -> i32 {
        clamped_magnitude(self.0)
    }

    pub const fn is_muted(self) -> bool {
        self.0 < 0
    }

    /// True when the value defers to the owner's `ttl_default`.
    pub const fn is_default(self) -> bool {
        self.0 == 0
    }

    pub const fn mute(self) -> Self {
        Self(-self.seconds())
    }

    pub const fn unmute(self) -> Self {
        Self(self.seconds())
    }

    /// The interval actually used when deciding whether a refresh is due.
    ///
    /// A muted feed keeps its magnitude here — muting is expressed by `is_muted`, and the
    /// due-feed query excludes muted rows with `ttl >= 0` before this is ever consulted.
    pub fn effective_seconds(self, user_default: i64) -> i64 {
        let magnitude = i64::from(self.seconds());
        if magnitude == 0 {
            user_default.max(0)
        } else {
            magnitude
        }
    }
}

impl Default for Ttl {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<i32> for Ttl {
    fn from(raw: i32) -> Self {
        Self::from_raw(raw)
    }
}

impl From<Ttl> for i32 {
    fn from(t: Ttl) -> Self {
        t.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_is_muted_and_keeps_its_magnitude() {
        let t = Ttl::new(3600, true);
        assert_eq!(t.raw(), -3600);
        assert!(t.is_muted());
        assert_eq!(t.seconds(), 3600);
    }

    #[test]
    fn muting_and_unmuting_round_trip() {
        let t = Ttl::new(900, false);
        assert!(!t.is_muted());
        assert_eq!(t.mute().raw(), -900);
        assert_eq!(t.mute().unmute(), t);
        // Muting an already-muted feed must not flip it back.
        assert_eq!(t.mute().mute().raw(), -900);
    }

    #[test]
    fn zero_defers_to_the_user_default() {
        let t = Ttl::DEFAULT;
        assert!(t.is_default());
        assert!(!t.is_muted());
        assert_eq!(t.effective_seconds(3600), 3600);
        assert_eq!(t.effective_seconds(60), 60);
    }

    #[test]
    fn a_nonzero_ttl_ignores_the_user_default() {
        assert_eq!(Ttl::new(120, false).effective_seconds(3600), 120);
        assert_eq!(Ttl::new(120, true).effective_seconds(3600), 120);
    }

    #[test]
    fn a_muted_zero_ttl_stays_zero_rather_than_becoming_negative() {
        // Muting a feed whose ttl was 0 stores -0 == 0, so `ttl >= 0` would no longer exclude it.
        // `is_muted` is therefore not sufficient on its own for a feed that was never given a
        // ttl; the caller must persist a real magnitude when muting.
        let t = Ttl::DEFAULT.mute();
        assert_eq!(t.raw(), 0);
        assert!(!t.is_muted());
    }

    #[test]
    fn magnitude_is_clamped() {
        assert_eq!(Ttl::new(i32::MAX, false).raw(), MAX_TTL);
        assert_eq!(Ttl::new(i32::MIN, false).raw(), MAX_TTL);
        assert_eq!(Ttl::new(i32::MAX, true).raw(), -MAX_TTL);
    }

    #[test]
    fn negative_user_default_is_treated_as_zero() {
        assert_eq!(Ttl::DEFAULT.effective_seconds(-5), 0);
    }

    #[test]
    fn serializes_as_the_raw_signed_value() {
        assert_eq!(serde_json::to_string(&Ttl::new(3600, true)).unwrap(), "-3600");
        let back: Ttl = serde_json::from_str("-3600").unwrap();
        assert!(back.is_muted());
        assert_eq!(back.seconds(), 3600);
    }
}
