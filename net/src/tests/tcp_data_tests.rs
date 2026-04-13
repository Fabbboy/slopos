//! TCP data transfer regression tests.
//!
//! Covers: ring buffer operations, send/receive buffers, data transfer through
//! the TCP state machine, delayed ACK, retransmission, flow control, and
//! zero-window probing.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::cong::CongestionControl;
use crate::tcp::{
    self, ConnId, DEFAULT_MSS, DELAYED_ACK_MS, MAX_RETRANSMITS, TCP_BUFFER_SIZE, TCP_FLAG_ACK,
    TCP_FLAG_FIN, TCP_FLAG_PSH, TcpError, TcpHeader, TcpState,
};
use crate::tests::tcp_common::{self, reset_all as reset};
use crate::with_data_state;

/// Legacy tuple form of [`tcp_common::establish_connection`], kept so the
/// ~38 call sites below don't need destructuring rewrites.
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

// =============================================================================
// Ring Buffer
// =============================================================================

pub fn test_ring_buffer_new_empty() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    assert_eq_test!(tcp::send_buffer_space(id), TCP_BUFFER_SIZE, "capacity");
    assert_eq_test!(tcp::recv_available(id), 0, "new len");
    assert_test!(!tcp::has_pending_data(id), "new is empty");
    pass!()
}

pub fn test_ring_buffer_write_read_basic() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let n = tcp::send(id, b"hello").unwrap();
    assert_eq_test!(n, 5, "write hello");
    assert_test!(tcp::has_pending_data(id), "pending after write");

    let mut out = [0u8; 16];
    let (_, r) = tcp::poll_transmit(id, &mut out, 0).unwrap();
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
    let first = [1u8; 256];
    let mut injected = 0usize;
    while injected < half {
        let n = core::cmp::min(first.len(), half - injected);
        let _ = inject_data_segment(
            [10, 0, 0, 2],
            [10, 0, 0, 1],
            80,
            client_port,
            seq,
            snd_nxt,
            &first[..n],
            injected as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut tmp = [0u8; 256];
    let mut drained = 0usize;
    while drained < half {
        let n = tcp::recv(id, &mut tmp).unwrap();
        if n == 0 {
            return fail!("expected data while draining first half");
        }
        assert_test!(tmp[..n].iter().all(|&x| x == 1), "read content");
        drained += n;
    }
    assert_eq_test!(drained, half, "read first half");

    let second = [2u8; 256];
    injected = 0;
    while injected < half {
        let n = core::cmp::min(second.len(), half - injected);
        let _ = inject_data_segment(
            [10, 0, 0, 2],
            [10, 0, 0, 1],
            80,
            client_port,
            seq,
            snd_nxt,
            &second[..n],
            (half + injected) as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut out = [0u8; 256];
    drained = 0;
    while drained < half {
        let n = tcp::recv(id, &mut out).unwrap();
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdefgh").unwrap();

    let mut first = [0u8; 2];
    let (seg1, n1) = tcp::poll_transmit(id, &mut first, 0).unwrap();
    assert_eq_test!(n1, 2, "first chunk len");
    assert_test!(&first == b"ab", "first chunk content");

    let mut second = [0u8; 3];
    let (_, n2) = tcp::poll_transmit(id, &mut second, 1).unwrap();
    assert_eq_test!(n2, 3, "peek len");
    assert_test!(&second == b"cde", "peek offset data");

    let mut third = [0u8; 8];
    let (_, n3) = tcp::poll_transmit(id, &mut third, 2).unwrap();
    assert_eq_test!(n3, 3, "remaining chunk len");
    assert_test!(&third[..3] == b"fgh", "remaining chunk content");
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 8,
        "peek does not consume"
    );

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg1.seq_num.wrapping_add(8),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 2);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "full data preserved"
    );
    pass!()
}

pub fn test_ring_buffer_consume() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdef").unwrap();
    let mut tx = [0u8; 16];
    let (seg, n) = tcp::poll_transmit(id, &mut tx, 0).unwrap();
    assert_eq_test!(n, 6, "initial send");

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(2),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE - 4,
        "len after consume"
    );

    assert_eq_test!(tcp::retransmit_check(1000), Some(id), "trigger retransmit");
    let mut out = [0u8; 8];
    let (_, r) = tcp::poll_transmit(id, &mut out, 1001).unwrap();
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

// =============================================================================
// Send Buffer
// =============================================================================

pub fn test_send_enqueue_and_peek() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let n = tcp::send(id, b"payload").unwrap();
    assert_eq_test!(n, 7, "enqueue len");
    assert_test!(tcp::has_pending_data(id), "unsent len");

    let mut out = [0u8; 7];
    let (_, p) = tcp::poll_transmit(id, &mut out, 0).unwrap();
    assert_eq_test!(p, 7, "peek unsent");
    assert_test!(&out == b"payload", "peek content");
    pass!()
}

pub fn test_send_mark_sent_and_ack() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcdef").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 6, "inflight after mark_sent");

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(6),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "buffer empty after ack"
    );
    pass!()
}

pub fn test_send_retransmit_timeout() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let mut payload = [0u8; 8];
    let _ = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    assert_eq_test!(tcp::retransmit_check(1000), Some(id), "inflight reset");
    let (_, n) = tcp::poll_transmit(id, &mut payload, 1001).unwrap();
    assert_eq_test!(n, 4, "retransmit flag set");
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, &[3u8; 1000]).unwrap();
    let mut payload = [0u8; 1200];
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 1000, "sent full test payload");

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(500),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
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
    let (_, retransmit_len) = tcp::poll_transmit(id, &mut payload, 1001).unwrap();
    assert_eq_test!(retransmit_len, 500, "remaining bytes retransmit");
    pass!()
}

pub fn test_send_ack_stops_rto_timer() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 4, "sent payload");

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(4),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 10);
    assert_test!(tcp::retransmit_check(2000).is_none(), "rto timer cleared");
    pass!()
}

// =============================================================================
// Receive Buffer
// =============================================================================

pub fn test_recv_enqueue_dequeue() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let before = with_data_state!(id, |d| d.rcv_wnd);
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let _ = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let first = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.segments().next().is_none(), "one segment not enough");
    let second = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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

// =============================================================================
// Data Transfer Integration
// =============================================================================

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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
    reset();
    let (id, _) = tcp::connect([10, 0, 0, 1], [10, 0, 0, 2], 80).unwrap();
    let err = tcp::send(id, b"x").unwrap_err();
    assert_eq_test!(err, TcpError::InvalidState, "send in SYN_SENT rejected");
    pass!()
}

pub fn test_tcp_poll_transmit_basic() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, b"abcd").unwrap();
    let (snd_nxt, rcv_nxt) = with_data_state!(id, |d| (d.snd_nxt.raw(), d.rcv_nxt.raw()));
    let mut payload = [0u8; 64];
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 10).unwrap();
    assert_eq_test!(n, 4, "payload len");
    assert_test!(&payload[..4] == b"abcd", "payload bytes");
    assert_eq_test!(seg.seq_num, snd_nxt, "segment seq");
    assert_eq_test!(seg.ack_num, rcv_nxt, "segment ack");
    assert_test!(seg.flags & TCP_FLAG_PSH != 0, "PSH set");
    pass!()
}

pub fn test_tcp_poll_transmit_mss_segmentation() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let data = [0x42u8; DEFAULT_MSS as usize + 100];
    let wrote = tcp::send(id, &data).unwrap();
    assert_eq_test!(wrote, data.len(), "enqueue full test payload");

    let mut payload = [0u8; 2048];
    let (_, first_len) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(first_len, DEFAULT_MSS as usize, "first chunk mss-sized");
    let (_, second_len) = tcp::poll_transmit(id, &mut payload, 1).unwrap();
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
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 0).unwrap();
    assert_eq_test!(n, 5, "sent len");

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(n as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_test!(!tcp::has_pending_data(id), "no pending data after ack");
    assert_eq_test!(
        tcp::send_buffer_space(id),
        TCP_BUFFER_SIZE,
        "send buffer reclaimed"
    );
    pass!()
}

pub fn test_tcp_recv_updates_window() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let before = with_data_state!(id, |d| d.rcv_wnd);
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());

    let first = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.segments().next().is_none(), "first segment delayed");

    let second = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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

// =============================================================================
// Retransmission
// =============================================================================

pub fn test_tcp_retransmit_on_timeout() -> TestResult {
    reset();
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
    reset();
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
    reset();
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"hello").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let ack = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(n as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 500);
    assert_test!(tcp::retransmit_check(1000).is_none(), "ack cancels timeout");
    pass!()
}

// =============================================================================
// RetxQueue wiring + cwnd gating + fast retransmit (D.1)
// =============================================================================

pub fn test_retx_queue_populated_by_poll_transmit() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let _ = tcp::send(c.id, &[0xAA; 100]).unwrap();
    let mut buf = [0u8; 1500];
    let _ = tcp::poll_transmit(c.id, &mut buf, 0).unwrap();

    let (inflight, entries) = with_data_state!(c.id, |d| (d.retx.inflight_bytes(), d.retx.len()));
    assert_eq_test!(inflight, 100, "retx tracks 100 inflight bytes");
    assert_eq_test!(entries, 1, "one retx entry");
    pass!()
}

pub fn test_poll_transmit_respects_cwnd() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    // Send a small payload and transmit it.
    let _ = tcp::send(id, b"x").unwrap();
    let mut buf = [0u8; 1500];
    let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();

    // Trigger RTO → cwnd shrinks to MSS (1460).
    let _ = tcp::retransmit_check(1000);

    // Fill the buffer well past cwnd.
    let _ = tcp::send(id, &[0x42; 4380]).unwrap();

    // First poll: sends MSS bytes (limited by peer_mss and cwnd).
    let (_, n1) = tcp::poll_transmit(id, &mut buf, 1001).unwrap();
    assert_eq_test!(n1, DEFAULT_MSS as usize, "first segment capped at MSS");

    // Second poll: cwnd exhausted (inflight = MSS = cwnd). Returns None.
    assert_test!(
        tcp::poll_transmit(id, &mut buf, 1002).is_none(),
        "blocked by cwnd"
    );
    pass!()
}

pub fn test_fast_retransmit_triggers_on_3_dup_acks() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;

    // Send MSS bytes and transmit.
    let _ = tcp::send(id, &[0xBB; DEFAULT_MSS as usize]).unwrap();
    let mut buf = [0u8; 1500];
    let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();

    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let snd_nxt_before = with_data_state!(id, |d| d.snd_nxt.raw());
    assert_test!(snd_nxt_before > snd_una, "data in flight");

    // Inject 3 duplicate ACKs (ack = snd_una, not advancing).
    let peer_seq = c.peer_iss.wrapping_add(1);
    for _ in 0..3 {
        let _ = tcp_common::inject_ack(&c, peer_seq, snd_una);
    }

    // After 3rd dup ACK: fast retransmit rewound snd_nxt to snd_una.
    let snd_nxt_after = with_data_state!(id, |d| d.snd_nxt.raw());
    assert_eq_test!(snd_nxt_after, snd_una, "snd_nxt rewound to snd_una");

    let in_recovery = with_data_state!(id, |d| d.cc.in_recovery());
    assert_test!(in_recovery, "entered fast recovery");

    // poll_transmit re-sends the data from snd_una.
    let resent = tcp::poll_transmit(id, &mut buf, 1);
    assert_test!(resent.is_some(), "retransmit after fast retransmit");
    pass!()
}

pub fn test_fast_retransmit_cwnd_reduction() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;

    // Send 3 MSS-sized segments (4380 bytes total).
    let _ = tcp::send(id, &[0xCC; 4380]).unwrap();
    let mut buf = [0u8; 1500];
    for _ in 0..3 {
        let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();
    }

    let flight = with_data_state!(id, |d| d.retx.inflight_bytes());
    assert_eq_test!(flight, 4380, "3 MSS in flight");

    // 3 dup ACKs → fast retransmit.
    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let peer_seq = c.peer_iss.wrapping_add(1);
    for _ in 0..3 {
        let _ = tcp_common::inject_ack(&c, peer_seq, snd_una);
    }

    // ssthresh = max(4380/2, 2*1460) = max(2190, 2920) = 2920
    // cwnd = ssthresh + 3*MSS = 2920 + 4380 = 7300
    let cwnd = with_data_state!(id, |d| d.cc.cwnd());
    assert_eq_test!(cwnd, 7300, "cwnd = ssthresh + 3*MSS");
    pass!()
}

pub fn test_fast_retransmit_not_during_recovery() -> TestResult {
    reset();
    let c = tcp_common::establish_connection();
    let id = c.id;

    let _ = tcp::send(id, &[0xDD; DEFAULT_MSS as usize]).unwrap();
    let mut buf = [0u8; 1500];
    let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();

    let snd_una = with_data_state!(id, |d| d.snd_una.raw());
    let peer_seq = c.peer_iss.wrapping_add(1);

    // First 3 dup ACKs → fast retransmit.
    for _ in 0..3 {
        let _ = tcp_common::inject_ack(&c, peer_seq, snd_una);
    }
    assert_test!(with_data_state!(id, |d| d.cc.in_recovery()), "in recovery");

    // Re-send data via poll_transmit.
    let _ = tcp::poll_transmit(id, &mut buf, 1).unwrap();
    let snd_nxt_before = with_data_state!(id, |d| d.snd_nxt.raw());

    // 3 more dup ACKs during recovery — must NOT trigger again.
    for _ in 0..3 {
        let _ = tcp_common::inject_ack(&c, peer_seq, snd_una);
    }
    let snd_nxt_after = with_data_state!(id, |d| d.snd_nxt.raw());
    assert_eq_test!(
        snd_nxt_after,
        snd_nxt_before,
        "no re-trigger during recovery"
    );
    pass!()
}

pub fn test_rto_resets_cwnd_and_clears_retx() -> TestResult {
    reset();
    let (id, _, _) = establish_connection();
    let _ = tcp::send(id, &[0xEE; 100]).unwrap();
    let mut buf = [0u8; 1500];
    let _ = tcp::poll_transmit(id, &mut buf, 0).unwrap();

    assert_eq_test!(
        with_data_state!(id, |d| d.retx.inflight_bytes()),
        100,
        "retx inflight before RTO"
    );

    // Trigger RTO.
    let _ = tcp::retransmit_check(1000);

    let cwnd = with_data_state!(id, |d| d.cc.cwnd());
    assert_eq_test!(cwnd, DEFAULT_MSS as u32, "cwnd reset to MSS");
    assert_test!(with_data_state!(id, |d| d.retx.is_empty()), "retx cleared");
    assert_eq_test!(
        with_data_state!(id, |d| d.retx.inflight_bytes()),
        0,
        "inflight zero"
    );
    pass!()
}

// =============================================================================
// Flow Control
// =============================================================================

pub fn test_tcp_respects_peer_window() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcde").unwrap();
    let mut payload = [0u8; 512];
    let (first_seg, first_len) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let shrink = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: first_seg.seq_num.wrapping_add(first_len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 100,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &shrink, &[], &[], 1);

    let _ = tcp::send(id, &[0x11u8; 200]).unwrap();
    let (_, next_len) = tcp::poll_transmit(id, &mut payload, 2).unwrap();
    assert_eq_test!(next_len, 100, "limited by peer window");
    pass!()
}

pub fn test_tcp_zero_window_blocks_send() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, b"abcde").unwrap();
    let mut payload = [0u8; 128];
    let (seg, len) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let zero_wnd = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &zero_wnd, &[], &[], 1);

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
    let (seg, len) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let zero_wnd = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &zero_wnd, &[], &[], 1);

    // Enqueue new data after zero-window so there is unsent data for the probe
    let _ = tcp::send(id, b"more").unwrap();

    let before = with_data_state!(id, |d| d.snd_nxt.raw());
    let probe = tcp::zero_window_probe(id, 2);
    assert_test!(probe.is_some(), "probe generated");
    let after = with_data_state!(id, |d| d.snd_nxt.raw());
    assert_eq_test!(before, after, "probe does not advance snd_nxt");
    pass!()
}

pub fn test_tcp_window_update_resumes_send() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let _ = tcp::send(id, &[0x33u8; 50]).unwrap();
    let mut payload = [0u8; 256];
    let (seg, len) = tcp::poll_transmit(id, &mut payload, 0).unwrap();

    let ack_half_zero = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(25),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 0,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack_half_zero, &[], &[], 1);
    assert_test!(
        tcp::poll_transmit(id, &mut payload, 2).is_none(),
        "send blocked at wnd=0"
    );

    let ack_rest_open = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: seg.seq_num.wrapping_add(len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 200,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack_rest_open, &[], &[], 3);

    let _ = tcp::send(id, &[0x44u8; 80]).unwrap();
    let resumed = tcp::poll_transmit(id, &mut payload, 4);
    assert_test!(resumed.is_some(), "send resumes after window opens");
    pass!()
}

// =============================================================================
// Delayed ACK
// =============================================================================

pub fn test_tcp_delayed_ack_after_two_segments() -> TestResult {
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());

    let r1 = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        snd_nxt,
        b"1",
        0,
    );
    assert_test!(r1.segments().next().is_none(), "first segment delayed");

    let r2 = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
    reset();
    let (id, server_iss, client_port) = establish_connection();
    let snd_nxt = with_data_state!(id, |d| d.snd_nxt.raw());
    let r = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
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
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &fin, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::CloseWait),
        "fin moves to close_wait"
    );
    assert_test!(result.segments().next().is_some(), "fin gets immediate ack");
    pass!()
}

slopos_testing::define_test_suite!(
    tcp_data,
    [
        test_ring_buffer_new_empty,
        test_ring_buffer_write_read_basic,
        test_ring_buffer_write_full,
        test_ring_buffer_wrap_around,
        test_ring_buffer_peek_offset,
        test_ring_buffer_consume,
        test_ring_buffer_clear,
        test_ring_buffer_partial_write,
        test_send_enqueue_and_peek,
        test_send_mark_sent_and_ack,
        test_send_retransmit_timeout,
        test_send_free_space,
        test_send_partial_ack,
        test_send_ack_stops_rto_timer,
        test_recv_enqueue_dequeue,
        test_recv_window_decreases,
        test_recv_ack_tracking,
        test_recv_delayed_ack_segments,
        test_recv_delayed_ack_timeout,
        test_tcp_send_in_established,
        test_tcp_recv_in_established,
        test_tcp_send_wrong_state,
        test_tcp_poll_transmit_basic,
        test_tcp_poll_transmit_mss_segmentation,
        test_tcp_poll_transmit_none_when_empty,
        test_tcp_data_roundtrip,
        test_tcp_recv_updates_window,
        test_tcp_retransmit_on_timeout,
        test_tcp_retransmit_exponential_backoff,
        test_tcp_retransmit_max_exceeded,
        test_tcp_retransmit_canceled_by_ack,
        test_retx_queue_populated_by_poll_transmit,
        test_poll_transmit_respects_cwnd,
        test_fast_retransmit_triggers_on_3_dup_acks,
        test_fast_retransmit_cwnd_reduction,
        test_fast_retransmit_not_during_recovery,
        test_rto_resets_cwnd_and_clears_retx,
        test_tcp_respects_peer_window,
        test_tcp_zero_window_blocks_send,
        test_tcp_zero_window_probe,
        test_tcp_window_update_resumes_send,
        test_tcp_delayed_ack_after_two_segments,
        test_tcp_delayed_ack_timeout,
        test_tcp_immediate_ack_for_fin,
    ]
);
