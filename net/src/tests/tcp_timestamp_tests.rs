//! TCP Timestamps (RFC 7323) tests.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use super::tcp_common::*;
use crate::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_PSH, TCP_FLAG_RST};

/// Read (ts_enabled, ts_recent) from a Data-state PCB.
fn ts_state(id: tcp::ConnId) -> (bool, u32) {
    tcp::with_pcb(id, |pcb| {
        let tcp::PcbState::Data(d) = &pcb.state else {
            panic!("expected Data state");
        };
        (d.ts_enabled, d.ts_recent)
    })
    .expect("PCB should exist")
}

/// Read whether the RTT estimator has a sample.
fn has_rtt_sample(id: tcp::ConnId) -> bool {
    tcp::with_pcb(id, |pcb| {
        let tcp::PcbState::Data(d) = &pcb.state else {
            panic!("expected Data state");
        };
        d.rtt.has_sample()
    })
    .expect("PCB should exist")
}

/// Read rcv_nxt.
fn rcv_nxt_raw(id: tcp::ConnId) -> u32 {
    tcp::with_pcb(id, |pcb| {
        let tcp::PcbState::Data(d) = &pcb.state else {
            panic!("expected Data state");
        };
        d.rcv_nxt.raw()
    })
    .expect("PCB should exist")
}

/// Read reset_received.
fn is_reset(id: tcp::ConnId) -> bool {
    tcp::with_pcb(id, |pcb| {
        let tcp::PcbState::Data(d) = &pcb.state else {
            return false;
        };
        d.reset_received
    })
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Negotiation
// ---------------------------------------------------------------------------

pub fn test_active_open_ts_negotiation() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(100);

    let conn = establish_connection_with_ts();
    let (enabled, recent) = ts_state(conn.id);
    assert_test!(enabled, "ts_enabled after TS negotiation");
    assert_eq_test!(recent, 1000, "ts_recent = peer SYN-ACK tsval");
    pass!()
}

pub fn test_ts_declined_by_peer() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(100);

    let conn = establish_connection();
    let (enabled, _) = ts_state(conn.id);
    assert_test!(!enabled, "ts_enabled false when peer declines");

    tcp::send(conn.id, b"hello").ok();
    if let Some((seg, _)) = poll_once(conn.id) {
        assert_test!(
            seg.timestamp.is_none(),
            "no TSopt when timestamps not negotiated"
        );
    }
    pass!()
}

// ---------------------------------------------------------------------------
// Data segments carry TSopt
// ---------------------------------------------------------------------------

pub fn test_data_segments_carry_tsopt() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(500);

    let conn = establish_connection_with_ts();

    tcp::send(conn.id, b"hello").ok();
    let (seg, _) = poll_once(conn.id).expect("should have data");
    assert_test!(seg.timestamp.is_some(), "data segment carries TSopt");
    let (tsval, tsecr) = seg.timestamp.unwrap();
    assert_test!(tsval > 0, "TSval non-zero");
    assert_eq_test!(tsecr, 1000, "TSecr echoes peer SYN-ACK tsval");
    pass!()
}

// ---------------------------------------------------------------------------
// PAWS
// ---------------------------------------------------------------------------

pub fn test_paws_rejects_old_duplicate() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(100);

    let conn = establish_connection_with_ts();
    let peer_seq = conn.peer_iss + 1;

    // Fresh timestamp → accepted.
    let tsopt_fresh = build_tsopt(2000, 0);
    let actions = inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        peer_seq,
        conn.our_iss + 1,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        &tsopt_fresh,
        b"good",
    );
    assert_test!(!actions.notify.is_empty(), "fresh-TS segment accepted");

    // Old timestamp → PAWS drop + dup-ACK.
    let tsopt_old = build_tsopt(500, 0);
    let actions2 = inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        peer_seq + 4,
        conn.our_iss + 1,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        &tsopt_old,
        b"old!",
    );
    assert_test!(actions2.segments().count() > 0, "PAWS drop emits dup-ACK");

    let nxt = rcv_nxt_raw(conn.id);
    assert_eq_test!(nxt, peer_seq + 4, "rcv_nxt unchanged after PAWS drop");
    pass!()
}

pub fn test_paws_allows_rst() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(100);

    let conn = establish_connection_with_ts();
    let peer_seq = conn.peer_iss + 1;

    let tsopt_fresh = build_tsopt(2000, 0);
    let _ = inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        peer_seq,
        conn.our_iss + 1,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        &tsopt_fresh,
        b"data",
    );

    // RST with old timestamp — should bypass PAWS.
    let tsopt_old = build_tsopt(100, 0);
    let _ = inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        peer_seq + 4,
        conn.our_iss + 1,
        TCP_FLAG_RST | TCP_FLAG_ACK,
        &tsopt_old,
        &[],
    );

    assert_test!(is_reset(conn.id), "RST bypasses PAWS");
    pass!()
}

// ---------------------------------------------------------------------------
// RTTM
// ---------------------------------------------------------------------------

pub fn test_rttm_samples_every_ack() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(1000);

    let conn = establish_connection_with_ts();

    tcp::send(conn.id, b"hello").ok();
    let (seg, _) = poll_once(conn.id).expect("should have data");
    let our_tsval = seg.timestamp.unwrap().0;

    #[cfg(feature = "test-hooks")]
    tcp::clock::MockClock::advance(50);

    let tsopt = build_tsopt(3000, our_tsval);
    let _ = inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        conn.peer_iss + 1,
        conn.our_iss + 1 + 5,
        TCP_FLAG_ACK,
        &tsopt,
        &[],
    );

    assert_test!(has_rtt_sample(conn.id), "RTTM produced a sample");
    pass!()
}

// ---------------------------------------------------------------------------
// Non-timestamp fallback
// ---------------------------------------------------------------------------

pub fn test_non_ts_fallback_karn_sampling() -> TestResult {
    reset_all();
    #[cfg(feature = "test-hooks")]
    let _clock = tcp::clock::MockClockGuard::install_at(1000);

    let conn = establish_connection();
    let (enabled, _) = ts_state(conn.id);
    assert_test!(!enabled, "no timestamps negotiated");

    tcp::send(conn.id, b"hello").ok();
    let _ = poll_once(conn.id).expect("should have data");

    #[cfg(feature = "test-hooks")]
    tcp::clock::MockClock::advance(30);

    inject_ack(&conn, conn.peer_iss + 1, conn.our_iss + 1 + 5);

    assert_test!(
        has_rtt_sample(conn.id),
        "Karn sampling works without timestamps"
    );
    pass!()
}
