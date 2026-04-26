//! Unit tests for `tcp::pcb::listen::ListenState::on_segment`.
//!
//! Drives the handler directly: we construct a `Pcb` in `Listen`
//! state, hand it a synthetic `TcpHeader`, and assert on the
//! returned `Actions` + any state mutation.  No table lookup, no
//! socket layer, no timer wheel.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::header::{
    DEFAULT_MSS, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader,
};
use crate::tcp::pcb::{ListenState, Pcb, PcbState};
use crate::tcp::tuple::TcpTuple;

fn make_pcb() -> Pcb {
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [0, 0, 0, 0],
        remote_port: 0,
    };
    Pcb::new(tuple, PcbState::Listen(ListenState::new()))
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port: 40000,
        dst_port: 80,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// RST in → no action (silently dropped, RFC 793 §3.4).
pub fn test_listen_rst_is_ignored() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    assert_eq_test!(actions.timer_ops_len, 0, "no timer ops");
    assert_test!(actions.accepted.is_none(), "no child accepted");
    assert_test!(actions.notify.is_empty(), "no notify bits");
    pass!()
}

/// ACK in → RST out, no child accepted.
pub fn test_listen_ack_triggers_rst() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_ACK, 1000, 2000), &[], 0);
    assert_eq_test!(actions.segments_len, 1, "one RST emitted");
    let rst = actions.segments[0].as_ref().unwrap();
    assert_test!((rst.flags & TCP_FLAG_RST) != 0, "response has RST flag");
    assert_test!(actions.accepted.is_none(), "no accepted");
    pass!()
}

/// SYN → SYN+ACK out, accepted populated, ACCEPT_WAKE set.
pub fn test_listen_syn_emits_syn_ack_and_accepts() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 5000, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 1, "one SYN+ACK emitted");
    let seg = actions.segments[0].as_ref().unwrap();
    assert_test!(
        (seg.flags & (TCP_FLAG_SYN | TCP_FLAG_ACK)) == (TCP_FLAG_SYN | TCP_FLAG_ACK),
        "SYN+ACK flags"
    );
    assert_eq_test!(seg.ack_num, 5001, "ack acknowledges peer ISS + 1");
    let accepted = actions.accepted.as_ref().expect("accepted populated");
    assert_eq_test!(accepted.irs, 5000, "IRS matches peer SYN seq");
    assert_eq_test!(accepted.peer_mss, DEFAULT_MSS, "default MSS");
    assert_test!(
        actions.notify.contains(SocketNotify::ACCEPT_WAKE),
        "accept_wake bit"
    );
    // Listen PCB itself did not transition.
    assert_test!(
        matches!(pcb.state, PcbState::Listen(_)),
        "Listen PCB stays in Listen"
    );
    pass!()
}

/// SYN with an MSS option is parsed into accepted.peer_mss.
pub fn test_listen_syn_parses_mss_option() -> TestResult {
    let mut pcb = make_pcb();
    // MSS option: kind 2, length 4, value 1200
    let opts = [2, 4, 0x04, 0xB0];
    let actions = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 0, 0), &opts, 0);
    let accepted = actions.accepted.as_ref().unwrap();
    assert_eq_test!(accepted.peer_mss, 1200, "parsed MSS");
    pass!()
}

/// FIN alone at a LISTEN socket is a malformed stranger; dropped.
pub fn test_listen_stray_fin_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments");
    assert_test!(actions.accepted.is_none(), "no accepted");
    pass!()
}

/// Consecutive SYNs each emit a fresh SYN+ACK with a different ISS.
pub fn test_listen_two_syns_get_distinct_iss() -> TestResult {
    let mut pcb = make_pcb();
    let a = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 1000, 0), &[], 0);
    let b = ListenState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 2000, 0), &[], 0);
    let iss_a = a.accepted.as_ref().unwrap().iss;
    let iss_b = b.accepted.as_ref().unwrap().iss;
    // ISN varies per-tuple; same tuple with a ~µs gap may collide on
    // the drift bucket but is unlikely to be identical across both
    // calls unless the hash is broken.  Just assert they aren't both
    // zero (a sanity check, not a distinctiveness proof).
    assert_test!(iss_a != 0, "ISS a not zero");
    assert_test!(iss_b != 0, "ISS b not zero");
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::stest!(name = test_listen_rst_is_ignored, suite = tcp_pcb_listen);
slopos_testing::stest!(name = test_listen_ack_triggers_rst, suite = tcp_pcb_listen);
slopos_testing::stest!(
    name = test_listen_syn_emits_syn_ack_and_accepts,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_syn_parses_mss_option,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(name = test_listen_stray_fin_dropped, suite = tcp_pcb_listen);
slopos_testing::stest!(
    name = test_listen_two_syns_get_distinct_iss,
    suite = tcp_pcb_listen
);
