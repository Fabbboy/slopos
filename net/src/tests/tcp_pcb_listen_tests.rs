//! Unit tests for `tcp::pcb::listen::ListenState::on_segment`.
//!
//! Drives the handler directly: a `Pcb` in `Listen` state, a synthetic
//! `TcpHeader`, and assertions on the returned `Actions` plus the SYN queue's
//! own state. No table lookup, no socket layer, no timer dispatch.
//!
//! The property these exist for is that a SYN costs a SYN-queue entry and
//! nothing else — no connection-table slot is spent until a handshake
//! completes, and the queue is bounded per listener.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::header::{
    DEFAULT_MSS, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader,
};
use crate::tcp::listener::{SYN_QUEUE_MAX, SynQueue};
use crate::tcp::pcb::{ListenState, Pcb, PcbState};
use crate::tcp::tuple::TcpTuple;
use crate::types::{Ipv4Addr, Port, SockAddr};

const LOCAL_IP: [u8; 4] = [10, 0, 0, 1];
const LOCAL_PORT: u16 = 80;
const REMOTE_IP: [u8; 4] = [10, 0, 0, 2];

fn make_pcb() -> Pcb {
    let tuple = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: LOCAL_PORT,
        remote_ip: [0, 0, 0, 0],
        remote_port: 0,
    };
    let syn = SynQueue::with_capacity(SockAddr::new(Ipv4Addr(LOCAL_IP), Port(LOCAL_PORT)))
        .expect("syn queue alloc");
    Pcb::new(tuple, PcbState::Listen(ListenState::with_syn_queue(syn)))
}

/// The four-tuple a segment from `remote_port` arrives on.
fn incoming(remote_port: u16) -> TcpTuple {
    TcpTuple {
        local_ip: LOCAL_IP,
        local_port: LOCAL_PORT,
        remote_ip: REMOTE_IP,
        remote_port,
    }
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    hdr_from(40000, flags, seq, ack)
}

fn hdr_from(src_port: u16, flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port,
        dst_port: LOCAL_PORT,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

fn syn_queue_len(pcb: &Pcb) -> usize {
    match &pcb.state {
        PcbState::Listen(listen) => listen.syn_queue().len(),
        _ => usize::MAX,
    }
}

/// RST in → no action, and the half-open entry it names is retired.
pub fn test_listen_rst_is_ignored() -> TestResult {
    let mut pcb = make_pcb();
    let actions =
        ListenState::on_segment(&mut pcb, &incoming(40000), &hdr(TCP_FLAG_RST, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    assert_eq_test!(actions.timer_ops_len, 0, "no timer ops");
    assert_test!(actions.accepted.is_none(), "no child accepted");
    assert_test!(actions.notify.is_empty(), "no notify bits");
    pass!()
}

/// A RST for a queued half-open connection retires it, freeing the slot.
pub fn test_listen_rst_retires_the_half_open_entry() -> TestResult {
    let mut pcb = make_pcb();
    let _ = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    assert_eq_test!(syn_queue_len(&pcb), 1, "SYN queued");

    let _ = ListenState::on_segment(&mut pcb, &incoming(40000), &hdr(TCP_FLAG_RST, 0, 0), &[], 0);
    assert_eq_test!(syn_queue_len(&pcb), 0, "RST retired the entry");
    pass!()
}

/// An ACK naming no half-open connection → RST out, nothing accepted.
pub fn test_listen_ack_triggers_rst() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_ACK, 1000, 2000),
        &[],
        0,
    );
    assert_eq_test!(actions.segments_len, 1, "one RST emitted");
    let rst = actions.segments[0].as_ref().unwrap();
    assert_test!((rst.flags & TCP_FLAG_RST) != 0, "response has RST flag");
    assert_test!(actions.accepted.is_none(), "no accepted");
    pass!()
}

/// SYN → SYN+ACK out and a SYN-queue entry, but **nothing accepted**: a
/// half-open connection must not reach the connection table.
pub fn test_listen_syn_queues_without_accepting() -> TestResult {
    let mut pcb = make_pcb();
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    assert_eq_test!(actions.segments_len, 1, "one SYN+ACK emitted");
    let seg = actions.segments[0].as_ref().unwrap();
    assert_test!(
        (seg.flags & (TCP_FLAG_SYN | TCP_FLAG_ACK)) == (TCP_FLAG_SYN | TCP_FLAG_ACK),
        "SYN+ACK flags"
    );
    assert_eq_test!(seg.ack_num, 5001, "ack acknowledges peer ISS + 1");
    assert_test!(
        actions.accepted.is_none(),
        "a SYN must not produce an accepted connection"
    );
    assert_eq_test!(syn_queue_len(&pcb), 1, "one half-open connection queued");
    assert_test!(
        matches!(pcb.state, PcbState::Listen(_)),
        "Listen PCB stays in Listen"
    );
    pass!()
}

/// The final ACK completes the handshake: the entry leaves the SYN queue and
/// becomes an `AcceptedConn` carrying both sides' sequence numbers.
pub fn test_listen_final_ack_completes_the_handshake() -> TestResult {
    let mut pcb = make_pcb();
    let syn = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    let our_iss = syn.segments[0].as_ref().unwrap().seq_num;

    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_ACK, 5001, our_iss.wrapping_add(1)),
        &[],
        0,
    );
    let accepted = match actions.accepted.as_ref() {
        Some(a) => a,
        None => return fail!("the final ACK did not complete the handshake"),
    };
    assert_eq_test!(accepted.iss, our_iss, "our ISS carried through");
    assert_eq_test!(accepted.irs, 5000, "peer ISS carried through");
    assert_eq_test!(
        accepted.tuple.remote_port,
        40000,
        "peer port carried through"
    );
    assert_eq_test!(
        accepted.tuple.remote_ip,
        REMOTE_IP,
        "peer IP carried through"
    );
    assert_test!(
        actions.notify.contains(SocketNotify::ACCEPT_WAKE),
        "accept_wake bit"
    );
    assert_eq_test!(actions.segments_len, 0, "no segment answers the final ACK");
    assert_eq_test!(syn_queue_len(&pcb), 0, "the entry left the SYN queue");
    pass!()
}

/// An ACK whose number does not acknowledge our SYN-ACK is not a handshake.
pub fn test_listen_wrong_ack_number_does_not_complete() -> TestResult {
    let mut pcb = make_pcb();
    let _ = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_ACK, 5001, 0xDEAD_BEEF),
        &[],
        0,
    );
    assert_test!(
        actions.accepted.is_none(),
        "wrong ACK completed a handshake"
    );
    assert_eq_test!(
        actions.segments_len,
        1,
        "an unmatched ACK is answered by RST"
    );
    assert_eq_test!(syn_queue_len(&pcb), 1, "the entry stays queued");
    pass!()
}

/// SYN with an MSS option carries it through to the accepted connection.
pub fn test_listen_syn_parses_mss_option() -> TestResult {
    let mut pcb = make_pcb();
    // MSS option: kind 2, length 4, value 1200
    let opts = [2, 4, 0x04, 0xB0];
    let syn = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 0, 0),
        &opts,
        0,
    );
    let our_iss = syn.segments[0].as_ref().unwrap().seq_num;
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_ACK, 1, our_iss.wrapping_add(1)),
        &[],
        0,
    );
    let accepted = actions.accepted.as_ref().unwrap();
    assert_eq_test!(accepted.peer_mss, 1200, "parsed MSS");
    pass!()
}

/// A SYN with no MSS option falls back to the default.
pub fn test_listen_syn_without_mss_uses_default() -> TestResult {
    let mut pcb = make_pcb();
    let syn = ListenState::on_segment(&mut pcb, &incoming(40000), &hdr(TCP_FLAG_SYN, 0, 0), &[], 0);
    let our_iss = syn.segments[0].as_ref().unwrap().seq_num;
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_ACK, 1, our_iss.wrapping_add(1)),
        &[],
        0,
    );
    assert_eq_test!(
        actions.accepted.as_ref().unwrap().peer_mss,
        DEFAULT_MSS,
        "default MSS"
    );
    pass!()
}

/// FIN alone at a LISTEN socket is a malformed stranger; dropped.
pub fn test_listen_stray_fin_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions =
        ListenState::on_segment(&mut pcb, &incoming(40000), &hdr(TCP_FLAG_FIN, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments");
    assert_test!(actions.accepted.is_none(), "no accepted");
    assert_eq_test!(syn_queue_len(&pcb), 0, "nothing queued");
    pass!()
}

/// A duplicate SYN retransmits the original SYN-ACK rather than taking a
/// second queue slot.
pub fn test_listen_duplicate_syn_reuses_its_slot() -> TestResult {
    let mut pcb = make_pcb();
    let first = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    let second = ListenState::on_segment(
        &mut pcb,
        &incoming(40000),
        &hdr(TCP_FLAG_SYN, 5000, 0),
        &[],
        0,
    );
    assert_eq_test!(syn_queue_len(&pcb), 1, "no second slot taken");
    assert_eq_test!(
        second.segments[0].as_ref().unwrap().seq_num,
        first.segments[0].as_ref().unwrap().seq_num,
        "the same SYN-ACK is retransmitted"
    );
    pass!()
}

/// The queue is bounded: SYN number `SYN_QUEUE_MAX + 1` is dropped, and
/// dropped *silently* — a RST would confirm to the sender that its flood is
/// working.
pub fn test_listen_syn_queue_is_bounded() -> TestResult {
    let mut pcb = make_pcb();
    for i in 0..SYN_QUEUE_MAX {
        let port = 40000 + i as u16;
        let actions = ListenState::on_segment(
            &mut pcb,
            &incoming(port),
            &hdr_from(port, TCP_FLAG_SYN, 1000 + i as u32, 0),
            &[],
            0,
        );
        if actions.segments_len != 1 {
            return fail!("SYN {} was refused before the bound", i);
        }
    }
    assert_eq_test!(
        syn_queue_len(&pcb),
        SYN_QUEUE_MAX,
        "queue filled to its bound"
    );

    let port = 40000 + SYN_QUEUE_MAX as u16;
    let actions = ListenState::on_segment(
        &mut pcb,
        &incoming(port),
        &hdr_from(port, TCP_FLAG_SYN, 9999, 0),
        &[],
        0,
    );
    assert_eq_test!(
        actions.segments_len,
        0,
        "the overflowing SYN is dropped silently"
    );
    assert_test!(actions.accepted.is_none(), "nothing accepted");
    assert_eq_test!(
        syn_queue_len(&pcb),
        SYN_QUEUE_MAX,
        "the queue did not grow past its bound"
    );
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::stest!(name = test_listen_rst_is_ignored, suite = tcp_pcb_listen);
slopos_testing::stest!(
    name = test_listen_rst_retires_the_half_open_entry,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(name = test_listen_ack_triggers_rst, suite = tcp_pcb_listen);
slopos_testing::stest!(
    name = test_listen_syn_queues_without_accepting,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_final_ack_completes_the_handshake,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_wrong_ack_number_does_not_complete,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_syn_parses_mss_option,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_syn_without_mss_uses_default,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(name = test_listen_stray_fin_dropped, suite = tcp_pcb_listen);
slopos_testing::stest!(
    name = test_listen_duplicate_syn_reuses_its_slot,
    suite = tcp_pcb_listen
);
slopos_testing::stest!(
    name = test_listen_syn_queue_is_bounded,
    suite = tcp_pcb_listen
);
