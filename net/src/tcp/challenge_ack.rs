//! RST validation and challenge ACK rate limiting (RFC 5961).
//!
//! An in-window RST whose `seg.seq` is not exactly `rcv_nxt` is answered with a
//! bare ACK carrying `rcv_nxt` instead of being accepted, so an off-path
//! injector cannot tear the connection down; a per-epoch cap keeps that reply
//! from becoming an amplification vector.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::seq::{seq_gt, seq_lt};

/// Action to take for an incoming RST segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RstAction {
    /// `seg_seq == rcv_nxt`; tear the connection down.
    Accept,
    /// In-window but not exact; keep the connection alive.
    ChallengeAck,
    /// Outside the receive window.
    Drop,
}

/// Classify an incoming RST's sequence number against the receive window.
///
/// `window` is the **effective** receive window in bytes; callers apply the
/// window scale before calling. With `window == 0` only an exact match is
/// accepted, a zero-window connection having no room for anything else.
pub fn classify_rst(seg_seq: u32, rcv_nxt: u32, window: u32) -> RstAction {
    if seg_seq == rcv_nxt {
        return RstAction::Accept;
    }
    if window > 0 && seq_gt(seg_seq, rcv_nxt) && seq_lt(seg_seq, rcv_nxt.wrapping_add(window)) {
        return RstAction::ChallengeAck;
    }
    RstAction::Drop
}

/// Per-epoch cap, matching Linux's `tcp_challenge_ack_limit` default.
const CHALLENGE_ACK_LIMIT: u32 = 1000;

const EPOCH_MS: u64 = 1000;

/// Monotonic ms; `0` means the next call initializes the epoch.
static EPOCH_START: AtomicU64 = AtomicU64::new(0);

static CHALLENGE_COUNT: AtomicU32 = AtomicU32::new(0);

/// `false` when the per-epoch cap is reached and the RST must be dropped
/// unanswered instead.
pub fn try_challenge_ack(now_ms: u64) -> bool {
    let epoch = EPOCH_START.load(Ordering::Relaxed);

    if now_ms >= epoch.wrapping_add(EPOCH_MS) || epoch == 0 {
        // A racing caller may win the CAS instead; either epoch start is correct.
        let _ = EPOCH_START.compare_exchange(epoch, now_ms, Ordering::Relaxed, Ordering::Relaxed);
        CHALLENGE_COUNT.store(1, Ordering::Relaxed);
        return true;
    }

    let prev = CHALLENGE_COUNT.fetch_add(1, Ordering::Relaxed);
    prev < CHALLENGE_ACK_LIMIT
}

/// Reset the rate limiter; called by [`super::reset_all`].
#[cfg(feature = "test-hooks")]
pub fn reset_for_tests() {
    EPOCH_START.store(0, Ordering::Relaxed);
    CHALLENGE_COUNT.store(0, Ordering::Relaxed);
}
