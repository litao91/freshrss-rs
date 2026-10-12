//! The read/starred filter, as the two groups it actually is.
//!
//! FreshRSS spells this as a bitmask on `FreshRSS_Entry` — `STATE_READ=1`, `STATE_NOT_READ=2`,
//! `STATE_FAVORITE=4`, `STATE_NOT_FAVORITE=8`, `STATE_OR_NOT_READ=32`, `STATE_OR_FAVORITE=64` — and
//! compiles it to SQL by string surgery: it appends `AND (is_read=0)`, then `rtrim($search, ') ')` to
//! *reopen the last group it emitted* and splice ` OR is_read=0)` into it. The plan for this port calls
//! that out as the thing not to reproduce, and this module is why: the splice means the ORs land
//! inside whichever AND happened to be written last, so the same bits produce a different predicate
//! depending on the order the conditions were emitted in.
//!
//! Here the two groups are separate fields and the meaning is fixed:
//!
//! - every condition in [`EntryState::ands`] must hold;
//! - if [`EntryState::ors`] is non-empty, at least one of those must hold too.
//!
//! # Where that differs from FreshRSS
//!
//! Only when both groups are non-empty at once. FreshRSS would fold the ORs into the last AND; this
//! keeps them as their own conjunction term. The difference is unreachable in practice — FreshRSS sets
//! the OR bits on their own (`Context::$state = STATE_OR_NOT_READ | STATE_OR_FAVORITE`, the "unread or
//! starred" view) and never combines them with the AND bits — and the reading here is the one the
//! names suggest.

/// One read/starred condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cond {
    Read,
    NotRead,
    Favorite,
    NotFavorite,
}

impl Cond {
    /// The column this condition reads, unqualified.
    pub fn column(&self) -> &'static str {
        match self {
            Cond::Read | Cond::NotRead => "is_read",
            Cond::Favorite | Cond::NotFavorite => "is_favorite",
        }
    }

    /// The value the column must equal.
    pub fn value(&self) -> i64 {
        match self {
            Cond::Read | Cond::Favorite => 1,
            Cond::NotRead | Cond::NotFavorite => 0,
        }
    }

    /// The condition that excludes everything this one includes.
    pub fn negate(self) -> Self {
        match self {
            Cond::Read => Cond::NotRead,
            Cond::NotRead => Cond::Read,
            Cond::Favorite => Cond::NotFavorite,
            Cond::NotFavorite => Cond::Favorite,
        }
    }
}

/// Which articles a listing admits, by read and starred state.
///
/// The default is unconstrained: everything the scope and the search allow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryState {
    /// Conditions every row must satisfy.
    ///
    /// Contradictory pairs cancel rather than match nothing: `READ | NOT_READ` in FreshRSS's bitmask
    /// means "either", which is no filter at all. [`EntryState::from_bits`] applies that rule, so a
    /// caller building this directly should not push both.
    pub ands: Vec<Cond>,
    /// Conditions of which at least one must hold. Empty means "no additional constraint", not
    /// "match nothing".
    pub ors: Vec<Cond>,
}

/// The FreshRSS bitmask values, kept so the wire format and any imported preference still reads.
pub mod bits {
    pub const READ: i32 = 1;
    pub const NOT_READ: i32 = 2;
    pub const ALL: i32 = READ | NOT_READ;
    pub const FAVORITE: i32 = 4;
    pub const NOT_FAVORITE: i32 = 8;
    pub const OR_NOT_READ: i32 = 32;
    pub const OR_FAVORITE: i32 = 64;
}

impl EntryState {
    /// No constraint at all — FreshRSS's `STATE_ALL`, which is `READ | NOT_READ` and therefore
    /// deliberately means "either".
    pub const ALL: Self = Self {
        ands: Vec::new(),
        ors: Vec::new(),
    };

    /// Reads a FreshRSS state bitmask.
    ///
    /// A pair of contradictory bits cancels out the way the bitmask does: `READ | NOT_READ` asks for
    /// both, which is everything, so neither becomes a condition.
    pub fn from_bits(bits: i32) -> Self {
        let mut state = Self::ALL;
        let read = bits & bits::READ != 0;
        let not_read = bits & bits::NOT_READ != 0;
        if read != not_read {
            state.ands.push(if read { Cond::Read } else { Cond::NotRead });
        }
        let favorite = bits & bits::FAVORITE != 0;
        let not_favorite = bits & bits::NOT_FAVORITE != 0;
        if favorite != not_favorite {
            state.ands.push(if favorite {
                Cond::Favorite
            } else {
                Cond::NotFavorite
            });
        }
        if bits & bits::OR_NOT_READ != 0 {
            state.ors.push(Cond::NotRead);
        }
        if bits & bits::OR_FAVORITE != 0 {
            state.ors.push(Cond::Favorite);
        }
        state
    }

    /// The bitmask form, for storing in a saved query's `state` column.
    ///
    /// Lossy in one direction: a state whose `ands` hold both `Read` and `NotRead` cannot be built
    /// from bits and cannot be written back as any meaningful bit.
    pub fn to_bits(&self) -> i32 {
        let mut out = 0;
        for cond in &self.ands {
            out |= match cond {
                Cond::Read => bits::READ,
                Cond::NotRead => bits::NOT_READ,
                Cond::Favorite => bits::FAVORITE,
                Cond::NotFavorite => bits::NOT_FAVORITE,
            };
        }
        for cond in &self.ors {
            out |= match cond {
                Cond::NotRead => bits::OR_NOT_READ,
                Cond::Favorite => bits::OR_FAVORITE,
                Cond::Read | Cond::NotFavorite => 0,
            };
        }
        out
    }

    /// True when this admits every row, so the query builder can omit the clause entirely.
    pub fn is_unconstrained(&self) -> bool {
        self.ands.is_empty() && self.ors.is_empty()
    }

    /// Only unread articles.
    pub fn unread() -> Self {
        Self {
            ands: vec![Cond::NotRead],
            ors: Vec::new(),
        }
    }

    /// Only read articles.
    pub fn read() -> Self {
        Self {
            ands: vec![Cond::Read],
            ors: Vec::new(),
        }
    }

    /// Only starred articles.
    pub fn starred() -> Self {
        Self {
            ands: vec![Cond::Favorite],
            ors: Vec::new(),
        }
    }

    /// Unread or starred — the `or_unread`/`or_starred` view FreshRSS sets on its own.
    pub fn unread_or_starred() -> Self {
        Self {
            ands: Vec::new(),
            ors: vec![Cond::NotRead, Cond::Favorite],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_admits_everything() {
        assert!(EntryState::ALL.is_unconstrained());
        assert!(EntryState::default().is_unconstrained());
        assert_eq!(EntryState::ALL.to_bits(), 0);
    }

    #[test]
    fn state_all_is_read_or_not_read_which_is_no_filter() {
        // FreshRSS's `STATE_ALL` is `READ | NOT_READ`. Asking for both is asking for everything, and
        // a port that turned it into `is_read=1 AND is_read=0` would list nothing at all.
        assert!(EntryState::from_bits(bits::ALL).is_unconstrained());
        assert!(EntryState::from_bits(bits::ALL | bits::FAVORITE | bits::NOT_FAVORITE).ands.is_empty());
    }

    #[test]
    fn a_single_bit_becomes_one_condition() {
        assert_eq!(EntryState::from_bits(bits::NOT_READ), EntryState::unread());
        assert_eq!(EntryState::from_bits(bits::READ), EntryState::read());
        assert_eq!(EntryState::from_bits(bits::FAVORITE), EntryState::starred());
        assert_eq!(
            EntryState::from_bits(bits::NOT_FAVORITE).ands,
            vec![Cond::NotFavorite]
        );
    }

    #[test]
    fn read_and_starred_compose() {
        let s = EntryState::from_bits(bits::NOT_READ | bits::FAVORITE);
        assert_eq!(s.ands, vec![Cond::NotRead, Cond::Favorite]);
        assert!(s.ors.is_empty());
    }

    #[test]
    fn the_or_bits_are_their_own_group() {
        let s = EntryState::from_bits(bits::OR_NOT_READ | bits::OR_FAVORITE);
        assert!(s.ands.is_empty());
        assert_eq!(s.ors, vec![Cond::NotRead, Cond::Favorite]);
        assert_eq!(s, EntryState::unread_or_starred());
        assert!(!s.is_unconstrained());
    }

    #[test]
    fn bits_round_trip() {
        for bits in [
            0,
            bits::READ,
            bits::NOT_READ,
            bits::FAVORITE,
            bits::NOT_FAVORITE,
            bits::NOT_READ | bits::FAVORITE,
            bits::OR_NOT_READ,
            bits::OR_FAVORITE,
            bits::OR_NOT_READ | bits::OR_FAVORITE,
        ] {
            assert_eq!(EntryState::from_bits(bits).to_bits(), bits, "{bits}");
        }
    }

    #[test]
    fn a_contradictory_pair_round_trips_to_nothing() {
        // `READ | NOT_READ` means "either", so writing it back drops both rather than inventing one.
        assert_eq!(EntryState::from_bits(bits::ALL).to_bits(), 0);
    }

    #[test]
    fn conditions_know_their_column_and_value() {
        assert_eq!(Cond::Read.column(), "is_read");
        assert_eq!(Cond::Read.value(), 1);
        assert_eq!(Cond::NotRead.column(), "is_read");
        assert_eq!(Cond::NotRead.value(), 0);
        assert_eq!(Cond::Favorite.column(), "is_favorite");
        assert_eq!(Cond::NotFavorite.value(), 0);
    }

    #[test]
    fn negation_flips_within_a_column() {
        assert_eq!(Cond::Read.negate(), Cond::NotRead);
        assert_eq!(Cond::NotRead.negate(), Cond::Read);
        assert_eq!(Cond::Favorite.negate(), Cond::NotFavorite);
        assert_eq!(Cond::NotFavorite.negate(), Cond::Favorite);
        for c in [Cond::Read, Cond::NotRead, Cond::Favorite, Cond::NotFavorite] {
            assert_eq!(c.negate().negate(), c);
            assert_eq!(c.negate().column(), c.column());
        }
    }

    #[test]
    fn an_or_group_ignores_the_ands_only_conditions() {
        // `Cond::Read` has no OR spelling in the bitmask, so it contributes nothing rather than
        // silently becoming `OR_NOT_READ`.
        let s = EntryState {
            ands: Vec::new(),
            ors: vec![Cond::Read, Cond::NotFavorite],
        };
        assert_eq!(s.to_bits(), 0);
    }
}
