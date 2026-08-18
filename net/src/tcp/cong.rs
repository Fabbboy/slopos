//! TCP congestion control — CUBIC (RFC 8312) + Hystart++ (RFC 9406).

use crate::tcp::seq::seq_ge;

pub trait CongestionControl {
    /// Advance the algorithm after an ACK that advanced `snd_una` by
    /// `acked_bytes`.
    fn on_ack(
        &mut self,
        acked_bytes: u32,
        rtt_sample_ms: Option<u32>,
        snd_una: u32,
        snd_nxt: u32,
        now_ms: u64,
    );

    /// The retransmission timer fired for the oldest unacknowledged segment;
    /// resets to slow start.
    fn on_timeout(&mut self, flight_size: u32);

    /// SACK-based loss detection found the first lost segment; enters recovery.
    fn on_fast_retransmit(&mut self, flight_size: u32, high_water: u32);

    /// Current congestion window in bytes.
    fn cwnd(&self) -> u32;

    /// Current slow-start threshold in bytes.
    fn ssthresh(&self) -> u32;

    fn in_recovery(&self) -> bool;
}

/// Matches [`crate::tcp::DEFAULT_MSS`].
const DEFAULT_MSS: u32 = 1460;

/// Initial congestion window in bytes: IW = 10 MSS per RFC 6928.
pub const INITIAL_CWND: u32 = 10 * DEFAULT_MSS;

/// Arbitrarily high so the very first ACK does not exit slow start.
/// RFC 5681 §3.1.
pub const INITIAL_SSTHRESH: u32 = u32::MAX;

/// Scaling constant C = 0.4 = C_NUM / C_DEN.
const C_NUM: u64 = 2;
const C_DEN: u64 = 5;

/// Multiplicative decrease factor β = 0.7 = BETA_NUM / BETA_DEN.
const BETA_NUM: u32 = 7;
const BETA_DEN: u32 = 10;

/// TCP-friendliness linear growth factor α = 3(1−β)/(1+β) ≈ 0.5294.
const TCP_ALPHA_NUM: u64 = 529;
const TCP_ALPHA_DEN: u64 = 1000;

/// Fast-convergence W_max adjustment: (1+β)/2 = 1.7/2 = 17/20.
const FAST_CONV_NUM: u32 = 17;
const FAST_CONV_DEN: u32 = 20;

/// Number of Conservative Slow Start rounds before entering congestion
/// avoidance.
const CSS_ROUNDS: u8 = 5;

const MIN_RTT_THRESH_MS: u32 = 4;

const MAX_RTT_THRESH_MS: u32 = 16;

const RTT_DIVISOR: u32 = 8;

/// Integer cube root via Newton's method.  Returns ⌊∛x⌋.
fn integer_cbrt(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    if x < 8 {
        return 1;
    }
    let bits = 64 - x.leading_zeros();
    let mut r = 1u64 << ((bits + 2) / 3);

    for _ in 0..7 {
        let r2 = r.saturating_mul(r);
        if r2 == 0 {
            break;
        }
        r = (2 * r + x / r2) / 3;
    }

    while r > 1 && r.saturating_mul(r).saturating_mul(r) > x {
        r -= 1;
    }
    while (r + 1).saturating_mul(r + 1).saturating_mul(r + 1) <= x {
        r += 1;
    }
    r
}

#[derive(Clone, Copy, Debug)]
pub struct Cubic {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,

    /// RFC 6582 recovery point.  `Some` means in recovery, which ends when
    /// `snd_una >= recover`.
    recover: Option<u32>,

    /// Epoch start (ms).  0 = not started.
    epoch_start_ms: u64,

    /// W_max: cwnd at the last loss, possibly adjusted by fast convergence,
    /// used as the origin of the cubic function.
    origin_point: u32,

    /// W_max from the loss event *before* the current one, compared against
    /// cwnd at loss time for fast convergence (RFC 8312 §4.6).
    last_max_cwnd: u32,

    /// Time (ms) from epoch start to reach `origin_point`.
    k_ms: u64,

    /// TCP-friendliness comparison window (bytes).
    tcp_cwnd: u32,

    /// `snd_nxt` saved at the start of the current measurement round; the
    /// round completes when `snd_una >= round_start`.
    round_start: u32,

    /// Minimum RTT observed in the last completed round (baseline).
    last_round_min_rtt_ms: u32,

    curr_round_min_rtt_ms: u32,

    in_css: bool,

    css_rounds: u8,

    css_baseline_rtt_ms: u32,

    /// At least one full round completed, so `last_round_min_rtt_ms` is valid.
    round_started: bool,
}

impl Cubic {
    pub const fn new(mss: u32) -> Self {
        Self {
            cwnd: 10 * mss,
            ssthresh: INITIAL_SSTHRESH,
            mss,
            recover: None,
            epoch_start_ms: 0,
            origin_point: 0,
            last_max_cwnd: 0,
            k_ms: 0,
            tcp_cwnd: 0,
            round_start: 0,
            last_round_min_rtt_ms: 0,
            curr_round_min_rtt_ms: u32::MAX,
            in_css: false,
            css_rounds: 0,
            css_baseline_rtt_ms: 0,
            round_started: false,
        }
    }

    #[inline]
    pub const fn default_mss() -> Self {
        Self::new(DEFAULT_MSS)
    }

    /// Shared loss handling for both fast retransmit and RTO.  Does **not**
    /// touch `cwnd` or `recover` — callers do that.
    fn on_loss(&mut self) {
        let cwnd_before = self.cwnd;

        // Fast convergence (RFC 8312 §4.6): a cwnd that never recovered to the
        // previous W_max means the network is degrading, so pull W_max down.
        if cwnd_before < self.last_max_cwnd {
            self.last_max_cwnd = cwnd_before;
            self.origin_point =
                (cwnd_before as u64 * FAST_CONV_NUM as u64 / FAST_CONV_DEN as u64) as u32;
        } else {
            self.last_max_cwnd = cwnd_before;
            self.origin_point = cwnd_before;
        }

        self.ssthresh = core::cmp::max(
            (self.origin_point as u64 * BETA_NUM as u64 / BETA_DEN as u64) as u32,
            2 * self.mss,
        );

        self.epoch_start_ms = 0;
        self.tcp_cwnd = 0;
    }

    /// Compute the Hystart++ RTT increase threshold (ms).
    /// `clamp(baseline / RTT_DIVISOR, MIN_RTT_THRESH, MAX_RTT_THRESH)`.
    #[inline]
    fn hystart_thresh(&self, baseline_rtt_ms: u32) -> u32 {
        (baseline_rtt_ms / RTT_DIVISOR).clamp(MIN_RTT_THRESH_MS, MAX_RTT_THRESH_MS)
    }

    /// Compute CUBIC's K: the time (ms) to grow from `ssthresh` back to
    /// `origin_point`.
    ///
    /// K = ∛( (W_max − cwnd) / C )  in seconds, converted to ms.
    ///
    /// Since `origin_point = W_max` (bytes) and `cwnd ≈ ssthresh` at
    /// epoch start:
    ///   K_ms³ = (origin_point − cwnd) * 1e9 / (C * mss)
    ///         = (origin_point − cwnd) * 5e9 / (2 * mss)
    ///
    /// We clamp `origin_point − cwnd` to 0 when cwnd ≥ origin_point
    /// (which happens after fast convergence adjustments).
    fn compute_k(&self, cwnd_at_epoch: u32) -> u64 {
        let diff = self.origin_point.saturating_sub(cwnd_at_epoch);
        if diff == 0 {
            return 0;
        }
        // K_ms = cbrt( diff * 5_000_000_000 / (2 * mss) )
        //      = cbrt( diff * 2_500_000_000 / mss )
        let numerator = diff as u64 * (C_DEN * 1_000_000_000) / (C_NUM * self.mss as u64);
        integer_cbrt(numerator)
    }

    /// Compute W_cubic(t_ms) in bytes.
    fn w_cubic(&self, t_ms: u64) -> u32 {
        // W_cubic(t) = C * (t − K)³ + origin_point   (in segments)
        // In bytes:  = origin_point + C * mss * (t − K)³ / 1e9
        //            = origin_point + 2 * mss * (t − K)³ / (5 * 1e9)
        let (delta_ms, above) = if t_ms >= self.k_ms {
            (t_ms - self.k_ms, true)
        } else {
            (self.k_ms - t_ms, false)
        };

        let dt3 = delta_ms.saturating_mul(delta_ms).saturating_mul(delta_ms);
        let offset = (C_NUM * self.mss as u64).saturating_mul(dt3) / (C_DEN * 1_000_000_000);

        let offset = core::cmp::min(offset, u32::MAX as u64) as u32;

        if above {
            self.origin_point.saturating_add(offset)
        } else {
            self.origin_point.saturating_sub(offset)
        }
    }
}

impl Default for Cubic {
    fn default() -> Self {
        Self::default_mss()
    }
}

impl CongestionControl for Cubic {
    fn on_ack(
        &mut self,
        acked_bytes: u32,
        rtt_sample_ms: Option<u32>,
        snd_una: u32,
        snd_nxt: u32,
        now_ms: u64,
    ) {
        if acked_bytes == 0 {
            return;
        }

        // Recovery exit check: if snd_una has passed the recover point,
        // exit recovery and deflate cwnd to ssthresh.  RFC 6582 §3.2.
        if let Some(recover) = self.recover {
            if seq_ge(snd_una, recover) {
                self.recover = None;
                self.cwnd = self.ssthresh;
            }
            // In recovery or just exited — don't grow cwnd this ACK.
            return;
        }

        // Update Hystart++ per-sample RTT tracking.
        if let Some(rtt) = rtt_sample_ms {
            if rtt < self.curr_round_min_rtt_ms {
                self.curr_round_min_rtt_ms = rtt;
            }
        }

        // -- Slow start (cwnd < ssthresh) -----------------------------------
        if self.cwnd < self.ssthresh {
            // Hystart++ round tracking: detect round boundary.
            if !self.round_started {
                // First ACK ever — seed the round.
                self.round_start = snd_nxt;
                self.round_started = true;
            } else if seq_ge(snd_una, self.round_start) {
                // Round completed.
                if self.last_round_min_rtt_ms != 0 && self.curr_round_min_rtt_ms != u32::MAX {
                    let thresh = self.hystart_thresh(self.last_round_min_rtt_ms);

                    if self.in_css {
                        // CSS: check if the RTT increase was a false alarm.
                        if self.curr_round_min_rtt_ms
                            < self.css_baseline_rtt_ms.saturating_add(thresh)
                        {
                            // False alarm — revert to standard SS.
                            self.in_css = false;
                            self.css_rounds = 0;
                        } else {
                            self.css_rounds += 1;
                            if self.css_rounds >= CSS_ROUNDS {
                                // CSS rounds exhausted → enter CA.
                                self.ssthresh = self.cwnd;
                                self.in_css = false;
                                self.css_rounds = 0;
                            }
                        }
                    } else if self.curr_round_min_rtt_ms
                        >= self.last_round_min_rtt_ms.saturating_add(thresh)
                    {
                        // RTT increased beyond threshold → enter CSS.
                        self.in_css = true;
                        self.css_rounds = 0;
                        self.css_baseline_rtt_ms = self.curr_round_min_rtt_ms;
                    }
                }

                // Advance to new round.
                self.last_round_min_rtt_ms = self.curr_round_min_rtt_ms;
                self.curr_round_min_rtt_ms = u32::MAX;
                self.round_start = snd_nxt;
            }

            if self.in_css {
                // Conservative Slow Start: grow linearly (≈ 1 MSS per RTT).
                let increment = self.mss.saturating_mul(self.mss) / self.cwnd.max(1);
                self.cwnd = self.cwnd.saturating_add(increment);
            } else {
                // Standard slow start: cwnd += min(acked, MSS).
                let growth = core::cmp::min(acked_bytes, self.mss);
                self.cwnd = self.cwnd.saturating_add(growth);
            }
            return;
        }

        // -- Congestion avoidance (cwnd >= ssthresh) — CUBIC ----------------

        // Reset Hystart state on CA entry.
        self.in_css = false;
        self.css_rounds = 0;

        // Start a new CUBIC epoch if needed.
        if self.epoch_start_ms == 0 {
            self.epoch_start_ms = now_ms;
            self.k_ms = self.compute_k(self.cwnd);
            self.tcp_cwnd = self.cwnd;
        }

        let t_ms = now_ms.saturating_sub(self.epoch_start_ms);
        let w_cubic = self.w_cubic(t_ms);

        // TCP friendliness (RFC 8312 §4.3): linear Reno-equivalent growth.
        // tcp_cwnd += α * mss * acked_bytes / cwnd
        let tcp_inc = TCP_ALPHA_NUM * self.mss as u64 * acked_bytes as u64
            / (TCP_ALPHA_DEN * self.cwnd.max(1) as u64);
        self.tcp_cwnd = self.tcp_cwnd.saturating_add(tcp_inc as u32);

        let target = core::cmp::max(w_cubic, self.tcp_cwnd);

        if target > self.cwnd {
            // Per-ACK fractional increase toward target.
            let diff = target - self.cwnd;
            let inc = (diff as u64 * self.mss as u64 / self.cwnd.max(1) as u64) as u32;
            self.cwnd = self.cwnd.saturating_add(inc.max(1));
        }
    }

    fn on_timeout(&mut self, _flight_size: u32) {
        self.on_loss();
        self.cwnd = self.mss;
        self.recover = None;
        // Reset Hystart state — start fresh after RTO.
        self.in_css = false;
        self.css_rounds = 0;
        self.round_started = false;
        self.curr_round_min_rtt_ms = u32::MAX;
    }

    fn on_fast_retransmit(&mut self, _flight_size: u32, high_water: u32) {
        self.on_loss();
        self.cwnd = self.ssthresh;
        self.recover = Some(high_water);
    }

    #[inline]
    fn cwnd(&self) -> u32 {
        self.cwnd
    }

    #[inline]
    fn ssthresh(&self) -> u32 {
        self.ssthresh
    }

    #[inline]
    fn in_recovery(&self) -> bool {
        self.recover.is_some()
    }
}

// ---------------------------------------------------------------------------
// Pluggable algorithm enum (no Box<dyn>, no_std-friendly)
// ---------------------------------------------------------------------------

/// Zero-allocation dispatch over the supported CC algorithms.  Wires into
/// the data path as a single field on the connection state block.
#[derive(Clone, Copy, Debug)]
pub enum CcAlgo {
    Cubic(Cubic),
}

impl CcAlgo {
    pub const fn cubic(mss: u32) -> Self {
        Self::Cubic(Cubic::new(mss))
    }
}

impl Default for CcAlgo {
    fn default() -> Self {
        Self::Cubic(Cubic::default())
    }
}

impl CongestionControl for CcAlgo {
    fn on_ack(
        &mut self,
        acked_bytes: u32,
        rtt_sample_ms: Option<u32>,
        snd_una: u32,
        snd_nxt: u32,
        now_ms: u64,
    ) {
        match self {
            Self::Cubic(c) => c.on_ack(acked_bytes, rtt_sample_ms, snd_una, snd_nxt, now_ms),
        }
    }
    fn on_timeout(&mut self, flight_size: u32) {
        match self {
            Self::Cubic(c) => c.on_timeout(flight_size),
        }
    }
    fn on_fast_retransmit(&mut self, flight_size: u32, high_water: u32) {
        match self {
            Self::Cubic(c) => c.on_fast_retransmit(flight_size, high_water),
        }
    }
    fn cwnd(&self) -> u32 {
        match self {
            Self::Cubic(c) => c.cwnd(),
        }
    }
    fn ssthresh(&self) -> u32 {
        match self {
            Self::Cubic(c) => c.ssthresh(),
        }
    }
    fn in_recovery(&self) -> bool {
        match self {
            Self::Cubic(c) => c.in_recovery(),
        }
    }
}
