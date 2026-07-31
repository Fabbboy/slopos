//! TCP sequence number arithmetic (RFC 793 §3.3).
//!
//! Provides wrapping comparison helpers that treat 32-bit sequence numbers as
//! points on a circular number line, plus a strongly-typed [`SeqNum`] newtype
//! for call sites that want the extra safety.  The free-function `seq_lt`
//! family is retained for source compatibility with the existing state
//! machine — rewriting every use site is a P3-era concern, not the purpose
//! of this module.
//!
//! ## Wrapping comparison (RFC 793 §3.3)
//!
//! TCP sequence space is a 32-bit unsigned counter that wraps.  Two sequence
//! numbers are compared as if they lay on a circle: the "distance" between
//! them is computed via wrapping subtraction and reinterpreted as `i32`, so
//! a value "close to wrap" compares less-than a value just past zero.  This
//! is **not** the same as `<` on `u32`; tests frequently catch bugs where a
//! helper accidentally uses the latter.
//!
//! ```ignore
//! use slopos_net::tcp::seq::SeqNum;
//! assert!(SeqNum::new(0xFFFF_FFFE) < SeqNum::new(0x0000_0002));
//! ```

use core::cmp::Ordering;
use core::ops::{Add, AddAssign, Sub};

// -----------------------------------------------------------------------------
// Free-function form (legacy callers)
// -----------------------------------------------------------------------------

/// `a` is before `b` in sequence space (wrapping comparison).
#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// `a` is before or equal to `b` in sequence space.
#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// `a` is after `b` in sequence space.
#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// `a` is after or equal to `b` in sequence space.
#[inline]
pub fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

// -----------------------------------------------------------------------------
// Newtype form
// -----------------------------------------------------------------------------

/// A TCP sequence number.  Wraps a `u32` so that `<`/`>`/`PartialOrd` route
/// through the RFC 793 wrapping comparison instead of naive integer
/// comparison.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, slopos_ostd::Zeroable)]
#[repr(transparent)]
pub struct SeqNum(pub u32);

impl SeqNum {
    pub const ZERO: Self = Self(0);

    #[inline]
    pub const fn new(v: u32) -> Self {
        Self(v)
    }

    /// The underlying `u32` in host byte order.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Wrapping addition — returns `self + n` in 32-bit sequence space.
    #[inline]
    pub const fn wrapping_add(self, n: u32) -> Self {
        Self(self.0.wrapping_add(n))
    }

    /// Wrapping subtraction — returns `self - n` in 32-bit sequence space.
    #[inline]
    pub const fn wrapping_sub(self, n: u32) -> Self {
        Self(self.0.wrapping_sub(n))
    }

    /// Unsigned distance from `self` to `other` in sequence space, treating
    /// the result as a positive increment.  Equivalent to
    /// `other.0.wrapping_sub(self.0)` — callers that need a signed delta
    /// should use [`SeqDelta`].
    #[inline]
    pub const fn distance_to(self, other: SeqNum) -> u32 {
        other.0.wrapping_sub(self.0)
    }
}

impl PartialOrd for SeqNum {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SeqNum {
    /// Total-ordering sequence number comparison **within a half-window**.
    ///
    /// RFC 793 only defines the ordering when `|a - b| < 2^31`.  Outside
    /// that range the result is undefined (and any implementation choice
    /// is acceptable).  We adopt "shortest-arc" semantics: reinterpret the
    /// wrapping delta as `i32` and compare to zero.
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        let delta = self.0.wrapping_sub(other.0) as i32;
        delta.cmp(&0)
    }
}

impl Add<u32> for SeqNum {
    type Output = SeqNum;
    #[inline]
    fn add(self, rhs: u32) -> SeqNum {
        self.wrapping_add(rhs)
    }
}

impl AddAssign<u32> for SeqNum {
    #[inline]
    fn add_assign(&mut self, rhs: u32) {
        self.0 = self.0.wrapping_add(rhs);
    }
}

impl Sub<SeqNum> for SeqNum {
    type Output = u32;
    /// `a - b` yields the **unsigned** wrapping distance from `b` to `a`.
    /// Callers that need a signed delta should compute it themselves via
    /// `(a.0.wrapping_sub(b.0)) as i32`.
    #[inline]
    fn sub(self, rhs: SeqNum) -> u32 {
        self.0.wrapping_sub(rhs.0)
    }
}

impl From<u32> for SeqNum {
    #[inline]
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<SeqNum> for u32 {
    #[inline]
    fn from(s: SeqNum) -> Self {
        s.0
    }
}

// -----------------------------------------------------------------------------
// Signed delta helper
// -----------------------------------------------------------------------------

/// Signed difference between two [`SeqNum`]s, in the range `[-2^31, 2^31)`.
///
/// Only defined when the two values are within a half-window of each other;
/// outside that range the result wraps and becomes meaningless (same caveat
/// as RFC 793's own ordering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct SeqDelta(pub i32);

impl SeqDelta {
    #[inline]
    pub const fn of(lhs: SeqNum, rhs: SeqNum) -> Self {
        Self(lhs.0.wrapping_sub(rhs.0) as i32)
    }
}
