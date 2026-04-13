//! TCP congestion control.
//!
//! Provides a [`CongestionControl`] trait and a default [`NewReno`]
//! implementation per RFC 5681 + RFC 6582.
//!
//! ## Algorithm summary
//!
//! - **Slow start**: starts at `cwnd = IW` (initial window, typically 10
//!   MSS per RFC 6928) and doubles `cwnd` every RTT (i.e. `cwnd += MSS`
//!   for every ACKed byte) until `cwnd >= ssthresh`.
//! - **Congestion avoidance**: grows `cwnd` linearly by roughly one MSS
//!   per RTT (we track a residual byte counter to approximate this on
//!   per-ACK granularity — `cwnd += (MSS*MSS)/cwnd` rounded).
//! - **Fast retransmit / fast recovery** (RFC 5681 §3.2, RFC 6582): SACK-
//!   based loss detection triggers an immediate retransmit of lost segments;
//!   `ssthresh = max(FlightSize/2, 2*MSS)`, `cwnd = ssthresh + 3*MSS`,
//!   and the connection enters "recovery" until `snd_una` passes the
//!   RECOVER point.
//! - **RTO timeout**: `ssthresh = max(FlightSize/2, 2*MSS)`, `cwnd = MSS`,
//!   drop back to slow start.  RTT estimator separately applies Karn's
//!   back-off.

use crate::tcp::seq::seq_ge;

/// Trait every congestion-control algorithm must implement.
///
/// The state machine holds the trait object as an enum variant (see
/// [`CcAlgo`]) so no heap allocation is needed.
pub trait CongestionControl {
    /// Advance the algorithm after an ACK that advanced `snd_una` by
    /// `acked_bytes`.  `rtt_sample_ms` is `Some(r)` when the ACK yields
    /// an eligible RTT sample.  `snd_una` is the new cumulative ACK
    /// point — used to detect recovery exit.
    fn on_ack(&mut self, acked_bytes: u32, rtt_sample_ms: Option<u32>, snd_una: u32);

    /// Called when the retransmission timer fires for the oldest
    /// unacknowledged segment.  Resets to slow start.
    fn on_timeout(&mut self, flight_size: u32);

    /// Called when SACK-based loss detection finds the first lost segment.
    /// Halves cwnd, sets the recovery high-water mark, and enters recovery.
    fn on_fast_retransmit(&mut self, flight_size: u32, high_water: u32);

    /// Current congestion window in bytes.
    fn cwnd(&self) -> u32;

    /// Current slow-start threshold in bytes.
    fn ssthresh(&self) -> u32;

    /// Are we currently in the fast-recovery phase?
    fn in_recovery(&self) -> bool;
}

/// Maximum segment size used for the default NewReno instance.  Matches
/// [`crate::tcp::DEFAULT_MSS`].
const DEFAULT_MSS: u32 = 1460;

/// Initial congestion window in bytes.  RFC 6928 recommends IW = 10 MSS
/// for modern loss-resilient stacks.
pub const INITIAL_CWND: u32 = 10 * DEFAULT_MSS;

/// Initial slow-start threshold — start arbitrarily high so the very
/// first ACK doesn't immediately exit slow start.  RFC 5681 §3.1.
pub const INITIAL_SSTHRESH: u32 = u32::MAX;

// -----------------------------------------------------------------------------
// NewReno
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct NewReno {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,
    /// When `Some(seq)`, we are in recovery.  Recovery ends when
    /// `snd_una >= recover`.  RFC 6582's `recover` variable.
    recover: Option<u32>,
    /// Residual bytes counter for fractional congestion-avoidance growth.
    /// Accumulates `(MSS*MSS)/cwnd` every ACK until it ticks over `mss`.
    ca_residual: u32,
}

impl NewReno {
    pub const fn new(mss: u32) -> Self {
        Self {
            cwnd: 10 * mss,
            ssthresh: INITIAL_SSTHRESH,
            mss,
            recover: None,
            ca_residual: 0,
        }
    }

    #[inline]
    pub const fn default_mss() -> Self {
        Self::new(DEFAULT_MSS)
    }
}

impl Default for NewReno {
    fn default() -> Self {
        Self::default_mss()
    }
}

impl CongestionControl for NewReno {
    fn on_ack(&mut self, acked_bytes: u32, _rtt_sample_ms: Option<u32>, snd_una: u32) {
        if acked_bytes == 0 {
            return;
        }

        // Recovery exit check: if snd_una has passed the recover point,
        // exit recovery and deflate cwnd to ssthresh.  RFC 6582 §3.2.
        if let Some(recover) = self.recover {
            if seq_ge(snd_una, recover) {
                self.recover = None;
                self.cwnd = self.ssthresh;
                // Fall through to normal cwnd growth below.
            } else {
                // Still in recovery — don't grow cwnd.
                return;
            }
        }

        if self.cwnd < self.ssthresh {
            // Slow start: cwnd += MSS per MSS-ish of ACK'd data.
            let growth = core::cmp::min(acked_bytes, self.mss);
            self.cwnd = self.cwnd.saturating_add(growth);
        } else {
            // Congestion avoidance: cwnd += (MSS*MSS)/cwnd per ACK.
            let increment = self.mss.saturating_mul(self.mss) / self.cwnd.max(1);
            self.ca_residual = self.ca_residual.saturating_add(increment);
            while self.ca_residual >= self.mss {
                self.cwnd = self.cwnd.saturating_add(self.mss);
                self.ca_residual -= self.mss;
            }
        }
    }

    fn on_timeout(&mut self, flight_size: u32) {
        self.ssthresh = core::cmp::max(flight_size / 2, 2 * self.mss);
        self.cwnd = self.mss;
        self.recover = None;
        self.ca_residual = 0;
    }

    fn on_fast_retransmit(&mut self, flight_size: u32, high_water: u32) {
        self.ssthresh = core::cmp::max(flight_size / 2, 2 * self.mss);
        self.cwnd = self.ssthresh.saturating_add(3 * self.mss);
        self.recover = Some(high_water);
        self.ca_residual = 0;
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

// -----------------------------------------------------------------------------
// Pluggable algorithm enum (no Box<dyn>, no_std-friendly)
// -----------------------------------------------------------------------------

/// Zero-allocation dispatch over the supported CC algorithms.  Wires into
/// the data path as a single field on the connection state block.
#[derive(Clone, Copy, Debug)]
pub enum CcAlgo {
    NewReno(NewReno),
}

impl CcAlgo {
    pub const fn new_reno(mss: u32) -> Self {
        Self::NewReno(NewReno::new(mss))
    }
}

impl Default for CcAlgo {
    fn default() -> Self {
        Self::NewReno(NewReno::default())
    }
}

impl CongestionControl for CcAlgo {
    fn on_ack(&mut self, acked_bytes: u32, rtt_sample_ms: Option<u32>, snd_una: u32) {
        match self {
            Self::NewReno(r) => r.on_ack(acked_bytes, rtt_sample_ms, snd_una),
        }
    }
    fn on_timeout(&mut self, flight_size: u32) {
        match self {
            Self::NewReno(r) => r.on_timeout(flight_size),
        }
    }
    fn on_fast_retransmit(&mut self, flight_size: u32, high_water: u32) {
        match self {
            Self::NewReno(r) => r.on_fast_retransmit(flight_size, high_water),
        }
    }
    fn cwnd(&self) -> u32 {
        match self {
            Self::NewReno(r) => r.cwnd(),
        }
    }
    fn ssthresh(&self) -> u32 {
        match self {
            Self::NewReno(r) => r.ssthresh(),
        }
    }
    fn in_recovery(&self) -> bool {
        match self {
            Self::NewReno(r) => r.in_recovery(),
        }
    }
}
