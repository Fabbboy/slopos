//! TCP sequence number arithmetic (RFC 793 §3.3).
//!
//! Provides wrapping comparison helpers that treat 32-bit sequence numbers as
//! points on a circular number line.  A later phase upgrades these into a
//! `SeqNum` newtype so comparisons can't accidentally fall back to `<`/`>`
//! on bare `u32`, but the free-function form preserves source compatibility
//! with every existing call site.

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
