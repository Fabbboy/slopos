//! NewReno congestion control tests.
//!
//! Drives the algorithm directly — no TCP state machine, no data path.
//! Asserts per-RFC 5681 / RFC 6582 behavior for slow start, congestion
//! avoidance, fast retransmit, and RTO timeout transitions.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::cong::{CcAlgo, CongestionControl, INITIAL_CWND, NewReno};

const MSS: u32 = 1460;

// -----------------------------------------------------------------------------
// Slow start
// -----------------------------------------------------------------------------

pub fn test_newreno_initial_cwnd_is_iw10() -> TestResult {
    let r = NewReno::default();
    assert_eq_test!(r.cwnd(), INITIAL_CWND, "IW = 10 * MSS");
    assert_test!(r.ssthresh() > r.cwnd(), "ssthresh starts high");
    assert_test!(!r.in_recovery(), "not in recovery at birth");
    pass!()
}

/// Each ACK in slow start grows cwnd by min(acked, MSS).
pub fn test_newreno_slow_start_grows_per_ack() -> TestResult {
    let mut r = NewReno::default();
    let c0 = r.cwnd();
    r.on_ack(MSS, Some(50));
    assert_eq_test!(r.cwnd(), c0 + MSS, "cwnd grew by MSS");
    r.on_ack(500, Some(50));
    assert_eq_test!(r.cwnd(), c0 + MSS + 500, "cwnd grew by partial ACK");
    pass!()
}

/// Slow start saturates when cwnd reaches ssthresh.
pub fn test_newreno_exits_slow_start_at_ssthresh() -> TestResult {
    let mut r = NewReno::new(MSS);
    // Artificially pin ssthresh via an RTO back-off.
    r.on_timeout(10 * MSS); // ssthresh = max(10MSS/2, 2MSS) = 5*MSS, cwnd=MSS
    assert_eq_test!(r.ssthresh(), 5 * MSS, "ssthresh = FlightSize/2");
    assert_eq_test!(r.cwnd(), MSS, "cwnd resets to 1 MSS on timeout");

    // Now grow cwnd in slow start until it reaches ssthresh.
    let ssthresh = r.ssthresh();
    while r.cwnd() < ssthresh {
        r.on_ack(MSS, Some(50));
    }
    // Next ACK enters congestion avoidance — cwnd increment becomes
    // fractional, not a full MSS.
    let before = r.cwnd();
    r.on_ack(MSS, Some(50));
    let after = r.cwnd();
    assert_test!(after < before + MSS, "CA grows sublinearly");
    pass!()
}

// -----------------------------------------------------------------------------
// Congestion avoidance
// -----------------------------------------------------------------------------

/// Roughly 1 MSS growth per RTT: receiving `cwnd / MSS` acks of MSS each
/// grows cwnd by approximately 1 MSS.
pub fn test_newreno_ca_grows_one_mss_per_rtt() -> TestResult {
    let mut r = NewReno::new(MSS);
    r.on_timeout(10 * MSS); // ssthresh = 5MSS, cwnd = MSS
    // Skip slow start: crank ssthresh down by doing another timeout.
    r.on_timeout(2 * MSS); // ssthresh = max(MSS, 2MSS) = 2MSS, cwnd = MSS
    // Grow cwnd past ssthresh in slow start first.
    while r.cwnd() < r.ssthresh() {
        r.on_ack(MSS, Some(50));
    }
    let before_ca = r.cwnd();
    let acks_per_rtt = before_ca / MSS;
    // Fire `acks_per_rtt` back-to-back ACKs.
    for _ in 0..acks_per_rtt {
        r.on_ack(MSS, Some(50));
    }
    let after = r.cwnd();
    let growth = after - before_ca;
    // Allow some fractional slack — the residual accumulator can be off
    // by a few bytes at this scale.
    assert_test!(
        growth >= MSS / 2 && growth <= MSS + MSS / 2,
        "approximately one MSS per RTT"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// Fast retransmit / fast recovery
// -----------------------------------------------------------------------------

pub fn test_newreno_dup_ack_counter_increments() -> TestResult {
    let mut r = NewReno::new(MSS);
    assert_eq_test!(r.dup_acks(), 0, "starts at 0");
    r.on_dup_ack();
    r.on_dup_ack();
    assert_eq_test!(r.dup_acks(), 2, "increments on dup");
    r.on_ack(MSS, None);
    assert_eq_test!(r.dup_acks(), 0, "resets on forward progress");
    pass!()
}

pub fn test_newreno_fast_retransmit_halves_cwnd() -> TestResult {
    let mut r = NewReno::new(MSS);
    // Push cwnd up so halving is observable.
    for _ in 0..20 {
        r.on_ack(MSS, Some(50));
    }
    let flight_size = r.cwnd();
    r.on_fast_retransmit(flight_size, 100_000);
    // ssthresh = max(FlightSize/2, 2*MSS)
    let expected_ssthresh = core::cmp::max(flight_size / 2, 2 * MSS);
    assert_eq_test!(r.ssthresh(), expected_ssthresh, "ssthresh halved");
    // cwnd = ssthresh + 3*MSS (inflated)
    assert_eq_test!(r.cwnd(), expected_ssthresh + 3 * MSS, "cwnd inflated");
    assert_test!(r.in_recovery(), "recovery flag set");
    pass!()
}

pub fn test_newreno_timeout_resets_to_one_mss() -> TestResult {
    let mut r = NewReno::new(MSS);
    for _ in 0..10 {
        r.on_ack(MSS, Some(50));
    }
    r.on_timeout(r.cwnd());
    assert_eq_test!(r.cwnd(), MSS, "cwnd = MSS after timeout");
    assert_test!(r.ssthresh() >= 2 * MSS, "ssthresh at least 2*MSS");
    assert_test!(!r.in_recovery(), "recovery cleared on timeout");
    pass!()
}

// -----------------------------------------------------------------------------
// CcAlgo enum dispatch
// -----------------------------------------------------------------------------

pub fn test_cc_algo_enum_dispatches_to_newreno() -> TestResult {
    let mut algo = CcAlgo::new_reno(MSS);
    let c0 = algo.cwnd();
    algo.on_ack(MSS, Some(10));
    assert_eq_test!(algo.cwnd(), c0 + MSS, "enum dispatches to NewReno");
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::define_test_suite!(
    tcp_cong_newreno,
    [
        test_newreno_initial_cwnd_is_iw10,
        test_newreno_slow_start_grows_per_ack,
        test_newreno_exits_slow_start_at_ssthresh,
        test_newreno_ca_grows_one_mss_per_rtt,
        test_newreno_dup_ack_counter_increments,
        test_newreno_fast_retransmit_halves_cwnd,
        test_newreno_timeout_resets_to_one_mss,
        test_cc_algo_enum_dispatches_to_newreno,
    ]
);
