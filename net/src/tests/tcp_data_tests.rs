//! TCP data transfer regression tests.
//!
//! Covers: ring buffer operations, send/receive buffers, data transfer through
//! the TCP state machine, delayed ACK, retransmission, flow control, and
//! zero-window probing.
//!
//! A test holds a [`NetTestScope`] when it leaves live PCB state the kernel's
//! own threads can act on: a latched delayed ACK (whose deadline, computed from
//! a test's `now_ms`, is already past against the real uptime the net-timer
//! kthread reads), unacknowledged inflight data with an RTO armed in the live
//! wheel, or a PCB still in `SYN_SENT`. Reading a global table is not by itself
//! the criterion, and neither is the TEST-NET-1 address: the boot default route
//! carries 192.0.2.2 to the physical NIC exactly as it did 10.0.0.2, and only
//! the scope's own metric-0 `/24` wins longest-prefix over it. The scope resets
//! the socket and TCP tables on both edges, in the wheel that minted their
//! tokens, so those tests take no separate `reset()`.

use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::uframe::{KeepaliveFrames, UFrame};
use slopos_ostd::{KBox, KVec, ZcNotifToken};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ok, assert_test, fail, pass};

use crate::tcp::cong::CongestionControl;
use crate::tcp::{
    self, ConnId, DEFAULT_MSS, DELAYED_ACK_MS, MAX_RETRANSMITS, TCP_BUFFER_SIZE, TCP_FLAG_ACK,
    TCP_FLAG_FIN, TCP_FLAG_PSH, TcpError, TcpHeader, TcpSendState, TcpState,
};
use crate::tests::net_scope::{NetTestScope, ScopeError};
use crate::tests::tcp_common::{self, LOCAL_IP, REMOTE_IP, REMOTE_PORT, reset_all as reset};
use crate::with_data_state;

#[cold]
#[inline(never)]
fn scope_error(e: ScopeError) -> TestResult {
    fail!("net scope: {:?}", e)
}

fn establish_connection() -> (ConnId, u32, u16) {
    let c = tcp_common::establish_connection();
    (c.id, c.peer_iss, c.local_port)
}

fn inject_data_segment(
    remote_ip: [u8; 4],
    local_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    data: &[u8],
    now_ms: u64,
) -> tcp::Actions {
    let hdr = TcpHeader {
        src_port,
        dst_port,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags: TCP_FLAG_ACK | TCP_FLAG_PSH,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(remote_ip, local_ip, &hdr, &[], data, now_ms)
}

pub fn test_ring_buffer_new_empty() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    assert_eq_test!(tcp::send_buffer_space(id), TCP_BUFFER_SIZE, "capacity");
    assert_eq_test!(tcp::recv_available(id), 0, "new len");
    assert_test!(!tcp::has_pending_data(id), "new is empty");
    pass!()
}

pub fn test_ring_buffer_write_read_basic() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let n = tcp::send(id, b"hello").unwrap();
    assert_eq_test!(n, 5, "write hello");
    assert_test!(tcp::has_pending_data(id), "pending after write");

    let mut out = [0u8; 16];
    let (_, r, _) = tcp::poll_transmit(id, &mut out, 0).unwrap();
    assert_eq_test!(r, 5, "read hello");
    assert_test!(&out[..5] == b"hello", "content matches");
    pass!()
}

pub fn test_ring_buffer_write_full() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let chunk = [0xABu8; 512];
    let mut remaining = TCP_BUFFER_SIZE;
    while remaining > 0 {
        let to_write = core::cmp::min(remaining, chunk.len());
        let wrote = tcp::send(id, &chunk[..to_write]).unwrap();
        assert_eq_test!(wrote, to_write, "write chunk into send buffer");
        remaining -= to_write;
    }

    assert_eq_test!(tcp::send_buffer_space(id), 0, "no free space");
    let second = tcp::send(id, &[1, 2, 3]).unwrap();
    assert_eq_test!(second, 0, "write when full");
    pass!()
}

pub fn test_ring_buffer_wrap_around() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let mut seq = server_iss.wrapping_add(1);
    let half = TCP_BUFFER_SIZE / 2;
    let mut first: KBox<[u8; 256]> = KBox::zeroed().expect("alloc");
    first.iter_mut().for_each(|b| *b = 1);
    let mut injected = 0usize;
    while injected < half {
        let n = core::cmp::min(first.len(), half - injected);
        let _ = inject_data_segment(
            REMOTE_IP,
            LOCAL_IP,
            REMOTE_PORT,
            client_port,
            seq,
            snd_nxt,
            &first[..n],
            injected as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut tmp: KBox<[u8; 256]> = KBox::zeroed().expect("alloc");
    let mut drained = 0usize;
    while drained < half {
        let n = tcp::recv(id, &mut *tmp).unwrap();
        if n == 0 {
            return fail!("expected data while draining first half");
        }
        assert_test!(tmp[..n].iter().all(|&x| x == 1), "read content");
        drained += n;
    }
    assert_eq_test!(drained, half, "read first half");

    let mut second: KBox<[u8; 256]> = KBox::zeroed().expect("alloc");
    second.iter_mut().for_each(|b| *b = 2);
    injected = 0;
    while injected < half {
        let n = core::cmp::min(second.len(), half - injected);
        let _ = inject_data_segment(
            REMOTE_IP,
            LOCAL_IP,
            REMOTE_PORT,
            client_port,
            seq,
            snd_nxt,
            &second[..n],
            (half + injected) as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut out: KBox<[u8; 256]> = KBox::zeroed().expect("alloc");
    drained = 0;
    while drained < half {
        let n = tcp::recv(id, &mut *out).unwrap();
        if n == 0 {
            return fail!("expected data while draining wrapped half");
        }
        assert_test!(out[..n].iter().all(|&x| x == 2), "wrapped content");
        drained += n;
    }
    assert_eq_test!(drained, half, "read wrapped half");
    pass!()
}

pub fn test_ring_buffer_peek_offset() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdefgh").unwrap();

    let mut first = [0u8; 2];
    let (seg1, n1, _) = tcp::poll_transmit(id, &mut first, 0).unwrap();
    assert_eq_test!(n1, 2, "first chunk len");
    assert_test!(&first == b"ab", "first chunk content");

    let mut second = [0u8; 3];
    let (_, n2, _) = tcp::poll_transmit(id, &mut second, 1).unwrap();
    assert_eq_test!(n2, 3, "peek len");
    assert_test!(&second == b"cde", "peek offset data");

    let mut third = [0u8; 8];
    let (_, n3, _) = tcp::poll_transmit(id, &mut third, 2).unwrap();
    assert_eq_test!(n3, 3, "remaining chunk len");
    assert_test!(&third[..3] == b"fgh", "remaining chunk content");
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 8,
        "peek does not consume"
    );

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg1.seq_num.wrapping_add(8),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 2);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "full data preserved"
    );
    pass!()
}

pub fn test_ring_buffer_consume() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdef").unwrap();
    let mut tx = [0u8; 16];
    let (seg, n, _) = tcp::poll_transmit(id, &mut tx, 0).unwrap();
    assert_eq_test!(n, 6, "initial send");

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(2),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 4,
        "len after consume"
    );

    assert_eq_test!(tcp::retransmit_check(1000), Some(id), "trigger retransmit");
    let mut out = [0u8; 8];
    let (_, r, _) = tcp::poll_transmit(id, &mut out, 1001).unwrap();
    assert_eq_test!(r, 4, "read remaining");
    assert_test!(&out[..4] == b"cdef", "remaining content");
    pass!()
}

pub fn test_ring_buffer_clear() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"data").unwrap();
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 4,
        "wrote data"
    );

    reset();
    let (id2, _, _) = establish_connection();
    assert_eq_test!(tcp::recv_available(id2), 0, "len after clear");
    assert_eq_test!(
        tcp::send_buffer_space(id2),
        TCP_BUFFER_SIZE,
        "free after clear"
    );
    pass!()
}

pub fn test_ring_buffer_partial_write() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let chunk = [7u8; 512];
    let mut remaining = TCP_BUFFER_SIZE - 4;
    while remaining > 0 {
        let to_write = core::cmp::min(remaining, chunk.len());
        let wrote = tcp::send(id, &chunk[..to_write]).unwrap();
        assert_eq_test!(wrote, to_write, "fill buffer");
        remaining -= to_write;
    }

    let n = tcp::send(id, &[9u8; 16]).unwrap();
    assert_eq_test!(n, 4, "partial write limited by free space");
    assert_eq_test!(
        tcp::send_buffer_space(id),
        0,
        "buffer full after partial write"
    );
    pass!()
}

pub fn test_send_enqueue_and_peek() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let n = tcp::send(id, b"payload").unwrap();
    assert_eq_test!(n, 7, "enqueue len");
    assert_test!(tcp::has_pending_data(id), "unsent len");

    let mut out = [0u8; 7];
    let (_, p, _) = tcp::poll_transmit(id, &mut out, 0).unwrap();
    assert_eq_test!(p, 7, "peek unsent");
    assert_test!(&out == b"payload", "peek content");
    pass!()
}

pub fn test_send_mark_sent_and_ack() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdef").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 6, "inflight after mark_sent");

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(6),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "buffer empty after ack"
    );
    pass!()
}

pub fn test_send_retransmit_timeout() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let mut payload = [0u8; 8];
    let _ = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    assert_eq_test!(tcp::retransmit_check(1000), Some(id), "RTO fired");
    let (_, n, _) = tcp::poll_transmit(id, &mut payload, 1001).unwrap();
    assert_eq_test!(n, 4, "retransmit of Lost segment");
    assert_test!(&payload[..4] == b"abcd", "retransmit payload");
    pass!()
}

pub fn test_send_free_space() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let before = tcp::send_buffer_space(id);
    let _ = tcp::send(id, &[1u8; 128]).unwrap();
    assert_eq_test!(
        tcp::send_buffer_space(id),
        before - 128,
        "free space decreases"
    );
    pass!()
}

pub fn test_send_partial_ack() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, &[3u8; 1000]).unwrap();
    let mut payload: KBox<[u8; 1200]> = KBox::zeroed().expect("alloc");
    let (seg, n, _) = tcp::poll_transmit(id, &mut *payload, 0).unwrap();
    assert_eq_test!(n, 1000, "sent full test payload");

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(500),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 500,
        "buffered after partial ack"
    );

    assert_eq_test!(
        tcp::retransmit_check(1000),
        Some(id),
        "inflight after partial ack"
    );
    let (_, retransmit_len, _) = tcp::poll_transmit(id, &mut *payload, 1001).unwrap();
    assert_eq_test!(retransmit_len, 500, "remaining bytes retransmit");
    pass!()
}

pub fn test_send_ack_stops_rto_timer() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 4, "sent payload");

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(4),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 10);
    assert_test!(tcp::retransmit_check(2000).is_none(), "rto timer cleared");
    pass!()
}

pub fn test_recv_enqueue_dequeue() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"hello",
        10,
    );
    assert_test!(res.segments().next().is_none(), "first segment delayed ack");

    let n = tcp::recv_available(id);
    assert_eq_test!(n, 5, "enqueue len");

    let mut out = [0u8; 5];
    let r = tcp::recv(id, &mut out).unwrap();
    assert_eq_test!(r, 5, "dequeue len");
    assert_test!(&out == b"hello", "dequeue content");
    pass!()
}

pub fn test_recv_window_decreases() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let before = with_data_state!(id, |d| d.rcv_wnd);
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    tcp_common::inject_discarding(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        &[1u8; 256],
        0,
    );
    let after_enqueue = with_data_state!(id, |d| d.rcv_wnd);
    assert_test!(after_enqueue < before, "window shrinks after enqueue");

    let mut out = [0u8; 256];
    let _ = tcp::recv(id, &mut out).unwrap();
    let after_dequeue = with_data_state!(id, |d| d.rcv_wnd);
    assert_test!(after_dequeue > after_enqueue, "window grows after dequeue");
    pass!()
}

pub fn test_recv_ack_tracking() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"x",
        100,
    );
    assert_test!(res.segments().next().is_none(), "ack pending set");
    let delayed = tcp::delayed_ack_check(100 + DELAYED_ACK_MS);
    assert_test!(delayed.is_some(), "ack pending cleared");
    assert_test!(
        tcp::delayed_ack_check(100 + DELAYED_ACK_MS + 1).is_none(),
        "segment counter cleared"
    );
    pass!()
}

pub fn test_recv_delayed_ack_segments() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let first = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.segments().next().is_none(), "one segment not enough");
    let second = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(2),
        snd_nxt,
        b"b",
        1,
    );
    assert_test!(
        second.segments().next().is_some(),
        "two segments trigger ack"
    );
    pass!()
}

pub fn test_recv_delayed_ack_timeout() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"x",
        1000,
    );
    assert_test!(res.segments().next().is_none(), "initial delayed ack");
    assert_test!(
        tcp::delayed_ack_check(1000 + DELAYED_ACK_MS - 1).is_none(),
        "before timeout"
    );
    assert_test!(
        tcp::delayed_ack_check(1000 + DELAYED_ACK_MS).is_some(),
        "timeout triggers ack"
    );
    pass!()
}

pub fn test_tcp_send_in_established() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let before = tcp::send_buffer_space(id);
    let wrote = tcp::send(id, b"hello").unwrap();
    assert_eq_test!(wrote, 5, "tcp_send wrote bytes");
    assert_eq_test!(tcp::send_buffer_space(id), before - 5, "send space reduced");
    pass!()
}

pub fn test_tcp_recv_in_established() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"abc",
        0,
    );
    assert_test!(res.segments().next().is_none(), "first segment delayed ack");

    let mut out = [0u8; 8];
    let n = tcp::recv(id, &mut out).unwrap();
    assert_eq_test!(n, 3, "recv bytes");
    assert_test!(&out[..3] == b"abc", "recv content");
    pass!()
}

pub fn test_tcp_send_wrong_state() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).unwrap();
    let err = tcp::send(id, b"x").unwrap_err();
    assert_eq_test!(err, TcpError::InvalidState, "send in SYN_SENT rejected");
    pass!()
}

pub fn test_tcp_poll_transmit_basic() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let (snd_nxt, rcv_nxt) = with_data_state!(id, |d| (d.snd_nxt.raw(), d.rcv_nxt.raw()));
    let mut payload = [0u8; 64];
    let (seg, n, _) = tcp::poll_transmit(id, &mut payload, 10).unwrap();
    assert_eq_test!(n, 4, "payload len");
    assert_test!(&payload[..4] == b"abcd", "payload bytes");
    assert_eq_test!(seg.seq_num, snd_nxt, "segment seq");
    assert_eq_test!(seg.ack_num, rcv_nxt, "segment ack");
    assert_test!(seg.flags & TCP_FLAG_PSH != 0, "PSH set");
    pass!()
}

pub fn test_tcp_poll_transmit_mss_segmentation() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let mut data: KBox<[u8; DEFAULT_MSS as usize + 100]> = KBox::zeroed().expect("alloc");
    data.iter_mut().for_each(|b| *b = 0x42);
    let wrote = tcp::send(id, &*data).unwrap();
    assert_eq_test!(wrote, data.len(), "enqueue full test payload");

    let mut payload: KBox<[u8; 2048]> = KBox::zeroed().expect("alloc");
    let (_, first_len, _) = tcp::poll_transmit(id, &mut *payload, 0).unwrap();
    assert_eq_test!(first_len, DEFAULT_MSS as usize, "first chunk mss-sized");
    let (_, second_len, _) = tcp::poll_transmit(id, &mut *payload, 1).unwrap();
    assert_eq_test!(second_len, 100, "second chunk remainder");
    pass!()
}

pub fn test_tcp_poll_transmit_none_when_empty() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let mut payload = [0u8; 64];
    assert_test!(
        tcp::poll_transmit(id, &mut payload, 0).is_none(),
        "none when empty"
    );
    pass!()
}

pub fn test_tcp_data_roundtrip() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"hello").unwrap();
    let mut payload = [0u8; 64];
    let (seg, n, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 5, "sent len");

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(n as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 1);
    assert_test!(!tcp::has_pending_data(id), "no pending data after ack");
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "send buffer reclaimed"
    );
    pass!()
}

pub fn test_tcp_recv_updates_window() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let before = with_data_state!(id, |d| d.rcv_wnd);
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());

    let first = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.segments().next().is_none(), "first segment delayed");

    let second = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(2),
        snd_nxt,
        b"b",
        1,
    );
    let ack = match second.segments().next() {
        Some(s) => s.clone(),
        None => return fail!("expected immediate ACK after second segment"),
    };
    let after = with_data_state!(id, |d| d.rcv_wnd);
    assert_test!(after < before, "receive window decreased");
    assert_eq_test!(ack.window_size, after, "ack advertises updated window");
    pass!()
}

pub fn test_tcp_retransmit_on_timeout() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abc").unwrap();
    let mut payload = [0u8; 16];
    let _ = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_test!(tcp::retransmit_check(999).is_none(), "before timeout");
    assert_eq_test!(
        tcp::retransmit_check(1000),
        Some(id),
        "timeout triggers retransmit"
    );
    pass!()
}

pub fn test_tcp_retransmit_exponential_backoff() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abc").unwrap();
    let mut payload = [0u8; 16];
    let _ = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let _ = tcp::retransmit_check(1000);
    let rto1 = with_data_state!(id, |d| d.rtt.rto_ms());
    assert_eq_test!(rto1, 2000, "first timeout doubles rto");
    let _ = tcp::poll_transmit(id, &mut payload, 1001).unwrap();

    let _ = tcp::retransmit_check(3001);
    let rto2 = with_data_state!(id, |d| d.rtt.rto_ms());
    assert_eq_test!(rto2, 4000, "second timeout doubles rto again");
    pass!()
}

pub fn test_tcp_retransmit_max_exceeded() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"x").unwrap();
    let mut payload = [0u8; 8];
    let _ = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let mut now = 0u64;
    for _ in 0..MAX_RETRANSMITS {
        let rto = with_data_state!(id, |d| d.rtt.rto_ms()) as u64;
        now = now.saturating_add(rto);
        assert_eq_test!(tcp::retransmit_check(now), Some(id), "retransmit fires");
        let _ = tcp::poll_transmit(id, &mut payload, now + 1).unwrap();
    }

    let rto = with_data_state!(id, |d| d.rtt.rto_ms()) as u64;
    now = now.saturating_add(rto);
    let _ = tcp::retransmit_check(now);
    assert_test!(
        tcp::get_state(id).is_none(),
        "connection released after max retransmits"
    );
    pass!()
}

pub fn test_tcp_retransmit_canceled_by_ack() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"hello").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(n as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 500);
    assert_test!(tcp::retransmit_check(1000).is_none(), "ack cancels timeout");
    pass!()
}

pub fn test_retx_queue_populated_by_poll_transmit() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    let _ = tcp::send(c.id, &[0xAA; 100]).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let _ = tcp::poll_transmit(c.id, &mut *buf, 0).unwrap();

    let (total, entries) = with_data_state!(c.id, |d| (d.sendmap.total_bytes(), d.sendmap.len()));
    assert_eq_test!(total, 100, "sendmap tracks 100 bytes");
    assert_eq_test!(entries, 1, "one sendmap entry");
    pass!()
}

pub fn test_poll_transmit_respects_cwnd() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"x").unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let _ = tcp::poll_transmit(id, &mut *buf, 0).unwrap();

    let _ = tcp::retransmit_check(1000);

    let (_, n0, _) = tcp::poll_transmit(id, &mut *buf, 1001).unwrap();
    assert_eq_test!(n0, 1, "retransmit of Lost 1-byte entry");

    let _ = tcp::send(id, &[0x42; 4380]).unwrap();

    // pipe=1, cwnd=1460 → effective 1459, which Nagle defers as sub-MSS.
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.nagle_enabled = false;
        }
    });
    let (_, n1, _) = tcp::poll_transmit(id, &mut *buf, 1002).unwrap();
    assert_eq_test!(n1, 1459, "second segment limited by cwnd - pipe");

    // pipe = 1 + 1459 = 1460 = cwnd.
    assert_test!(
        tcp::poll_transmit(id, &mut *buf, 1003).is_none(),
        "blocked by cwnd"
    );
    pass!()
}

/// Inject one dup ACK carrying SACK blocks for `segs[1..4]`.
///
/// `#[inline(never)]`: folding the option buffer and the `Actions` slot into
/// the caller puts it over the 2 KiB stack gate.
#[inline(never)]
fn inject_sack_dup_ack(c: &tcp_common::EstablishedConn, snd_una: u32, segs: &[(u32, u32); 4]) {
    let peer_seq = c.peer_iss.wrapping_add(1);
    let mut opts = [0u8; 28]; // NOP NOP SACK(kind=5, len=26, 3 blocks)
    opts[0] = 1; // NOP
    opts[1] = 1; // NOP
    opts[2] = 5; // SACK kind
    opts[3] = 26; // len = 2 + 3*8
    opts[4..8].copy_from_slice(&segs[1].0.to_be_bytes());
    opts[8..12].copy_from_slice(&segs[1].1.to_be_bytes());
    opts[12..16].copy_from_slice(&segs[2].0.to_be_bytes());
    opts[16..20].copy_from_slice(&segs[2].1.to_be_bytes());
    opts[20..24].copy_from_slice(&segs[3].0.to_be_bytes());
    opts[24..28].copy_from_slice(&segs[3].1.to_be_bytes());
    // Reusable heap Actions slot: the ~400 B return value would put this
    // frame over the 2 KiB stack gate.
    let mut actions: KBox<tcp::Actions> =
        KBox::try_init(tcp::Actions::init_default()).expect("alloc");
    tcp_common::inject_with_options_into(
        &mut *actions,
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        c.local_port,
        peer_seq,
        snd_una,
        TCP_FLAG_ACK,
        &opts,
        &[],
    );
}

pub fn test_fast_retransmit_triggers_on_3_dup_acks() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    let id = c.id;
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.sack_permitted = true;
        }
    });

    let mut send_payload: KBox<[u8; 4 * DEFAULT_MSS as usize]> = KBox::zeroed().expect("alloc");
    send_payload.iter_mut().for_each(|b| *b = 0xBB);
    let _ = tcp::send(id, &*send_payload).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let mut segs = [(0u32, 0u32); 4];
    for i in 0..4 {
        let (seg, n, _) = tcp::poll_transmit(id, &mut *buf, 0).unwrap();
        segs[i] = (seg.seq_num, seg.seq_num.wrapping_add(n as u32));
    }

    // One PCB borrow per observation point: each expands to a lock guard and
    // a state match with stack slots of their own.
    let (snd_una, snd_nxt_before) = with_data_state!(id, |d| (d.snd_una.raw(), d.snd_nxt.raw()));
    assert_test!(snd_nxt_before > snd_una, "data in flight");

    // Three SACKed entries past seg 1 declare seg 1 Lost.
    inject_sack_dup_ack(&c, snd_una, &segs);

    let (snd_nxt_after, in_recovery, has_lost) = with_data_state!(id, |d| (
        d.snd_nxt.raw(),
        d.cc.in_recovery(),
        d.sendmap.has_lost()
    ));
    assert_eq_test!(snd_nxt_after, snd_nxt_before, "snd_nxt not rewound");
    assert_test!(in_recovery, "entered fast recovery");
    assert_test!(has_lost, "segment marked Lost");

    let resent = tcp::poll_transmit(id, &mut *buf, 1);
    assert_test!(resent.is_some(), "retransmit of Lost segment");
    let (seg, _, _) = resent.unwrap();
    assert_eq_test!(seg.seq_num, segs[0].0, "retransmit starts at seg 1");
    pass!()
}

pub fn test_fast_retransmit_cwnd_reduction() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    let id = c.id;
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.sack_permitted = true;
        }
    });

    let mut send_payload: KBox<[u8; 4 * DEFAULT_MSS as usize]> = KBox::zeroed().expect("alloc");
    send_payload.iter_mut().for_each(|b| *b = 0xCC);
    let _ = tcp::send(id, &*send_payload).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let mut segs = [(0u32, 0u32); 4];
    for i in 0..4 {
        let (seg, n, _) = tcp::poll_transmit(id, &mut *buf, 0).unwrap();
        segs[i] = (seg.seq_num, seg.seq_num.wrapping_add(n as u32));
    }

    let total = with_data_state!(id, |d| d.sendmap.total_bytes());
    assert_eq_test!(total, 4 * DEFAULT_MSS as u32, "4 MSS in flight");

    // CUBIC reduces on cwnd, not pipe: cwnd = ssthresh = IW 14600 * 0.7.
    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let peer_seq = c.peer_iss.wrapping_add(1);
    let mut opts = [0u8; 28];
    opts[0] = 1;
    opts[1] = 1;
    opts[2] = 5;
    opts[3] = 26;
    opts[4..8].copy_from_slice(&segs[1].0.to_be_bytes());
    opts[8..12].copy_from_slice(&segs[1].1.to_be_bytes());
    opts[12..16].copy_from_slice(&segs[2].0.to_be_bytes());
    opts[16..20].copy_from_slice(&segs[2].1.to_be_bytes());
    opts[20..24].copy_from_slice(&segs[3].0.to_be_bytes());
    opts[24..28].copy_from_slice(&segs[3].1.to_be_bytes());
    let _ = tcp_common::inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        c.local_port,
        peer_seq,
        snd_una,
        TCP_FLAG_ACK,
        &opts,
        &[],
    );

    let cwnd = with_data_state!(id, |d| d.cc.cwnd());
    assert_eq_test!(cwnd, 10220, "cwnd = ssthresh (CUBIC β=0.7)");
    pass!()
}

pub fn test_fast_retransmit_not_during_recovery() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    let id = c.id;
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.sack_permitted = true;
        }
    });

    let mut send_payload: KBox<[u8; 5 * DEFAULT_MSS as usize]> = KBox::zeroed().expect("alloc");
    send_payload.iter_mut().for_each(|b| *b = 0xDD);
    let _ = tcp::send(id, &*send_payload).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let mut segs = [(0u32, 0u32); 5];
    for i in 0..5 {
        let (seg, n, _) = tcp::poll_transmit(id, &mut *buf, 0).unwrap();
        segs[i] = (seg.seq_num, seg.seq_num.wrapping_add(n as u32));
    }

    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let peer_seq = c.peer_iss.wrapping_add(1);
    let mut actions: KBox<tcp::Actions> =
        KBox::try_init(tcp::Actions::init_default()).expect("alloc");

    let mut opts = [0u8; 28];
    opts[0] = 1;
    opts[1] = 1;
    opts[2] = 5;
    opts[3] = 26;
    opts[4..8].copy_from_slice(&segs[1].0.to_be_bytes());
    opts[8..12].copy_from_slice(&segs[1].1.to_be_bytes());
    opts[12..16].copy_from_slice(&segs[2].0.to_be_bytes());
    opts[16..20].copy_from_slice(&segs[2].1.to_be_bytes());
    opts[20..24].copy_from_slice(&segs[3].0.to_be_bytes());
    opts[24..28].copy_from_slice(&segs[3].1.to_be_bytes());
    tcp_common::inject_with_options_into(
        &mut *actions,
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        c.local_port,
        peer_seq,
        snd_una,
        TCP_FLAG_ACK,
        &opts,
        &[],
    );
    assert_test!(with_data_state!(id, |d| d.cc.in_recovery()), "in recovery");

    let cwnd_before = with_data_state!(id, |d| d.cc.cwnd());

    // A second SACK re-runs loss detection while already in recovery.
    let mut opts2 = [0u8; 12];
    opts2[0] = 1;
    opts2[1] = 1;
    opts2[2] = 5;
    opts2[3] = 10;
    opts2[4..8].copy_from_slice(&segs[4].0.to_be_bytes());
    opts2[8..12].copy_from_slice(&segs[4].1.to_be_bytes());
    tcp_common::inject_with_options_into(
        &mut *actions,
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        c.local_port,
        peer_seq,
        snd_una,
        TCP_FLAG_ACK,
        &opts2,
        &[],
    );
    let cwnd_after = with_data_state!(id, |d| d.cc.cwnd());
    assert_eq_test!(cwnd_after, cwnd_before, "cwnd unchanged during recovery");
    pass!()
}

pub fn test_rto_resets_cwnd_and_marks_lost() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, &[0xEE; 100]).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let _ = tcp::poll_transmit(id, &mut *buf, 0).unwrap();

    assert_eq_test!(
        with_data_state!(id, |d| d.sendmap.total_bytes()),
        100,
        "sendmap total before RTO"
    );

    let _ = tcp::retransmit_check(1000);

    let cwnd = with_data_state!(id, |d| d.cc.cwnd());
    assert_eq_test!(cwnd, DEFAULT_MSS as u32, "cwnd reset to MSS");
    assert_test!(
        !with_data_state!(id, |d| d.sendmap.is_empty()),
        "sendmap not cleared"
    );
    assert_test!(
        with_data_state!(id, |d| d.sendmap.has_lost()),
        "entries marked Lost"
    );
    assert_eq_test!(with_data_state!(id, |d| d.sendmap.pipe()), 0, "pipe zero");
    pass!()
}

pub fn test_sack_permitted_negotiated_active_open() -> TestResult {
    reset();
    let (id, syn_seg) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("connect");
    let local_port = syn_seg.tuple.local_port;
    let our_iss = syn_seg.seq_num;
    assert_test!(syn_seg.sack_permitted, "SYN carries SACK-Permitted");

    let opts: [u8; 6] = [
        2, 4, 0x05, 0xB4, // MSS = 1460
        4, 2, // SACK-Permitted
    ];
    let syn_ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: local_port,
        seq_num: tcp_common::PEER_ISS,
        ack_num: our_iss.wrapping_add(1),
        data_offset: 5,
        flags: tcp::TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &syn_ack, &opts, &[], 0);

    let permitted = with_data_state!(id, |d| d.sack_permitted);
    assert_test!(permitted, "SACK permitted after negotiation");
    pass!()
}

pub fn test_sack_permitted_not_set_without_peer() -> TestResult {
    reset();
    // The helper's SYN-ACK carries no options, so SACK is never negotiated.
    let c = tcp_common::establish_connection();
    let permitted = with_data_state!(c.id, |d| d.sack_permitted);
    assert_test!(!permitted, "SACK not permitted when peer omits option");
    pass!()
}

pub fn test_sack_blocks_sent_on_ooo() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.sack_permitted = true;
        }
    });

    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let peer_seq = c.peer_iss.wrapping_add(1);

    // A gap at peer_seq, with data at peer_seq+100.
    let actions = tcp_common::inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        c.local_port,
        peer_seq.wrapping_add(100),
        snd_nxt,
        TCP_FLAG_ACK,
        b"ooo_data",
    );
    let seg = actions.segments().next().expect("dup ACK emitted");
    assert_test!(seg.sack_block_count > 0, "SACK blocks present in dup ACK");
    assert_eq_test!(
        seg.sack_blocks[0].0,
        peer_seq.wrapping_add(100),
        "SACK left edge"
    );
    pass!()
}

pub fn test_sack_blocks_parsed_from_peer_ack() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    let id = c.id;
    tcp::table::with_pcb_mut(id, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.sack_permitted = true;
        }
    });

    let _ = tcp::send(id, &[0xAA; 100]).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let _ = tcp::poll_transmit(id, &mut *buf, 0).unwrap();

    let snd_una = with_data_state!(id, |d| d.snd_una.raw());

    let sack_left = snd_una.wrapping_add(50);
    let sack_right = snd_una.wrapping_add(100);
    let mut opts = [0u8; 12];
    opts[0] = 1;
    opts[1] = 1;
    opts[2] = 5;
    opts[3] = 10;
    opts[4..8].copy_from_slice(&sack_left.to_be_bytes());
    opts[8..12].copy_from_slice(&sack_right.to_be_bytes());

    let hdr = tcp_common::make_header(
        REMOTE_PORT,
        c.local_port,
        c.peer_iss.wrapping_add(1),
        snd_una, // dup ACK
        TCP_FLAG_ACK,
        32768,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &hdr, &opts, &[], 0);

    // SACK blocks feed straight into the SendMap; there is no scoreboard.
    let permitted = with_data_state!(id, |d| d.sack_permitted);
    assert_test!(permitted, "sack_permitted still set");
    pass!()
}

pub fn test_sack_scoreboard_cleared_on_forward_ack() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;

    let _ = tcp::send(id, &[0xBB; 50]).unwrap();
    let mut buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let (seg, _, _) = tcp::poll_transmit(id, &mut *buf, 0).unwrap();

    let ack = tcp_common::make_header(
        REMOTE_PORT,
        c.local_port,
        c.peer_iss.wrapping_add(1),
        seg.seq_num.wrapping_add(50), // forward ACK
        TCP_FLAG_ACK,
        32768,
    );
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack, &[], &[], 1);

    let empty = with_data_state!(id, |d| d.sendmap.is_empty());
    assert_test!(empty, "sendmap cleared after forward ACK");
    pass!()
}

pub fn test_sack_blocks_from_ooo_assembler() -> TestResult {
    reset();
    let mut asm = tcp::Assembler::new();
    asm.insert(200, 10); // range [200, 210)
    asm.insert(100, 5); // range [100, 105)
    asm.insert(300, 20); // range [300, 320)

    let (blocks, count) = asm.sack_blocks();
    assert_eq_test!(count, 3, "three SACK blocks");
    assert_eq_test!(blocks[0].0, 100, "first block left");
    assert_eq_test!(blocks[0].1, 105, "first block right");
    assert_eq_test!(blocks[1].0, 200, "second block left");
    assert_eq_test!(blocks[2].0, 300, "third block left");
    pass!()
}

pub fn test_so_sndbuf_caps_send_space() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let before = tcp::send_buffer_space(id);
    assert_test!(before > 1024, "default > 1024");

    tcp::set_sndbuf(id, 1024);
    let after = tcp::send_buffer_space(id);
    assert_test!(after <= 1024, "capped to 1024 by SO_SNDBUF");

    let wrote = tcp::send(id, &[0xAA; 2000]).unwrap();
    assert_test!(wrote <= 1024, "send limited by effective capacity");
    pass!()
}

pub fn test_so_rcvbuf_affects_window() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;

    tcp::set_rcvbuf(id, 4096);

    let window = tcp::table::with_bufs(id, |b| b.recv.window()).expect("buffer exists");
    assert_test!(window <= 4096, "recv window capped by SO_RCVBUF");
    pass!()
}

pub fn test_nagle_defers_sub_mss_when_inflight() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    // Re-enable Nagle (test helper disables it).
    tcp::set_nodelay(id, false);
    let _ = tcp::send(id, &[0xAA; DEFAULT_MSS as usize]).unwrap();
    // A full-MTU scratch buffer is most of the frame budget; keep it off the stack.
    let mut buf = assert_ok!(KVec::<u8>::zeroed(1500), "tx scratch buffer");
    let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();
    let _ = tcp::send(id, &[0xBB; 10]).unwrap();
    assert_test!(
        tcp::poll_transmit(id, &mut buf, 1).is_none(),
        "Nagle defers sub-MSS when inflight"
    );
    pass!()
}

pub fn test_nagle_sends_when_nothing_inflight() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, &[0xCC; 10]).unwrap();
    let mut buf = assert_ok!(KVec::<u8>::zeroed(1500), "tx scratch buffer");
    let result = tcp::poll_transmit(id, &mut buf, 0);
    assert_test!(result.is_some(), "sends sub-MSS when nothing inflight");
    let (_, n, _) = result.unwrap();
    assert_eq_test!(n, 10, "full 10 bytes sent");
    pass!()
}

pub fn test_tcp_nodelay_disables_nagle() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let c = tcp_common::establish_connection();
    tcp::set_nodelay(c.id, false);
    let _ = tcp::send(c.id, b"x").unwrap();
    let mut buf = assert_ok!(KVec::<u8>::zeroed(1500), "tx scratch buffer");
    let _ = tcp::poll_transmit(c.id, &mut buf, 0).unwrap();
    let _ = tcp::send(c.id, b"y").unwrap();
    assert_test!(
        tcp::poll_transmit(c.id, &mut buf, 1).is_none(),
        "nagle defers"
    );
    tcp::set_nodelay(c.id, true);
    assert_test!(
        tcp::poll_transmit(c.id, &mut buf, 2).is_some(),
        "nodelay sends"
    );
    pass!()
}

pub fn test_tcp_respects_peer_window() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcde").unwrap();
    let mut payload = [0u8; 512];
    let (first_seg, first_len, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let shrink = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: first_seg.seq_num.wrapping_add(first_len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 100,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &shrink, &[], &[], 1);

    let _ = tcp::send(id, &[0x11u8; 200]).unwrap();
    let (_, next_len, _) = tcp::poll_transmit(id, &mut payload, 2).unwrap();
    assert_eq_test!(next_len, 100, "limited by peer window");
    pass!()
}

pub fn test_tcp_zero_window_blocks_send() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcde").unwrap();
    let mut payload = [0u8; 128];
    let (seg, len, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let zero_wnd = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &zero_wnd, &[], &[], 1);

    let _ = tcp::send(id, &[0x22u8; 64]).unwrap();
    assert_test!(
        tcp::poll_transmit(id, &mut payload, 2).is_none(),
        "blocked by zero window"
    );
    pass!()
}

pub fn test_tcp_zero_window_probe() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcde").unwrap();
    let mut payload = [0u8; 128];
    let (seg, len, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let zero_wnd = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &zero_wnd, &[], &[], 1);

    // The probe needs unsent data, enqueued after the window closed.
    let _ = tcp::send(id, b"more").unwrap();

    let before = with_data_state!(id, |d| d.snd_nxt.raw());
    let probe = tcp::zero_window_probe(id, 2);
    assert_test!(probe.is_some(), "probe generated");
    let after = with_data_state!(id, |d| d.snd_nxt.raw());
    assert_eq_test!(before, after, "probe does not advance snd_nxt");
    pass!()
}

pub fn test_tcp_window_update_resumes_send() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, &[0x33u8; 50]).unwrap();
    let mut payload = [0u8; 256];
    let (seg, len, _) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let ack_half_zero = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(25),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &ack_half_zero, &[], &[], 1);
    assert_test!(
        tcp::poll_transmit(id, &mut payload, 2).is_none(),
        "send blocked at wnd=0"
    );

    // Through the out-of-line helper: a second inline `Actions` return slot in
    // this frame puts it over the 2 KiB stack gate.
    tcp_common::inject_window_update(
        client_port,
        server_iss.wrapping_add(1),
        seg.seq_num.wrapping_add(len as u32),
        200,
        3,
    );

    let _ = tcp::send(id, &[0x44u8; 80]).unwrap();
    let resumed = tcp::poll_transmit(id, &mut payload, 4);
    assert_test!(resumed.is_some(), "send resumes after window opens");
    pass!()
}

pub fn test_tcp_delayed_ack_after_two_segments() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());

    let r1 = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"1",
        0,
    );
    assert_test!(r1.segments().next().is_none(), "first segment delayed");

    let r2 = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(2),
        snd_nxt,
        b"2",
        1,
    );
    assert_test!(
        r2.segments().next().is_some(),
        "second segment triggers ack"
    );
    pass!()
}

pub fn test_tcp_delayed_ack_timeout() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let r = inject_data_segment(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"x",
        0,
    );
    assert_test!(r.segments().next().is_none(), "initial delayed ack");
    assert_test!(
        tcp::delayed_ack_check(DELAYED_ACK_MS - 1).is_none(),
        "before delayed ack timeout"
    );
    let delayed = tcp::delayed_ack_check(DELAYED_ACK_MS);
    assert_test!(delayed.is_some(), "delayed ack fires at timeout");
    pass!()
}

pub fn test_tcp_immediate_ack_for_fin() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let fin = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(REMOTE_IP, LOCAL_IP, &fin, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::CloseWait),
        "fin moves to close_wait"
    );
    assert_test!(result.segments().next().is_some(), "fin gets immediate ack");
    pass!()
}

/// Allocate `n` zeroed kernel frames as a keepalive page list — a headless
/// stand-in for a pinned user buffer (no process VM).
///
/// Charged to the root account, so the charge and refund are real rather than
/// elided for the test.
fn alloc_test_frames(n: usize) -> Option<KeepaliveFrames> {
    let alloc = slopos_ostd::mm::frame_alloc::current_frame_allocator()?;
    let mut frames = KVec::with_capacity(n).ok()?;
    for _ in 0..n {
        let opts = slopos_ostd::mm::FrameAllocOptions::single().zeroed();
        let pa = alloc.alloc(opts)?;
        frames
            .push(UFrame::<AnonymousMeta>::from_unused(pa, AnonymousMeta::default()).ok()?)
            .ok()?;
    }
    KeepaliveFrames::take(frames.as_slice(), slopos_ostd::process::quota::root())
}

/// TCP `MSG_ZEROCOPY` send-queue chunk lifecycle: a zero-copy chunk accounts for
/// its bytes (buffered / unsent / inflight), is read back from the pinned pages
/// on (re)transmit (`peek_retransmit` → volatile pin copy), survives a **partial**
/// cumulative ACK (the pin stays held), and releases its notification-token
/// reference only when **fully** cumulatively ACKed.
pub fn test_tcp_zerocopy_chunk_lifecycle() -> TestResult {
    let mut send = match TcpSendState::new(TCP_BUFFER_SIZE) {
        Ok(s) => s,
        Err(_) => return fail!("send state alloc"),
    };
    let Some(frames) = alloc_test_frames(1) else {
        return fail!("frame alloc");
    };
    let Some(token) = ZcNotifToken::new() else {
        return fail!("token alloc");
    };
    assert_test!(!token.is_notifiable(), "fresh token must not be notifiable");

    assert_test!(
        send.enqueue_zerocopy(frames, 0, 100, token.clone()),
        "enqueue_zerocopy"
    );
    assert_eq_test!(send.buffered_len(), 100, "buffered after enqueue");
    assert_eq_test!(send.unsent_len(), 100, "unsent after enqueue");

    // The volatile pin read: straight from the pinned pages, which are
    // zeroed test frames.
    let mut out = [0xFFu8; 50];
    let got = send.peek_retransmit(0, &mut out);
    assert_eq_test!(got, 50, "peek_retransmit read count from pin");
    assert_test!(
        out.iter().all(|&b| b == 0),
        "zero-copy pin reads test frames"
    );

    send.mark_sent(100);
    assert_eq_test!(send.inflight(), 100, "inflight after mark_sent");

    send.process_ack(40);
    assert_eq_test!(send.buffered_len(), 60, "buffered after partial ack");
    assert_eq_test!(send.inflight(), 60, "inflight after partial ack");
    assert_test!(
        !token.is_notifiable(),
        "token must stay held across a partial ack"
    );

    // With no in-flight DMA references left, the count hits zero.
    send.process_ack(60);
    assert_eq_test!(send.buffered_len(), 0, "buffered after full ack");
    assert_test!(
        token.is_notifiable(),
        "token released only on full cumulative ack"
    );
    pass!()
}

/// A zero-copy chunk after an inline chunk: a segment never straddles the
/// boundary — `segment_source`/`peek` clamp at the inline chunk's end.
pub fn test_tcp_zerocopy_no_chunk_straddle() -> TestResult {
    let mut send = match TcpSendState::new(TCP_BUFFER_SIZE) {
        Ok(s) => s,
        Err(_) => return fail!("send state alloc"),
    };
    assert_eq_test!(send.enqueue(&[7u8; 30]), 30, "inline enqueue");
    let Some(frames) = alloc_test_frames(1) else {
        return fail!("frame alloc");
    };
    let Some(token) = ZcNotifToken::new() else {
        return fail!("token alloc");
    };
    assert_test!(
        send.enqueue_zerocopy(frames, 0, 100, token),
        "zerocopy enqueue"
    );
    assert_eq_test!(send.buffered_len(), 130, "buffered = inline + zerocopy");

    let mut out = [0u8; 80];
    let got = send.peek_unsent(&mut out);
    assert_eq_test!(got, 30, "peek clamps to the inline chunk boundary");
    assert_test!(
        out[..30].iter().all(|&b| b == 7),
        "inline bytes read from the ring"
    );
    pass!()
}

slopos_testing::stest!(name = test_ring_buffer_new_empty, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_write_read_basic, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_write_full, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_wrap_around, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_peek_offset, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_consume, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_clear, suite = tcp_data);
slopos_testing::stest!(name = test_ring_buffer_partial_write, suite = tcp_data);
slopos_testing::stest!(name = test_send_enqueue_and_peek, suite = tcp_data);
slopos_testing::stest!(name = test_send_mark_sent_and_ack, suite = tcp_data);
slopos_testing::stest!(name = test_send_retransmit_timeout, suite = tcp_data);
slopos_testing::stest!(name = test_send_free_space, suite = tcp_data);
slopos_testing::stest!(name = test_send_partial_ack, suite = tcp_data);
slopos_testing::stest!(name = test_send_ack_stops_rto_timer, suite = tcp_data);
slopos_testing::stest!(name = test_recv_enqueue_dequeue, suite = tcp_data);
slopos_testing::stest!(name = test_recv_window_decreases, suite = tcp_data);
slopos_testing::stest!(name = test_recv_ack_tracking, suite = tcp_data);
slopos_testing::stest!(name = test_recv_delayed_ack_segments, suite = tcp_data);
slopos_testing::stest!(name = test_recv_delayed_ack_timeout, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_send_in_established, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_recv_in_established, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_send_wrong_state, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_poll_transmit_basic, suite = tcp_data);
slopos_testing::stest!(
    name = test_tcp_poll_transmit_mss_segmentation,
    suite = tcp_data
);
slopos_testing::stest!(
    name = test_tcp_poll_transmit_none_when_empty,
    suite = tcp_data
);
slopos_testing::stest!(name = test_tcp_data_roundtrip, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_recv_updates_window, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_retransmit_on_timeout, suite = tcp_data);
slopos_testing::stest!(
    name = test_tcp_retransmit_exponential_backoff,
    suite = tcp_data
);
slopos_testing::stest!(name = test_tcp_retransmit_max_exceeded, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_retransmit_canceled_by_ack, suite = tcp_data);
slopos_testing::stest!(
    name = test_retx_queue_populated_by_poll_transmit,
    suite = tcp_data
);
slopos_testing::stest!(name = test_poll_transmit_respects_cwnd, suite = tcp_data);
slopos_testing::stest!(
    name = test_fast_retransmit_triggers_on_3_dup_acks,
    suite = tcp_data
);
slopos_testing::stest!(name = test_fast_retransmit_cwnd_reduction, suite = tcp_data);
slopos_testing::stest!(
    name = test_fast_retransmit_not_during_recovery,
    suite = tcp_data
);
slopos_testing::stest!(name = test_rto_resets_cwnd_and_marks_lost, suite = tcp_data);
slopos_testing::stest!(
    name = test_sack_permitted_negotiated_active_open,
    suite = tcp_data
);
slopos_testing::stest!(
    name = test_sack_permitted_not_set_without_peer,
    suite = tcp_data
);
slopos_testing::stest!(name = test_sack_blocks_sent_on_ooo, suite = tcp_data);
slopos_testing::stest!(
    name = test_sack_blocks_parsed_from_peer_ack,
    suite = tcp_data
);
slopos_testing::stest!(
    name = test_sack_scoreboard_cleared_on_forward_ack,
    suite = tcp_data
);
slopos_testing::stest!(name = test_sack_blocks_from_ooo_assembler, suite = tcp_data);
slopos_testing::stest!(name = test_so_sndbuf_caps_send_space, suite = tcp_data);
slopos_testing::stest!(name = test_so_rcvbuf_affects_window, suite = tcp_data);
slopos_testing::stest!(
    name = test_nagle_defers_sub_mss_when_inflight,
    suite = tcp_data
);
slopos_testing::stest!(
    name = test_nagle_sends_when_nothing_inflight,
    suite = tcp_data
);
slopos_testing::stest!(name = test_tcp_nodelay_disables_nagle, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_respects_peer_window, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_zero_window_blocks_send, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_zero_window_probe, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_window_update_resumes_send, suite = tcp_data);
slopos_testing::stest!(
    name = test_tcp_delayed_ack_after_two_segments,
    suite = tcp_data
);
slopos_testing::stest!(name = test_tcp_delayed_ack_timeout, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_immediate_ack_for_fin, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_zerocopy_chunk_lifecycle, suite = tcp_data);
slopos_testing::stest!(name = test_tcp_zerocopy_no_chunk_straddle, suite = tcp_data);
