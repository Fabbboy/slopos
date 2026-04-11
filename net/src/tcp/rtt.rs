//! Round-trip-time estimator (RFC 6298 §2).
//!
//! Implements the Van Jacobson / Karn algorithm for computing `SRTT`,
//! `RTTVAR`, and the retransmission timeout `RTO` from observed RTT
//! samples.  The estimator is **sans-IO**: it has no clock, no globals,
//! and no dependency on the rest of the TCP state machine.  Callers feed
//! in samples via [`RttEstimator::sample`] and read the current RTO via
//! [`RttEstimator::rto_ms`].  The current TCP data path does not yet
//! consume this module — that wiring lands with the DataState refactor.
//!
//! ## The update rule
//!
//! RFC 6298 §2.2 (first sample):
//! ```text
//!     SRTT   = R
//!     RTTVAR = R / 2
//!     RTO    = SRTT + max(G, K * RTTVAR)    // K = 4
//! ```
//!
//! RFC 6298 §2.3 (subsequent samples):
//! ```text
//!     RTTVAR = (1 - β) * RTTVAR + β * |SRTT - R'|   // β = 1/4
//!     SRTT   = (1 - α) * SRTT   + α * R'            // α = 1/8
//!     RTO    = SRTT + max(G, K * RTTVAR)
//! ```
//!
//! `G` is the clock granularity (we use 1 ms), so `max(G, K*RTTVAR)` folds
//! to `max(1, 4*RTTVAR)`.
//!
//! ## Karn's algorithm
//!
//! Never sample a retransmitted segment — the ACK could be for the
//! original or the retransmit, and naively mixing the two destabilizes
//! the estimator.  Callers invoke [`RttEstimator::sample`] only for
//! freshly-acked segments; retransmits use [`RttEstimator::back_off`]
//! instead, which doubles the current RTO without touching `srtt` or
//! `rttvar`.
//!
//! ## RTO floor and ceiling
//!
//! RFC 6298 recommends a 1-second floor "if at all possible", but many
//! implementations use 200 ms for lower-latency environments (Linux's
//! `TCP_RTO_MIN`).  We match Linux here because this is a host stack
//! hitting a loopback / SLIRP peer, not a WAN client.  The ceiling is
//! 60 seconds per [`super::MAX_RTO_MS`].

use super::{INITIAL_RTO_MS, MAX_RTO_MS};

/// Minimum RTO in milliseconds.
///
/// Matches Linux's `TCP_RTO_MIN`.  Lower than RFC 6298 §2.4's suggested
/// 1 s floor, but more appropriate for fast-loopback testing.
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
    /// Smoothed RTT, in milliseconds.  `0` means "no sample yet".
    srtt_ms: u32,
    /// RTT variance, in milliseconds.
    rttvar_ms: u32,
    /// Current retransmission timeout.
    rto_ms: u32,
    has_sample: bool,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    /// Create a fresh estimator with no samples.  The initial RTO follows
    /// RFC 6298 §2.1 — it must be at least 1 second until the first
    /// measurement arrives — so we use [`INITIAL_RTO_MS`].
    pub const fn new() -> Self {
        Self {
            srtt_ms: 0,
            rttvar_ms: 0,
            rto_ms: INITIAL_RTO_MS,
            has_sample: false,
        }
    }

    /// The current RTO estimate, in milliseconds, already clamped to
    /// `[MIN_RTO_MS, MAX_RTO_MS]`.
    #[inline]
    pub const fn rto_ms(&self) -> u32 {
        self.rto_ms
    }

    /// Current smoothed RTT estimate, for diagnostics and tests.
    #[inline]
    pub const fn srtt_ms(&self) -> u32 {
        self.srtt_ms
    }

    /// Current RTT variance, for diagnostics and tests.
    #[inline]
    pub const fn rttvar_ms(&self) -> u32 {
        self.rttvar_ms
    }

    /// Has at least one sample been recorded?
    #[inline]
    pub const fn has_sample(&self) -> bool {
        self.has_sample
    }

    /// Incorporate a fresh RTT measurement `r_ms` (in milliseconds).
    ///
    /// Callers must **not** invoke this for retransmitted segments (Karn's
    /// algorithm) — use [`back_off`] for those.  A zero sample is treated
    /// as `G_MS` to avoid dividing by zero in the initial-state branch.
    pub fn sample(&mut self, r_ms: u32) {
        let r = r_ms.max(G_MS);
        if !self.has_sample {
            // RFC 6298 §2.2
            self.srtt_ms = r;
            self.rttvar_ms = r / 2;
            self.has_sample = true;
        } else {
            // RFC 6298 §2.3 — integer-scaled smoothing to avoid floats.
            // Use 64-bit intermediates so a pathological 32-bit srtt/r
            // (u32::MAX from a ceiling-check test) doesn't overflow.
            let diff = if self.srtt_ms > r {
                self.srtt_ms - r
            } else {
                r - self.srtt_ms
            };
            // RTTVAR = (1 - beta) * RTTVAR + beta * diff
            //        = (3/4) * RTTVAR + (1/4) * diff
            let rttvar_next = (((BETA_DEN - BETA_NUM) as u64) * self.rttvar_ms as u64
                + (BETA_NUM as u64) * diff as u64)
                / BETA_DEN as u64;
            self.rttvar_ms = core::cmp::min(rttvar_next, u32::MAX as u64) as u32;
            // SRTT = (1 - alpha) * SRTT + alpha * r
            //      = (7/8) * SRTT + (1/8) * r
            let srtt_next = (((ALPHA_DEN - ALPHA_NUM) as u64) * self.srtt_ms as u64
                + (ALPHA_NUM as u64) * r as u64)
                / ALPHA_DEN as u64;
            self.srtt_ms = core::cmp::min(srtt_next, u32::MAX as u64) as u32;
        }
        self.recompute_rto();
    }

    /// RFC 6298 §5.5: on retransmit timeout, double the current RTO (but
    /// never exceed [`MAX_RTO_MS`]).  Does **not** touch `srtt` / `rttvar`
    /// — those only update from fresh samples that pass Karn's filter.
    pub fn back_off(&mut self) {
        self.rto_ms = self.rto_ms.saturating_mul(2).min(MAX_RTO_MS);
    }

    /// Reset the estimator to its initial state.  Used on connection
    /// close so a reused slot doesn't inherit stale RTT state.
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
