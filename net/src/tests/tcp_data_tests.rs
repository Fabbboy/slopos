//! TCP data transfer regression tests.
//!
//! Covers: ring buffer operations, send/receive buffers, data transfer through
//! the TCP state machine, delayed ACK, retransmission, flow control, and
//! zero-window probing.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::{
    self, DEFAULT_MSS, DELAYED_ACK_MS, MAX_RETRANSMITS, TCP_BUFFER_SIZE, TCP_FLAG_ACK,
    TCP_FLAG_FIN, TCP_FLAG_PSH, TcpError, TcpHeader, TcpState,
};
use crate::tests::tcp_common::{self, reset_all as reset};

/// Legacy tuple form of [`tcp_common::establish_connection`], kept so the
/// ~38 call sites below don't need destructuring rewrites.
fn establish_connection() -> (usize, u32, u16) {
    let c = tcp_common::establish_connection();
    (c.idx, c.peer_iss, c.local_port)
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
) -> tcp::TcpInputResult {
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
    tcp::tcp_input(remote_ip, local_ip, &hdr, &[], data, now_ms)
}

// =============================================================================
// Ring Buffer
// =============================================================================

pub fn test_ring_buffer_new_empty() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    assert_eq_test!(tcp::tcp_send_buffer_space(idx), TCP_BUFFER_SIZE, "capacity");
    assert_eq_test!(tcp::tcp_recv_available(idx), 0, "new len");
    assert_test!(!tcp::tcp_has_pending_data(idx), "new is empty");
    pass!()
}

pub fn test_ring_buffer_write_read_basic() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let n = tcp::tcp_send(idx, b"hello").unwrap();
    assert_eq_test!(n, 5, "write hello");
    assert_test!(tcp::tcp_has_pending_data(idx), "pending after write");

    let mut out = [0u8; 16];
    let (_, r) = tcp::tcp_poll_transmit(idx, &mut out, 0).unwrap();
    assert_eq_test!(r, 5, "read hello");
    assert_test!(&out[..5] == b"hello", "content matches");
    pass!()
}

pub fn test_ring_buffer_write_full() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let chunk = [0xABu8; 512];
    let mut remaining = TCP_BUFFER_SIZE;
    while remaining > 0 {
        let to_write = core::cmp::min(remaining, chunk.len());
        let wrote = tcp::tcp_send(idx, &chunk[..to_write]).unwrap();
        assert_eq_test!(wrote, to_write, "write chunk into send buffer");
        remaining -= to_write;
    }

    assert_eq_test!(tcp::tcp_send_buffer_space(idx), 0, "no free space");
    let second = tcp::tcp_send(idx, &[1, 2, 3]).unwrap();
    assert_eq_test!(second, 0, "write when full");
    pass!()
}

pub fn test_ring_buffer_wrap_around() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
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
            conn.snd_nxt,
            &first[..n],
            injected as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut tmp = [0u8; 256];
    let mut drained = 0usize;
    while drained < half {
        let n = tcp::tcp_recv(idx, &mut tmp).unwrap();
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
            conn.snd_nxt,
            &second[..n],
            (half + injected) as u64,
        );
        seq = seq.wrapping_add(n as u32);
        injected += n;
    }

    let mut out = [0u8; 256];
    drained = 0;
    while drained < half {
        let n = tcp::tcp_recv(idx, &mut out).unwrap();
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
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcdefgh").unwrap();

    let mut first = [0u8; 2];
    let (seg1, n1) = tcp::tcp_poll_transmit(idx, &mut first, 0).unwrap();
    assert_eq_test!(n1, 2, "first chunk len");
    assert_test!(&first == b"ab", "first chunk content");

    let mut second = [0u8; 3];
    let (_, n2) = tcp::tcp_poll_transmit(idx, &mut second, 1).unwrap();
    assert_eq_test!(n2, 3, "peek len");
    assert_test!(&second == b"cde", "peek offset data");

    let mut third = [0u8; 8];
    let (_, n3) = tcp::tcp_poll_transmit(idx, &mut third, 2).unwrap();
    assert_eq_test!(n3, 3, "remaining chunk len");
    assert_test!(&third[..3] == b"fgh", "remaining chunk content");
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 2);
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE,
        "full data preserved"
    );
    pass!()
}

pub fn test_ring_buffer_consume() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcdef").unwrap();
    let mut tx = [0u8; 16];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut tx, 0).unwrap();
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE - 4,
        "len after consume"
    );

    assert_eq_test!(
        tcp::tcp_retransmit_check(1000),
        Some(idx),
        "trigger retransmit"
    );
    let mut out = [0u8; 8];
    let (_, r) = tcp::tcp_poll_transmit(idx, &mut out, 1001).unwrap();
    assert_eq_test!(r, 4, "read remaining");
    assert_test!(&out[..4] == b"cdef", "remaining content");
    pass!()
}

pub fn test_ring_buffer_clear() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"data").unwrap();
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE - 4,
        "wrote data"
    );

    reset();
    let (idx2, _, _) = establish_connection();
    assert_eq_test!(tcp::tcp_recv_available(idx2), 0, "len after clear");
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx2),
        TCP_BUFFER_SIZE,
        "free after clear"
    );
    pass!()
}

pub fn test_ring_buffer_partial_write() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let chunk = [7u8; 512];
    let mut remaining = TCP_BUFFER_SIZE - 4;
    while remaining > 0 {
        let to_write = core::cmp::min(remaining, chunk.len());
        let wrote = tcp::tcp_send(idx, &chunk[..to_write]).unwrap();
        assert_eq_test!(wrote, to_write, "fill buffer");
        remaining -= to_write;
    }

    let n = tcp::tcp_send(idx, &[9u8; 16]).unwrap();
    assert_eq_test!(n, 4, "partial write limited by free space");
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
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
    let (idx, _, _) = establish_connection();
    let n = tcp::tcp_send(idx, b"payload").unwrap();
    assert_eq_test!(n, 7, "enqueue len");
    assert_test!(tcp::tcp_has_pending_data(idx), "unsent len");

    let mut out = [0u8; 7];
    let (_, p) = tcp::tcp_poll_transmit(idx, &mut out, 0).unwrap();
    assert_eq_test!(p, 7, "peek unsent");
    assert_test!(&out == b"payload", "peek content");
    pass!()
}

pub fn test_send_mark_sent_and_ack() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcdef").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE,
        "buffer empty after ack"
    );
    pass!()
}

pub fn test_send_retransmit_timeout() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcd").unwrap();
    let mut payload = [0u8; 8];
    let _ = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

    assert_eq_test!(tcp::tcp_retransmit_check(1000), Some(idx), "inflight reset");
    let (_, n) = tcp::tcp_poll_transmit(idx, &mut payload, 1001).unwrap();
    assert_eq_test!(n, 4, "retransmit flag set");
    assert_test!(&payload[..4] == b"abcd", "retransmit payload");
    pass!()
}

pub fn test_send_free_space() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let before = tcp::tcp_send_buffer_space(idx);
    let _ = tcp::tcp_send(idx, &[1u8; 128]).unwrap();
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        before - 128,
        "free space decreases"
    );
    pass!()
}

pub fn test_send_partial_ack() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, &[3u8; 1000]).unwrap();
    let mut payload = [0u8; 1200];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE - 500,
        "buffered after partial ack"
    );

    assert_eq_test!(
        tcp::tcp_retransmit_check(1000),
        Some(idx),
        "inflight after partial ack"
    );
    let (_, retransmit_len) = tcp::tcp_poll_transmit(idx, &mut payload, 1001).unwrap();
    assert_eq_test!(retransmit_len, 500, "remaining bytes retransmit");
    pass!()
}

pub fn test_send_ack_stops_rto_timer() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcd").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 10);
    assert_test!(
        tcp::tcp_retransmit_check(2000).is_none(),
        "rto timer cleared"
    );
    pass!()
}

// =============================================================================
// Receive Buffer
// =============================================================================

pub fn test_recv_enqueue_dequeue() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"hello",
        10,
    );
    assert_test!(res.response.is_none(), "first segment delayed ack");

    let n = tcp::tcp_recv_available(idx);
    assert_eq_test!(n, 5, "enqueue len");

    let mut out = [0u8; 5];
    let r = tcp::tcp_recv(idx, &mut out).unwrap();
    assert_eq_test!(r, 5, "dequeue len");
    assert_test!(&out == b"hello", "dequeue content");
    pass!()
}

pub fn test_recv_window_decreases() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let before = tcp::tcp_get_connection(idx).unwrap().rcv_wnd;
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let _ = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        &[1u8; 256],
        0,
    );
    let after_enqueue = tcp::tcp_get_connection(idx).unwrap().rcv_wnd;
    assert_test!(after_enqueue < before, "window shrinks after enqueue");

    let mut out = [0u8; 256];
    let _ = tcp::tcp_recv(idx, &mut out).unwrap();
    let after_dequeue = tcp::tcp_get_connection(idx).unwrap().rcv_wnd;
    assert_test!(after_dequeue > after_enqueue, "window grows after dequeue");
    pass!()
}

pub fn test_recv_ack_tracking() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"x",
        100,
    );
    assert_test!(res.response.is_none(), "ack pending set");
    let delayed = tcp::tcp_delayed_ack_check(100 + DELAYED_ACK_MS);
    assert_test!(delayed.is_some(), "ack pending cleared");
    assert_test!(
        tcp::tcp_delayed_ack_check(100 + DELAYED_ACK_MS + 1).is_none(),
        "segment counter cleared"
    );
    pass!()
}

pub fn test_recv_delayed_ack_segments() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let first = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.response.is_none(), "one segment not enough");
    let second = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(2),
        conn.snd_nxt,
        b"b",
        1,
    );
    assert_test!(second.response.is_some(), "two segments trigger ack");
    pass!()
}

pub fn test_recv_delayed_ack_timeout() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"x",
        1000,
    );
    assert_test!(res.response.is_none(), "initial delayed ack");
    assert_test!(
        tcp::tcp_delayed_ack_check(1000 + DELAYED_ACK_MS - 1).is_none(),
        "before timeout"
    );
    assert_test!(
        tcp::tcp_delayed_ack_check(1000 + DELAYED_ACK_MS).is_some(),
        "timeout triggers ack"
    );
    pass!()
}

// =============================================================================
// Data Transfer Integration
// =============================================================================

pub fn test_tcp_send_in_established() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let before = tcp::tcp_send_buffer_space(idx);
    let wrote = tcp::tcp_send(idx, b"hello").unwrap();
    assert_eq_test!(wrote, 5, "tcp_send wrote bytes");
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        before - 5,
        "send space reduced"
    );
    pass!()
}

pub fn test_tcp_recv_in_established() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let res = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"abc",
        0,
    );
    assert_test!(res.response.is_none(), "first segment delayed ack");

    let mut out = [0u8; 8];
    let n = tcp::tcp_recv(idx, &mut out).unwrap();
    assert_eq_test!(n, 3, "recv bytes");
    assert_test!(&out[..3] == b"abc", "recv content");
    pass!()
}

pub fn test_tcp_send_wrong_state() -> TestResult {
    reset();
    let (idx, _) = tcp::tcp_connect([10, 0, 0, 1], [10, 0, 0, 2], 80).unwrap();
    let err = tcp::tcp_send(idx, b"x").unwrap_err();
    assert_eq_test!(err, TcpError::InvalidState, "send in SYN_SENT rejected");
    pass!()
}

pub fn test_tcp_poll_transmit_basic() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcd").unwrap();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let mut payload = [0u8; 64];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 10).unwrap();
    assert_eq_test!(n, 4, "payload len");
    assert_test!(&payload[..4] == b"abcd", "payload bytes");
    assert_eq_test!(seg.seq_num, conn.snd_nxt, "segment seq");
    assert_eq_test!(seg.ack_num, conn.rcv_nxt, "segment ack");
    assert_test!(seg.flags & TCP_FLAG_PSH != 0, "PSH set");
    pass!()
}

pub fn test_tcp_poll_transmit_mss_segmentation() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let data = [0x42u8; DEFAULT_MSS as usize + 100];
    let wrote = tcp::tcp_send(idx, &data).unwrap();
    assert_eq_test!(wrote, data.len(), "enqueue full test payload");

    let mut payload = [0u8; 2048];
    let (_, first_len) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
    assert_eq_test!(first_len, DEFAULT_MSS as usize, "first chunk mss-sized");
    let (_, second_len) = tcp::tcp_poll_transmit(idx, &mut payload, 1).unwrap();
    assert_eq_test!(second_len, 100, "second chunk remainder");
    pass!()
}

pub fn test_tcp_poll_transmit_none_when_empty() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let mut payload = [0u8; 64];
    assert_test!(
        tcp::tcp_poll_transmit(idx, &mut payload, 0).is_none(),
        "none when empty"
    );
    pass!()
}

pub fn test_tcp_data_roundtrip() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"hello").unwrap();
    let mut payload = [0u8; 64];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 1);
    assert_test!(!tcp::tcp_has_pending_data(idx), "no pending data after ack");
    assert_eq_test!(
        tcp::tcp_send_buffer_space(idx),
        TCP_BUFFER_SIZE,
        "send buffer reclaimed"
    );
    pass!()
}

pub fn test_tcp_recv_updates_window() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let before = tcp::tcp_get_connection(idx).unwrap().rcv_wnd;
    let conn = tcp::tcp_get_connection(idx).unwrap();

    let first = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"a",
        0,
    );
    assert_test!(first.response.is_none(), "first segment delayed");

    let second = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(2),
        conn.snd_nxt,
        b"b",
        1,
    );
    let ack = match second.response {
        Some(s) => s,
        None => return fail!("expected immediate ACK after second segment"),
    };
    let after = tcp::tcp_get_connection(idx).unwrap().rcv_wnd;
    assert_test!(after < before, "receive window decreased");
    assert_eq_test!(ack.window_size, after, "ack advertises updated window");
    pass!()
}

// =============================================================================
// Retransmission
// =============================================================================

pub fn test_tcp_retransmit_on_timeout() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abc").unwrap();
    let mut payload = [0u8; 16];
    let _ = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();
    assert_test!(tcp::tcp_retransmit_check(999).is_none(), "before timeout");
    assert_eq_test!(
        tcp::tcp_retransmit_check(1000),
        Some(idx),
        "timeout triggers retransmit"
    );
    pass!()
}

pub fn test_tcp_retransmit_exponential_backoff() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abc").unwrap();
    let mut payload = [0u8; 16];
    let _ = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

    let _ = tcp::tcp_retransmit_check(1000);
    let conn1 = tcp::tcp_get_connection(idx).unwrap();
    assert_eq_test!(conn1.rto_ms, 2000, "first timeout doubles rto");
    let _ = tcp::tcp_poll_transmit(idx, &mut payload, 1001).unwrap();

    let _ = tcp::tcp_retransmit_check(3001);
    let conn2 = tcp::tcp_get_connection(idx).unwrap();
    assert_eq_test!(conn2.rto_ms, 4000, "second timeout doubles rto again");
    pass!()
}

pub fn test_tcp_retransmit_max_exceeded() -> TestResult {
    reset();
    let (idx, _, _) = establish_connection();
    let _ = tcp::tcp_send(idx, b"x").unwrap();
    let mut payload = [0u8; 8];
    let _ = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

    let mut now = 0u64;
    for _ in 0..MAX_RETRANSMITS {
        let rto = tcp::tcp_get_connection(idx).unwrap().rto_ms as u64;
        now = now.saturating_add(rto);
        assert_eq_test!(
            tcp::tcp_retransmit_check(now),
            Some(idx),
            "retransmit fires"
        );
        let _ = tcp::tcp_poll_transmit(idx, &mut payload, now + 1).unwrap();
    }

    let rto = tcp::tcp_get_connection(idx).unwrap().rto_ms as u64;
    now = now.saturating_add(rto);
    let _ = tcp::tcp_retransmit_check(now);
    assert_test!(
        tcp::tcp_get_state(idx).is_none(),
        "connection released after max retransmits"
    );
    pass!()
}

pub fn test_tcp_retransmit_canceled_by_ack() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"hello").unwrap();
    let mut payload = [0u8; 16];
    let (seg, n) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack, &[], &[], 500);
    assert_test!(
        tcp::tcp_retransmit_check(1000).is_none(),
        "ack cancels timeout"
    );
    pass!()
}

// =============================================================================
// Flow Control
// =============================================================================

pub fn test_tcp_respects_peer_window() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcde").unwrap();
    let mut payload = [0u8; 512];
    let (first_seg, first_len) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &shrink, &[], &[], 1);

    let _ = tcp::tcp_send(idx, &[0x11u8; 200]).unwrap();
    let (_, next_len) = tcp::tcp_poll_transmit(idx, &mut payload, 2).unwrap();
    assert_eq_test!(next_len, 100, "limited by peer window");
    pass!()
}

pub fn test_tcp_zero_window_blocks_send() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcde").unwrap();
    let mut payload = [0u8; 128];
    let (seg, len) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &zero_wnd, &[], &[], 1);

    let _ = tcp::tcp_send(idx, &[0x22u8; 64]).unwrap();
    assert_test!(
        tcp::tcp_poll_transmit(idx, &mut payload, 2).is_none(),
        "blocked by zero window"
    );
    pass!()
}

pub fn test_tcp_zero_window_probe() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, b"abcde").unwrap();
    let mut payload = [0u8; 128];
    let (seg, len) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &zero_wnd, &[], &[], 1);

    // Enqueue new data after zero-window so there is unsent data for the probe
    let _ = tcp::tcp_send(idx, b"more").unwrap();

    let before = tcp::tcp_get_connection(idx).unwrap().snd_nxt;
    let probe = tcp::tcp_zero_window_probe(idx, 2);
    assert_test!(probe.is_some(), "probe generated");
    let after = tcp::tcp_get_connection(idx).unwrap().snd_nxt;
    assert_eq_test!(before, after, "probe does not advance snd_nxt");
    pass!()
}

pub fn test_tcp_window_update_resumes_send() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let _ = tcp::tcp_send(idx, &[0x33u8; 50]).unwrap();
    let mut payload = [0u8; 256];
    let (seg, len) = tcp::tcp_poll_transmit(idx, &mut payload, 0).unwrap();

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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack_half_zero, &[], &[], 1);
    assert_test!(
        tcp::tcp_poll_transmit(idx, &mut payload, 2).is_none(),
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
    let _ = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &ack_rest_open, &[], &[], 3);

    let _ = tcp::tcp_send(idx, &[0x44u8; 80]).unwrap();
    let resumed = tcp::tcp_poll_transmit(idx, &mut payload, 4);
    assert_test!(resumed.is_some(), "send resumes after window opens");
    pass!()
}

// =============================================================================
// Delayed ACK
// =============================================================================

pub fn test_tcp_delayed_ack_after_two_segments() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();

    let r1 = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"1",
        0,
    );
    assert_test!(r1.response.is_none(), "first segment delayed");

    let r2 = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(2),
        conn.snd_nxt,
        b"2",
        1,
    );
    assert_test!(r2.response.is_some(), "second segment triggers ack");
    pass!()
}

pub fn test_tcp_delayed_ack_timeout() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let r = inject_data_segment(
        [10, 0, 0, 2],
        [10, 0, 0, 1],
        80,
        client_port,
        server_iss.wrapping_add(1),
        conn.snd_nxt,
        b"x",
        0,
    );
    assert_test!(r.response.is_none(), "initial delayed ack");
    assert_test!(
        tcp::tcp_delayed_ack_check(DELAYED_ACK_MS - 1).is_none(),
        "before delayed ack timeout"
    );
    let delayed = tcp::tcp_delayed_ack_check(DELAYED_ACK_MS);
    assert_test!(delayed.is_some(), "delayed ack fires at timeout");
    pass!()
}

pub fn test_tcp_immediate_ack_for_fin() -> TestResult {
    reset();
    let (idx, server_iss, client_port) = establish_connection();
    let conn = tcp::tcp_get_connection(idx).unwrap();
    let fin = TcpHeader {
        src_port: 80,
        dst_port: client_port,
        seq_num: server_iss.wrapping_add(1),
        ack_num: conn.snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::tcp_input([10, 0, 0, 2], [10, 0, 0, 1], &fin, &[], &[], 0);
    assert_eq_test!(
        result.new_state,
        Some(TcpState::CloseWait),
        "fin moves to close_wait"
    );
    assert_test!(result.response.is_some(), "fin gets immediate ack");
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
        test_tcp_respects_peer_window,
        test_tcp_zero_window_blocks_send,
        test_tcp_zero_window_probe,
        test_tcp_window_update_resumes_send,
        test_tcp_delayed_ack_after_two_segments,
        test_tcp_delayed_ack_timeout,
        test_tcp_immediate_ack_for_fin,
    ]
);
