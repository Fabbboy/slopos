//! The network state sequence.
//!
//! One monotonic counter, bumped on every change to the stack's configuration.
//! It exists so a client can move from a snapshot to a live event stream
//! without a gap and without the kernel replaying anything:
//!
//! 1. open a `net_monitor` fd — from this instant the fd captures,
//! 2. issue a `net_query`, whose header carries the sequence the snapshot is
//!    consistent with,
//! 3. drain the fd and discard records with `seq <= hdr.seq`.
//!
//! Anything that happened before the snapshot is already in the snapshot;
//! anything after it is in the stream; and the boundary is exact. This is the
//! same invariant a subscribed netlink socket plus `NLM_F_DUMP` relies on.
//!
//! It lives in its own module because it belongs to neither of its two users:
//! the query header reads it, the event ring stamps records with it.

use core::sync::atomic::{AtomicU64, Ordering};

/// The next sequence to hand out. Starts at 1 so the first event is numbered
/// 1 and [`net_seq`] reads 0 before anything has happened, which is what a
/// caller that has never queried should compare against.
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// The sequence the current state is consistent with, for stamping a snapshot:
/// the number of the most recently published event, or 0 before the first.
///
/// It has to be the *last published* number rather than the next one to hand
/// out, because the discard rule is `seq <= hdr.seq` — stamping a snapshot with
/// a number no event carries yet would make the client discard the first event
/// that happened after it.
#[inline]
pub fn net_seq() -> u64 {
    NEXT_SEQ.load(Ordering::Acquire).saturating_sub(1)
}

/// Claim the next sequence for an event that is about to be published.
///
/// Bumped even when nothing is subscribed, so the numbering has no holes and a
/// client cannot mistake "no subscriber existed" for "no change happened".
#[inline]
pub fn next_seq() -> u64 {
    NEXT_SEQ.fetch_add(1, Ordering::AcqRel)
}
