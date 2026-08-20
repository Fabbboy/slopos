//! Segment acceptability, RST validation and challenge ACK rate limiting
//! (RFC 793 §3.9, RFC 5961).
//!
//! An in-window RST whose `seg.seq` is not exactly `rcv_nxt` is answered with a
//! bare ACK carrying `rcv_nxt` instead of being accepted, so an off-path
//! injector cannot tear the connection down; a per-connection per-epoch cap
//! keeps that reply from becoming an amplification vector, and keeps one
//! connection's budget from reporting on another's window (CVE-2016-5696).

use super::seq::{seq_ge, seq_gt, seq_lt};

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

/// RFC 793 §3.9 segment acceptability.
///
/// `seg_len` counts the SYN and FIN control bits the segment carries, since
/// each occupies a sequence position.
pub fn segment_acceptable(seg_seq: u32, seg_len: u32, rcv_nxt: u32, rcv_wnd: u32) -> bool {
    let in_window = |s: u32| seq_ge(s, rcv_nxt) && seq_lt(s, rcv_nxt.wrapping_add(rcv_wnd));

    match (seg_len, rcv_wnd) {
        (0, 0) => seg_seq == rcv_nxt,
        (0, _) => in_window(seg_seq),
        // A zero receive window accepts no data, only a pure ACK probing it.
        (_, 0) => false,
        (len, _) => in_window(seg_seq) || in_window(seg_seq.wrapping_add(len).wrapping_sub(1)),
    }
}

/// Per-epoch cap. RFC 5961 §7 leaves the value to the implementer; the count
/// is per connection, not global, so probing one connection's budget says
/// nothing about another's receive window.
const CHALLENGE_ACK_LIMIT: u32 = 1000;

const EPOCH_MS: u64 = 1000;

/// Per-connection challenge-ACK budget.
///
/// The jitter is drawn per epoch so the exact count at which replies stop is
/// not a constant an observer can calibrate against.
#[derive(Clone, Copy, Debug)]
pub struct ChallengeBudget {
    epoch_start_ms: u64,
    count: u32,
    limit: u32,
    started: bool,
}

impl ChallengeBudget {
    pub const fn new() -> Self {
        Self {
            epoch_start_ms: 0,
            count: 0,
            limit: CHALLENGE_ACK_LIMIT,
            started: false,
        }
    }

    /// `false` when the epoch's budget is spent and the segment must be
    /// dropped unanswered instead.
    pub fn try_consume(&mut self, now_ms: u64) -> bool {
        if !self.started || now_ms >= self.epoch_start_ms.wrapping_add(EPOCH_MS) {
            self.epoch_start_ms = now_ms;
            self.count = 1;
            self.limit = jittered_limit();
            self.started = true;
            return true;
        }

        let prev = self.count;
        self.count = self.count.saturating_add(1);
        prev < self.limit
    }
}

impl Default for ChallengeBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Half the nominal cap plus a random half, so the effective limit lands in
/// `[LIMIT/2, LIMIT)` and moves every epoch.
fn jittered_limit() -> u32 {
    let half = CHALLENGE_ACK_LIMIT / 2;
    let r = (slopos_kernel_services::platform::rng_next() % (half as u64)) as u32;
    half + r
}
