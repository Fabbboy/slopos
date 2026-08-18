//! The network state sequence: one monotonic counter, bumped on every change to
//! the stack's configuration.
//!
//! A client opens a `net_monitor` fd, stamps a `net_query` snapshot with the
//! sequence, and discards records with `seq <= hdr.seq`; the snapshot/stream
//! boundary is then exact with no replay.

use core::sync::atomic::{AtomicU64, Ordering};

/// The next sequence to hand out. Starts at 1 so [`net_seq`] reads 0 before
/// anything has happened.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// The sequence a snapshot is stamped with: the number of the most recently
/// published event, or 0 before the first.
///
/// Last-published rather than next-to-hand-out, because the discard rule is
/// `seq <= hdr.seq`.
#[inline]
pub fn net_seq() -> u64 {
    NEXT_SEQ.load(Ordering::Acquire).saturating_sub(1)
}

/// Claim the next sequence for an event that is about to be published.
///
/// Bumped even when nothing is subscribed, so the numbering has no holes.
#[inline]
pub fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::AcqRel)
}
