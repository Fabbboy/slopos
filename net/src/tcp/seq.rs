//! TCP sequence number arithmetic (RFC 793 §3.3).
//!
//! Sequence numbers are points on a circular 32-bit line: comparison is
//! wrapping subtraction reinterpreted as `i32`, so a value close to wrap
//! compares less-than one just past zero. This is **not** `<` on `u32`. The
//! [`SeqNum`] newtype routes the operators through it; the `seq_lt` family is
//! the same comparison as free functions.
//!
//! ```ignore
//! use slopos_net::tcp::seq::SeqNum;
//! assert!(SeqNum::new(0xFFFF_FFFE) < SeqNum::new(0x0000_0002));
//! ```

use core::cmp::Ordering;
use core::ops::{Add, AddAssign, Sub};

#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[inline]
pub fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

/// A TCP sequence number; `<`/`>`/`PartialOrd` route through the RFC 793
/// wrapping comparison rather than naive integer comparison.
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

    #[inline]
    pub const fn wrapping_add(self, n: u32) -> Self {
        Self(self.0.wrapping_add(n))
    }

    #[inline]
    pub const fn wrapping_sub(self, n: u32) -> Self {
        Self(self.0.wrapping_sub(n))
    }

    /// Unsigned forward distance from `self` to `other`; use [`SeqDelta`] for a
    /// signed one.
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
    /// RFC 793 defines the ordering only when `|a - b| < 2^31`; outside that
    /// range this picks the shortest arc.
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
    /// Yields the **unsigned** wrapping distance from `rhs` to `self`.
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

/// Signed difference between two [`SeqNum`]s, meaningful only when they are
/// within a half-window of each other (same caveat as RFC 793's own ordering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct SeqDelta(pub i32);

impl SeqDelta {
    #[inline]
    pub const fn of(lhs: SeqNum, rhs: SeqNum) -> Self {
        Self(lhs.0.wrapping_sub(rhs.0) as i32)
    }
}
