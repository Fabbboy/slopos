//! Tests for the RFC 6298 RTT estimator (`tcp/rtt.rs`).

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::rtt::{MIN_RTO_MS, RttEstimator};

pub fn test_rtt_first_sample_seeds_srtt() -> TestResult {
    let mut e = RttEstimator::new();
    assert_test!(!e.has_sample(), "new estimator has no sample");
    e.sample(100);
    assert_test!(e.has_sample(), "has sample after first call");
    assert_eq_test!(e.srtt_ms(), 100, "SRTT := R");
    assert_eq_test!(e.rttvar_ms(), 50, "RTTVAR := R / 2");
    assert_eq_test!(e.rto_ms(), 300, "RTO = SRTT + 4*RTTVAR");
    pass!()
}

pub fn test_rtt_subsequent_samples_smooth_srtt() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(100);
    // SRTT = (7/8)*100 + (1/8)*200 = 112.
    e.sample(200);
    assert_eq_test!(e.srtt_ms(), 112, "SRTT after second sample");
    // RTTVAR = (3/4)*50 + (1/4)*|100 - 200| = 37 + 25 = 62.
    assert_eq_test!(e.rttvar_ms(), 62, "RTTVAR after second sample");
    pass!()
}

pub fn test_rtt_stable_samples_decrease_rttvar() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(100);
    let _before = e.rttvar_ms();
    for _ in 0..20 {
        e.sample(100);
    }
    assert_test!(e.rttvar_ms() < 5, "RTTVAR decays towards 0 on stable RTT");
    assert_eq_test!(e.srtt_ms(), 100, "SRTT stable");
    pass!()
}

pub fn test_rtt_rto_floor_enforced() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(1); // srtt=1, rttvar=0 after clamp → rto=1 → clamped to MIN.
    assert_eq_test!(e.rto_ms(), MIN_RTO_MS, "floor");
    pass!()
}

pub fn test_rtt_rto_ceiling_enforced() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(u32::MAX);
    assert_test!(e.rto_ms() <= crate::tcp::MAX_RTO_MS, "ceiling");
    pass!()
}

pub fn test_rtt_back_off_doubles_rto_only() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(100);
    let srtt0 = e.srtt_ms();
    let rttvar0 = e.rttvar_ms();
    let rto0 = e.rto_ms();
    e.back_off();
    assert_eq_test!(e.srtt_ms(), srtt0, "srtt unchanged");
    assert_eq_test!(e.rttvar_ms(), rttvar0, "rttvar unchanged");
    assert_eq_test!(e.rto_ms(), rto0 * 2, "rto doubled");
    pass!()
}

pub fn test_rtt_back_off_stops_at_max() -> TestResult {
    let mut e = RttEstimator::new();
    for _ in 0..32 {
        e.back_off();
    }
    assert_eq_test!(e.rto_ms(), crate::tcp::MAX_RTO_MS, "clamped to MAX_RTO_MS");
    pass!()
}

pub fn test_rtt_reset_clears_state() -> TestResult {
    let mut e = RttEstimator::new();
    e.sample(100);
    e.sample(200);
    assert_test!(e.has_sample(), "has sample pre-reset");
    e.reset();
    assert_test!(!e.has_sample(), "no sample post-reset");
    assert_eq_test!(e.srtt_ms(), 0, "srtt cleared");
    assert_eq_test!(e.rttvar_ms(), 0, "rttvar cleared");
    pass!()
}

slopos_testing::stest!(name = test_rtt_first_sample_seeds_srtt, suite = tcp_rtt);
slopos_testing::stest!(
    name = test_rtt_subsequent_samples_smooth_srtt,
    suite = tcp_rtt
);
slopos_testing::stest!(
    name = test_rtt_stable_samples_decrease_rttvar,
    suite = tcp_rtt
);
slopos_testing::stest!(name = test_rtt_rto_floor_enforced, suite = tcp_rtt);
slopos_testing::stest!(name = test_rtt_rto_ceiling_enforced, suite = tcp_rtt);
slopos_testing::stest!(name = test_rtt_back_off_doubles_rto_only, suite = tcp_rtt);
slopos_testing::stest!(name = test_rtt_back_off_stops_at_max, suite = tcp_rtt);
slopos_testing::stest!(name = test_rtt_reset_clears_state, suite = tcp_rtt);
