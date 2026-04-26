//! RST validation and challenge ACK rate limiting (RFC 5961).
//!
//! RFC 5961 hardens TCP against off-path RST injection by requiring
//! sequence-number validation on incoming RST segments.  If the RST's
//! `seg.seq` is within the receive window but not exactly `rcv_nxt`, the
//! stack sends a "challenge ACK" — a bare ACK carrying `rcv_nxt` — so
//! the legitimate peer can retransmit a correctly-sequenced RST if the
//! connection really is dead.  A rate limiter bounds challenge ACK
//! emission to prevent the mechanism itself from becoming an
//! amplification vector.
//!
//! ## Classification
//!
//! [`classify_rst`] maps `(seg_seq, rcv_nxt, window)` to one of three
//! actions:
//!
//! - **Accept** — `seg_seq == rcv_nxt`.  The RST is exact-match;
//!   tear the connection down immediately.
//! - **ChallengeAck** — `rcv_nxt < seg_seq < rcv_nxt + window`
//!   (wrapping).  The RST is in-window but not exact; emit a challenge
//!   ACK and keep the connection alive.
//! - **Drop** — outside the window entirely.  Silently discard.
//!
//! ## Rate limiting
//!
//! [`try_challenge_ack`] enforces a global per-epoch cap on challenge
//! ACKs (default 1 000 per second, matching Linux's
//! `tcp_challenge_ack_limit`).  When the cap is exceeded, in-window
//! RSTs are silently dropped rather than answered.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::seq::{seq_gt, seq_lt};

// =============================================================================
// RST classification
// =============================================================================

/// Action to take for an incoming RST segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RstAction {
    /// `seg_seq == rcv_nxt`: accept the RST, tear down the connection.
    Accept,
    /// In-window but not exact: emit a challenge ACK, keep the
    /// connection alive.
    ChallengeAck,
    /// Outside the receive window: silently drop.
    Drop,
}

/// Classify an incoming RST's sequence number against the receive window.
///
/// `window` is the **effective** receive window in bytes (already scaled
/// if window scaling is active).  Callers must apply the shift before
/// calling.
///
/// Edge cases:
/// - `window == 0`: only exact-match RSTs are accepted; everything
///   else is dropped.  This is correct — a zero-window connection has
///   no room for in-window but non-exact RSTs.
/// - Sequence-space wrap: handled by [`seq_gt`] / [`seq_lt`], which
///   compare via `i32` reinterpretation.
pub fn classify_rst(seg_seq: u32, rcv_nxt: u32, window: u32) -> RstAction {
    if seg_seq == rcv_nxt {
        return RstAction::Accept;
    }
    // In-window: rcv_nxt < seg_seq < rcv_nxt + window (wrapping).
    if window > 0 && seq_gt(seg_seq, rcv_nxt) && seq_lt(seg_seq, rcv_nxt.wrapping_add(window)) {
        return RstAction::ChallengeAck;
    }
    RstAction::Drop
}

// =============================================================================
// Challenge ACK rate limiter
// =============================================================================

/// Maximum challenge ACKs per epoch.
const CHALLENGE_ACK_LIMIT: u32 = 1000;

/// Epoch duration in milliseconds.
const EPOCH_MS: u64 = 1000;

/// Start of the current epoch (monotonic ms).  A value of `0` means
/// "first call initializes the epoch".
static EPOCH_START: AtomicU64 = AtomicU64::new(0);

/// Number of challenge ACKs sent so far in the current epoch.
static CHALLENGE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Try to acquire a challenge ACK token.
///
/// Returns `true` if a challenge ACK may be sent; `false` if the
/// per-epoch rate limit has been reached and the RST should be
/// silently dropped instead.
pub fn try_challenge_ack(now_ms: u64) -> bool {
    let epoch = EPOCH_START.load(Ordering::Relaxed);

    // New epoch: reset the counter.
    if now_ms >= epoch.wrapping_add(EPOCH_MS) || epoch == 0 {
        // CAS to claim the epoch transition.  If another caller races
        // us, one of us wins — both outcomes are correct.
        let _ = EPOCH_START.compare_exchange(epoch, now_ms, Ordering::Relaxed, Ordering::Relaxed);
        CHALLENGE_COUNT.store(1, Ordering::Relaxed);
        return true;
    }

    let prev = CHALLENGE_COUNT.fetch_add(1, Ordering::Relaxed);
    prev < CHALLENGE_ACK_LIMIT
}

/// Reset the rate limiter to its initial state.  Called by
/// [`super::reset_all`] so tests see a clean epoch.
#[cfg(feature = "test-hooks")]
pub fn reset_for_tests() {
    EPOCH_START.store(0, Ordering::Relaxed);
    CHALLENGE_COUNT.store(0, Ordering::Relaxed);
}
