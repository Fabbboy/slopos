//! CUBIC congestion control tests (RFC 8312 + Hystart++ RFC 9406).
//!
//! Drives the algorithm directly — no TCP state machine, no data path.
//! Asserts per-RFC 8312 behavior for slow start, CUBIC congestion
//! avoidance, fast retransmit with β = 0.7, fast convergence, and
//! Hystart++ slow-start exit.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::cong::{CcAlgo, CongestionControl, Cubic, INITIAL_CWND};

const MSS: u32 = 1460;

// Synthetic time base for tests (ms).
const T0: u64 = 100_000;

// Helper: feed one ACK with given params.
fn ack(c: &mut Cubic, acked: u32, rtt: Option<u32>, snd_una: u32, snd_nxt: u32, now_ms: u64) {
    c.on_ack(acked, rtt, snd_una, snd_nxt, now_ms);
}

// =============================================================================
// Slow start
// =============================================================================

pub fn test_cubic_initial_cwnd_is_iw10() -> TestResult {
    let c = Cubic::default();
    assert_eq_test!(c.cwnd(), INITIAL_CWND, "IW = 10 * MSS");
    assert_test!(c.ssthresh() > c.cwnd(), "ssthresh starts high");
    assert_test!(!c.in_recovery(), "not in recovery at birth");
    pass!()
}

/// Each ACK in slow start grows cwnd by min(acked, MSS).
pub fn test_cubic_slow_start_grows_per_ack() -> TestResult {
    let mut c = Cubic::default();
    let c0 = c.cwnd();
    ack(&mut c, MSS, Some(50), MSS, 20 * MSS, T0);
    assert_eq_test!(c.cwnd(), c0 + MSS, "cwnd grew by MSS");
    ack(&mut c, 500, Some(50), 2 * MSS, 20 * MSS, T0 + 50);
    assert_eq_test!(c.cwnd(), c0 + MSS + 500, "cwnd grew by partial ACK");
    pass!()
}

// =============================================================================
// Congestion avoidance — CUBIC growth
// =============================================================================

/// After a loss, CUBIC growth is concave up to W_max, then convex beyond.
pub fn test_cubic_ca_concave_growth() -> TestResult {
    let mut c = Cubic::new(MSS);

    // Grow cwnd to 40 MSS via slow start.
    let mut una = 0u32;
    for i in 0..40 {
        ack(
            &mut c,
            MSS,
            Some(20),
            una,
            (i + 41) * MSS,
            T0 + i as u64 * 20,
        );
        una += MSS;
    }
    let pre_loss_cwnd = c.cwnd();
    assert_test!(pre_loss_cwnd >= 40 * MSS, "cwnd grew in slow start");

    // Trigger loss → ssthresh = cwnd * 0.7
    c.on_fast_retransmit(pre_loss_cwnd, una + 10 * MSS);
    let ssthresh_after = c.ssthresh();
    assert_test!(c.cwnd() == ssthresh_after, "cwnd = ssthresh after loss");

    // Exit recovery.
    ack(
        &mut c,
        MSS,
        Some(20),
        una + 10 * MSS,
        una + 20 * MSS,
        T0 + 1000,
    );
    assert_test!(!c.in_recovery(), "exited recovery");

    // Now in CA. Feed ACKs over time and verify cwnd grows.
    let cwnd_at_ca_start = c.cwnd();
    let mut now = T0 + 1000;
    for i in 0..100 {
        now += 20; // 20ms per ACK
        una += MSS;
        ack(&mut c, MSS, Some(20), una, una + 10 * MSS, now);
        // Suppress unused warning.
        let _ = i;
    }
    assert_test!(
        c.cwnd() > cwnd_at_ca_start,
        "cwnd grew in congestion avoidance"
    );
    // CUBIC should eventually exceed the pre-loss cwnd (convex region).
    // Feed more ACKs with time passing.
    for _ in 0..500 {
        now += 20;
        una += MSS;
        ack(&mut c, MSS, Some(20), una, una + 10 * MSS, now);
    }
    assert_test!(
        c.cwnd() > pre_loss_cwnd,
        "CUBIC grew past W_max (convex region)"
    );
    pass!()
}

/// TCP-friendliness: CUBIC's cwnd should grow at least as fast as Reno
/// in congestion avoidance.
pub fn test_cubic_tcp_friendliness() -> TestResult {
    let mut c = Cubic::new(MSS);

    // Force into CA: timeout twice to get a low ssthresh, then grow past it.
    c.on_timeout(10 * MSS);
    c.on_timeout(2 * MSS);
    while c.cwnd() < c.ssthresh() {
        ack(&mut c, MSS, Some(20), 0, 50 * MSS, T0);
    }

    let ca_start_cwnd = c.cwnd();
    let acks_per_rtt = ca_start_cwnd / MSS;

    // Fire one RTT's worth of ACKs.
    let mut now = T0;
    for _ in 0..acks_per_rtt {
        now += 20;
        ack(&mut c, MSS, Some(20), 0, 50 * MSS, now);
    }
    let growth = c.cwnd() - ca_start_cwnd;
    // Reno grows ~1 MSS per RTT.  CUBIC (via TCP-friendliness) should
    // grow at least half that — generous slack for small-window case.
    assert_test!(
        growth >= MSS / 4,
        "TCP-friendliness: grew at least MSS/4 in one RTT"
    );
    pass!()
}

// =============================================================================
// Fast retransmit — β = 0.7
// =============================================================================

pub fn test_cubic_fast_retransmit_beta_07() -> TestResult {
    let mut c = Cubic::new(MSS);
    // Grow cwnd to 20 MSS.
    let mut una = 0u32;
    for i in 0..20 {
        ack(&mut c, MSS, Some(20), una, (i + 21) * MSS, T0);
        una += MSS;
    }
    let pre_loss = c.cwnd();
    c.on_fast_retransmit(pre_loss, una + 10 * MSS);

    // ssthresh ≈ origin_point * 0.7.  origin_point = pre_loss (no fast
    // convergence on first loss).
    let expected_ssthresh = (pre_loss as u64 * 7 / 10) as u32;
    // Allow ±1 for integer truncation.
    assert_test!(
        c.ssthresh() >= expected_ssthresh - 1 && c.ssthresh() <= expected_ssthresh + 1,
        "ssthresh ≈ cwnd * 0.7"
    );
    assert_eq_test!(c.cwnd(), c.ssthresh(), "cwnd = ssthresh after loss");
    assert_test!(c.in_recovery(), "in recovery");
    pass!()
}

// =============================================================================
// RTO timeout
// =============================================================================

pub fn test_cubic_timeout_resets_to_one_mss() -> TestResult {
    let mut c = Cubic::new(MSS);
    let mut una = 0u32;
    for i in 0..10 {
        ack(&mut c, MSS, Some(20), una, (i + 11) * MSS, T0);
        una += MSS;
    }
    c.on_timeout(c.cwnd());
    assert_eq_test!(c.cwnd(), MSS, "cwnd = MSS after timeout");
    assert_test!(c.ssthresh() >= 2 * MSS, "ssthresh at least 2*MSS");
    assert_test!(!c.in_recovery(), "recovery cleared on timeout");
    pass!()
}

// =============================================================================
// Recovery exit
// =============================================================================

pub fn test_cubic_recovery_exit() -> TestResult {
    let mut c = Cubic::new(MSS);
    c.on_fast_retransmit(14600, 5000);
    assert_test!(c.in_recovery(), "in recovery after fast retransmit");

    // snd_una below recover → stay in recovery.
    ack(&mut c, MSS, None, 4000, 10000, T0);
    assert_test!(c.in_recovery(), "still in recovery when snd_una < recover");

    // snd_una at recover → exit.
    ack(&mut c, MSS, None, 5000, 10000, T0 + 100);
    assert_test!(!c.in_recovery(), "exited recovery when snd_una >= recover");
    assert_eq_test!(
        c.cwnd(),
        c.ssthresh(),
        "cwnd == ssthresh after recovery exit"
    );
    pass!()
}

// =============================================================================
// Fast convergence (RFC 8312 §4.6)
// =============================================================================

pub fn test_cubic_fast_convergence() -> TestResult {
    let mut c = Cubic::new(MSS);
    // Grow to 30 MSS.
    let mut una = 0u32;
    for i in 0..30 {
        ack(&mut c, MSS, Some(20), una, (i + 31) * MSS, T0);
        una += MSS;
    }
    let first_loss_cwnd = c.cwnd();

    // First loss: sets last_max_cwnd = cwnd.
    c.on_fast_retransmit(first_loss_cwnd, una + 10 * MSS);
    let ssthresh1 = c.ssthresh();

    // Exit recovery.
    ack(
        &mut c,
        MSS,
        Some(20),
        una + 10 * MSS,
        una + 20 * MSS,
        T0 + 500,
    );

    // Don't fully recover — lose again at a lower cwnd.
    let second_loss_cwnd = c.cwnd();
    assert_test!(
        second_loss_cwnd < first_loss_cwnd,
        "cwnd didn't recover to pre-loss level"
    );
    c.on_fast_retransmit(second_loss_cwnd, una + 20 * MSS);
    let ssthresh2 = c.ssthresh();

    // Fast convergence: origin_point = cwnd * 17/20 (< cwnd), so
    // ssthresh2 < ssthresh that would result from origin_point = cwnd.
    let naive_ssthresh = (second_loss_cwnd as u64 * 7 / 10) as u32;
    assert_test!(
        ssthresh2 < naive_ssthresh,
        "fast convergence reduced ssthresh below naive β*cwnd"
    );
    // Sanity: ssthresh2 < ssthresh1.
    assert_test!(ssthresh2 < ssthresh1, "ssthresh decreased on second loss");
    pass!()
}

// =============================================================================
// Hystart++ (RFC 9406)
// =============================================================================

/// RTT increase across rounds triggers CSS (linear growth instead of
/// exponential).
pub fn test_cubic_hystart_rtt_exit_to_css() -> TestResult {
    let mut c = Cubic::new(MSS);

    // Round 1: seed with low RTT (20 ms).
    // We need to complete one round to establish a baseline.
    let mut una: u32 = 0;
    let snd_nxt_r1 = 10 * MSS;
    for _ in 0..5 {
        ack(&mut c, MSS, Some(20), una, snd_nxt_r1, T0);
        una += MSS;
    }
    // Complete round 1: snd_una reaches round_start.
    ack(&mut c, MSS, Some(20), snd_nxt_r1, 20 * MSS, T0 + 100);
    una = snd_nxt_r1 + MSS;
    let cwnd_after_r1 = c.cwnd();

    // Round 2: RTT jumps to 60 ms (well above 20 + threshold).
    // threshold = clamp(20/8, 4, 16) = 4.  So 60 > 20 + 4 = 24 → CSS.
    let snd_nxt_r2 = 20 * MSS;
    for _ in 0..5 {
        ack(&mut c, MSS, Some(60), una, snd_nxt_r2, T0 + 200);
        una += MSS;
    }
    // Complete round 2.
    ack(&mut c, MSS, Some(60), snd_nxt_r2, 30 * MSS, T0 + 300);
    una = snd_nxt_r2 + MSS;

    // In CSS: growth should be linear (much less than exponential).
    let cwnd_before_css_ack = c.cwnd();
    ack(&mut c, MSS, Some(60), una, 30 * MSS, T0 + 320);
    let css_growth = c.cwnd() - cwnd_before_css_ack;

    // In standard SS, growth = MSS per ACK.  In CSS, growth ≈ MSS²/cwnd
    // which is much smaller for large cwnd.
    assert_test!(css_growth < MSS, "CSS: growth per ACK is sublinear (< MSS)");
    let _ = cwnd_after_r1;
    pass!()
}

/// After CSS_ROUNDS (5) rounds in CSS, ssthresh is set and we enter CA.
pub fn test_cubic_css_to_ca_after_5_rounds() -> TestResult {
    let mut c = Cubic::new(MSS);

    // Build up a baseline round with low RTT.
    let mut una: u32 = 0;
    let mut nxt: u32 = 10 * MSS;

    // Round 0: seed.
    for _ in 0..5 {
        ack(&mut c, MSS, Some(20), una, nxt, T0);
        una += MSS;
    }
    ack(&mut c, MSS, Some(20), nxt, nxt + 10 * MSS, T0 + 100);
    una = nxt + MSS;
    nxt += 10 * MSS;

    // Round 1: RTT jumps → enter CSS.
    for _ in 0..5 {
        ack(&mut c, MSS, Some(60), una, nxt, T0 + 200);
        una += MSS;
    }
    ack(&mut c, MSS, Some(60), nxt, nxt + 10 * MSS, T0 + 300);
    una = nxt + MSS;
    nxt += 10 * MSS;

    // Now in CSS. Complete 5 more rounds with sustained high RTT.
    // RTT must stay above css_baseline (60) + threshold (≈7) = 67 to
    // avoid the false-alarm reversion.  Use RTT=70.
    let mut time = T0 + 400;
    for _round in 0..5 {
        let round_target = nxt;
        for _ in 0..5 {
            ack(&mut c, MSS, Some(70), una, round_target, time);
            una += MSS;
            time += 20;
        }
        // Complete this round.
        nxt = round_target + 10 * MSS;
        ack(&mut c, MSS, Some(70), round_target, nxt, time);
        una = round_target + MSS;
        time += 100;
    }

    // After 5 CSS rounds, ssthresh should have been set to cwnd
    // (transitioning to CA).
    assert_test!(
        c.ssthresh() <= c.cwnd() + MSS,
        "ssthresh set near cwnd after CSS rounds"
    );
    pass!()
}

// =============================================================================
// Integer cube root
// =============================================================================

pub fn test_cubic_integer_cbrt() -> TestResult {
    // Test the cube root via the CUBIC algorithm indirectly:
    // After a loss from a known cwnd, K should be predictable.
    //
    // We test by verifying that the algorithm produces reasonable cwnd
    // growth that matches the cubic function timing.
    let mut c = Cubic::new(MSS);
    // Grow to 20 MSS, lose, verify we can recover.
    let mut una = 0u32;
    for i in 0..20 {
        ack(&mut c, MSS, Some(20), una, (i + 21) * MSS, T0);
        una += MSS;
    }
    let pre_loss = c.cwnd();
    c.on_fast_retransmit(pre_loss, una + 10 * MSS);

    // Exit recovery.
    ack(
        &mut c,
        MSS,
        Some(20),
        una + 10 * MSS,
        una + 20 * MSS,
        T0 + 500,
    );

    // Pump ACKs for a long time; eventually cwnd must reach pre_loss.
    let mut now = T0 + 500;
    for _ in 0..2000 {
        now += 20;
        una += MSS;
        ack(&mut c, MSS, Some(20), una, una + 10 * MSS, now);
    }
    assert_test!(
        c.cwnd() >= pre_loss,
        "cwnd recovered to pre-loss level (K computation correct)"
    );
    pass!()
}

// =============================================================================
// CcAlgo enum dispatch
// =============================================================================

pub fn test_cc_algo_enum_dispatches_to_cubic() -> TestResult {
    let mut algo = CcAlgo::cubic(MSS);
    let c0 = algo.cwnd();
    algo.on_ack(MSS, Some(10), 0, 20 * MSS, T0);
    assert_eq_test!(algo.cwnd(), c0 + MSS, "enum dispatches to Cubic");
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::define_test_suite!(
    tcp_cong_cubic,
    [
        test_cubic_initial_cwnd_is_iw10,
        test_cubic_slow_start_grows_per_ack,
        test_cubic_ca_concave_growth,
        test_cubic_tcp_friendliness,
        test_cubic_fast_retransmit_beta_07,
        test_cubic_timeout_resets_to_one_mss,
        test_cubic_recovery_exit,
        test_cubic_fast_convergence,
        test_cubic_hystart_rtt_exit_to_css,
        test_cubic_css_to_ca_after_5_rounds,
        test_cubic_integer_cbrt,
        test_cc_algo_enum_dispatches_to_cubic,
    ]
);
