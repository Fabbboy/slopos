//! Unit tests for `tcp::pcb::time_wait::TimeWaitState::on_segment`.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::header::{
    TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader,
};
use crate::tcp::pcb::time_wait::TimeWaitState;
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;

const LAST_RCV_NXT: u32 = 12_345;
const LAST_SND_NXT: u32 = 67_890;

fn make_pcb() -> Pcb {
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 49_152,
        remote_ip: [10, 0, 0, 2],
        remote_port: 80,
    };
    Pcb::new(
        tuple,
        PcbState::TimeWait(TimeWaitState::new(
            SeqNum::new(LAST_RCV_NXT),
            SeqNum::new(LAST_SND_NXT),
            32_768,
            1000,
        )),
    )
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port: 80,
        dst_port: 49_152,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32_768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// RST releases the PCB.
pub fn test_time_wait_rst_releases() -> TestResult {
    let mut pcb = make_pcb();
    let actions = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, 0, 0), 2000);
    assert_test!(actions.release, "release");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED"
    );
    pass!()
}

/// Retransmitted FIN re-ACKs with frozen sequence numbers.
pub fn test_time_wait_fin_re_acks() -> TestResult {
    let mut pcb = make_pcb();
    let actions =
        TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, 0, 0), 5_000);
    assert_eq_test!(actions.segments_len, 1, "one ACK emitted");
    let ack = actions.segments[0].as_ref().unwrap();
    assert_eq_test!(ack.seq_num, LAST_SND_NXT, "frozen snd_nxt");
    assert_eq_test!(ack.ack_num, LAST_RCV_NXT, "frozen rcv_nxt");
    assert_test!((ack.flags & TCP_FLAG_ACK) != 0, "ACK flag set");
    // entry_ms updated for the new timer baseline
    if let PcbState::TimeWait(s) = &pcb.state {
        assert_eq_test!(s.entry_ms, 5_000, "entry_ms refreshed");
    }
    assert_test!(!actions.release, "not released");
    pass!()
}

/// Data segment (PSH+ACK) is dropped.
pub fn test_time_wait_data_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions =
        TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_PSH | TCP_FLAG_ACK, 0, 0), 2000);
    assert_eq_test!(actions.segments_len, 0, "no response");
    assert_test!(!actions.release, "not released");
    pass!()
}

/// SYN is dropped.
pub fn test_time_wait_syn_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 0, 0), 2000);
    assert_eq_test!(actions.segments_len, 0, "no response");
    pass!()
}

/// Empty packet is dropped.
pub fn test_time_wait_empty_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions = TimeWaitState::on_segment(&mut pcb, &hdr(0, 0, 0), 2000);
    assert_eq_test!(actions.segments_len, 0, "no response");
    pass!()
}

/// Successive retransmitted FINs each refresh entry_ms.
pub fn test_time_wait_fin_refreshes_entry_ms_every_call() -> TestResult {
    let mut pcb = make_pcb();
    let _ = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, 0, 0), 500);
    let _ = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, 0, 0), 900);
    if let PcbState::TimeWait(s) = &pcb.state {
        assert_eq_test!(s.entry_ms, 900, "entry_ms tracks latest FIN");
    }
    pass!()
}

slopos_testing::define_test_suite!(
    tcp_pcb_time_wait,
    [
        test_time_wait_rst_releases,
        test_time_wait_fin_re_acks,
        test_time_wait_data_dropped,
        test_time_wait_syn_dropped,
        test_time_wait_empty_dropped,
        test_time_wait_fin_refreshes_entry_ms_every_call,
    ]
);
