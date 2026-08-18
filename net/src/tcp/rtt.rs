//! Round-trip-time estimator (RFC 6298 §2).
//!
//! Sans-IO: no clock, no globals, no dependency on the rest of the TCP state
//! machine.

use super::{INITIAL_RTO_MS, MAX_RTO_MS};

/// Minimum RTO in milliseconds: Linux's `TCP_RTO_MIN` rather than RFC 6298
/// §2.4's 1 s floor, since this stack talks to a loopback / SLIRP peer.
pub const MIN_RTO_MS: u32 = 200;

/// Alpha smoothing factor for SRTT, expressed as a numerator over 8.
const ALPHA_NUM: u32 = 1;
const ALPHA_DEN: u32 = 8;

/// Beta smoothing factor for RTTVAR, expressed as a numerator over 4.
const BETA_NUM: u32 = 1;
const BETA_DEN: u32 = 4;

/// K constant from RFC 6298 §2.2.
const K: u32 = 4;

/// Clock granularity in milliseconds.
const G_MS: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RttEstimator {
    srtt_ms: u32,
    rttvar_ms: u32,
    rto_ms: u32,
    has_sample: bool,
    /// Number of consecutive retransmit timeouts without a fresh sample.
    /// Reset to 0 on each successful RTT sample.
    pub consecutive_timeouts: u8,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    /// Fresh estimator with no samples; the initial RTO is
    /// [`INITIAL_RTO_MS`] per RFC 6298 §2.1.
    pub const fn new() -> Self {
        Self {
            srtt_ms: 0,
            rttvar_ms: 0,
            rto_ms: INITIAL_RTO_MS,
            has_sample: false,
            consecutive_timeouts: 0,
        }
    }

    /// Already clamped to `[MIN_RTO_MS, MAX_RTO_MS]`.
    #[inline]
    pub const fn rto_ms(&self) -> u32 {
        self.rto_ms
    }

    #[inline]
    pub const fn srtt_ms(&self) -> u32 {
        self.srtt_ms
    }

    #[inline]
    pub const fn rttvar_ms(&self) -> u32 {
        self.rttvar_ms
    }

    #[inline]
    pub const fn has_sample(&self) -> bool {
        self.has_sample
    }

    /// Must **not** be invoked for retransmitted segments (Karn's algorithm) —
    /// use [`back_off`] for those.  A zero sample is treated as `G_MS`.
    pub fn sample(&mut self, r_ms: u32) {
        let r = r_ms.max(G_MS);
        if !self.has_sample {
            // RFC 6298 §2.2
            self.srtt_ms = r;
            self.rttvar_ms = r / 2;
            self.has_sample = true;
        } else {
            // RFC 6298 §2.3, integer-scaled; 64-bit intermediates so a
            // pathological u32 srtt/sample cannot overflow.
            let diff = if self.srtt_ms > r {
                self.srtt_ms - r
            } else {
                r - self.srtt_ms
            };
            let rttvar_next = (((BETA_DEN - BETA_NUM) as u64) * self.rttvar_ms as u64
                + (BETA_NUM as u64) * diff as u64)
                / BETA_DEN as u64;
            self.rttvar_ms = core::cmp::min(rttvar_next, u32::MAX as u64) as u32;
            let srtt_next = (((ALPHA_DEN - ALPHA_NUM) as u64) * self.srtt_ms as u64
                + (ALPHA_NUM as u64) * r as u64)
                / ALPHA_DEN as u64;
            self.srtt_ms = core::cmp::min(srtt_next, u32::MAX as u64) as u32;
        }
        self.recompute_rto();
        self.consecutive_timeouts = 0;
    }

    /// RFC 6298 §5.5: double the RTO on retransmit timeout, capped at
    /// [`MAX_RTO_MS`].  Leaves `srtt` / `rttvar` untouched (Karn's filter).
    pub fn back_off(&mut self) {
        self.rto_ms = self.rto_ms.saturating_mul(2).min(MAX_RTO_MS);
        self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
    }

    /// Reset on connection close so a reused slot does not inherit stale RTT.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    #[inline]
    fn recompute_rto(&mut self) {
        let scaled_var = self.rttvar_ms.saturating_mul(K);
        let spread = core::cmp::max(G_MS, scaled_var);
        let rto = self.srtt_ms.saturating_add(spread);
        self.rto_ms = rto.clamp(MIN_RTO_MS, MAX_RTO_MS);
    }
}
