//! Integration tests — Two-Queue Listen Model.
//!
//! Tests the SYN queue, accept queue, SYN-ACK retransmission, overflow
//! behavior of [`TcpListenState`].

use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::listener::{
    SYN_QUEUE_MAX, SYN_RETRIES_MAX, SynQueue, TcpListenState, reset_syn_entry_keys,
};
use crate::types::{Ipv4Addr, Port, SockAddr};

/// Helper: a SYN queue bound to [`local_addr`], capacity reserved.
fn make_syn_queue() -> SynQueue {
    SynQueue::with_capacity(local_addr()).expect("syn queue alloc")
}

/// Helper: an accept queue with the given backlog.
fn make_listen(backlog: usize) -> TcpListenState {
    TcpListenState::new(backlog, local_addr()).expect("listen state alloc")
}

/// Helper: create a local listening address.
fn local_addr() -> SockAddr {
    SockAddr {
        ip: Ipv4Addr([10, 0, 0, 1]),
        port: Port(8080),
    }
}

/// Helper: create a unique remote client address.
fn client_addr(n: u16) -> SockAddr {
    SockAddr {
        ip: Ipv4Addr([192, 168, 1, (n & 0xff) as u8]),
        port: Port(40000 + n),
    }
}

// =============================================================================
// T1: SYN queue overflow — fill to SYN_QUEUE_MAX, verify next SYN returns None
// =============================================================================

pub fn test_syn_queue_overflow() -> TestResult {
    reset_syn_entry_keys();
    let mut syn = make_syn_queue();

    // Fill the SYN queue to capacity.
    for i in 0..SYN_QUEUE_MAX as u16 {
        let client = client_addr(i);
        let result = syn.on_syn(client, 1000 + i as u32, 1460, false, 0, None);
        assert_test!(
            result.is_some(),
            "SYN {} should succeed (queue not full yet)"
        );
    }

    assert_eq_test!(syn.len(), SYN_QUEUE_MAX, "SYN queue at capacity");

    // Next SYN should be silently dropped (no RST).
    let overflow_client = client_addr(SYN_QUEUE_MAX as u16);
    let overflow_result = syn.on_syn(overflow_client, 9999, 1460, false, 0, None);
    assert_test!(
        overflow_result.is_none(),
        "SYN queue full -> silently dropped (no RST)"
    );

    // Queue length unchanged.
    assert_eq_test!(
        syn.len(),
        SYN_QUEUE_MAX,
        "SYN queue still at capacity after overflow"
    );

    pass!()
}

// =============================================================================
// T2: A matching final ACK always completes the handshake; whether the result
//     can be queued for accept() is the socket layer's separate bound.
// =============================================================================

pub fn test_accept_queue_overflow() -> TestResult {
    reset_syn_entry_keys();
    let backlog = 2usize;
    let mut syn = make_syn_queue();
    let mut listen = make_listen(backlog);

    for i in 0..3u16 {
        let client = client_addr(i);
        let syn_ack = syn.on_syn(client, 1000 + i as u32, 1460, false, 0, None);
        assert_test!(syn_ack.is_some(), "SYN should succeed");
    }
    assert_eq_test!(syn.len(), 3, "3 entries in SYN queue");

    // A duplicate SYN hands the ISS back so each final ACK can be built.
    let mut queued = 0usize;
    let mut refused = 0usize;
    for i in 0..3u16 {
        let client = client_addr(i);
        let syn_ack = syn
            .on_syn(client, 1000 + i as u32, 1460, false, 0, None)
            .expect("duplicate SYN retransmits SYN-ACK");
        let ack_num = syn_ack.seq_num.wrapping_add(1);

        let accepted = syn.on_ack(client, ack_num);
        assert_test!(
            accepted.is_some(),
            "a matching final ACK always completes the handshake"
        );
        if listen.push_accepted(accepted.unwrap()) {
            queued += 1;
        } else {
            refused += 1;
        }
    }

    assert_eq_test!(syn.len(), 0, "every entry left the SYN queue");
    assert_eq_test!(queued, backlog, "the backlog took exactly its capacity");
    assert_eq_test!(refused, 1, "the connection past the backlog was refused");
    assert_eq_test!(listen.accept_queue_len(), backlog, "accept queue full");

    assert_test!(listen.accept().is_some(), "accept dequeues");
    assert_eq_test!(listen.accept_queue_len(), 1, "accept queue drained to 1");
    assert_test!(listen.accept_queue_has_room(), "room after draining");

    pass!()
}

// =============================================================================
// T3: SYN-ACK retransmission — verify 5 retransmissions with backoff, then
//     removal
// =============================================================================

pub fn test_syn_ack_retransmit_exhaustion() -> TestResult {
    reset_syn_entry_keys();
    let mut syn = make_syn_queue();

    let client = client_addr(0);
    let syn_ack = syn.on_syn(client, 5000, 1460, false, 0, None);
    assert_test!(syn_ack.is_some(), "initial SYN accepted");
    assert_eq_test!(syn.len(), 1, "1 entry in SYN queue");

    // The SYN-ACK returned by on_syn tells us the ISS.
    let syn_ack = syn_ack.unwrap();
    let original_iss = syn_ack.seq_num;

    // Determine the entry's key — it's the first key allocated (we reset keys).
    // Key=1 because we reset_syn_entry_keys() above.
    let entry_key = 1u32;

    // Simulate 5 retransmit timer firings — each should return a SYN-ACK.
    for _retry in 1..=SYN_RETRIES_MAX {
        let retransmit = syn.on_retransmit(entry_key);
        assert_test!(retransmit.is_some(), "retransmit should succeed on retry");
        let seg = retransmit.unwrap();
        assert_eq_test!(
            seg.seq_num,
            original_iss,
            "ISS unchanged across retransmits"
        );
        assert_eq_test!(syn.len(), 1, "entry still in SYN queue");
    }

    // 6th retransmit (retry > SYN_RETRIES_MAX) — entry should be removed.
    let exhausted = syn.on_retransmit(entry_key);
    assert_test!(
        exhausted.is_none(),
        "retransmit returns None after max retries"
    );
    assert_eq_test!(
        syn.len(),
        0,
        "entry removed from SYN queue after exhaustion"
    );

    pass!()
}

// =============================================================================
// Additional coverage: duplicate SYN retransmits existing SYN-ACK
// =============================================================================

pub fn test_duplicate_syn_retransmits() -> TestResult {
    reset_syn_entry_keys();
    let mut syn = make_syn_queue();

    let client = client_addr(42);
    let first = syn.on_syn(client, 7000, 1460, false, 0, None);
    assert_test!(first.is_some(), "first SYN accepted");
    let first_iss = first.unwrap().seq_num;

    // Send duplicate SYN — should retransmit the same SYN-ACK.
    let dup = syn.on_syn(client, 7000, 1460, false, 100, None);
    assert_test!(dup.is_some(), "duplicate SYN triggers SYN-ACK retransmit");
    let dup_iss = dup.unwrap().seq_num;

    assert_eq_test!(
        first_iss,
        dup_iss,
        "duplicate SYN returns same ISS (same entry)"
    );
    assert_eq_test!(syn.len(), 1, "no duplicate entry created");

    pass!()
}

// =============================================================================
// TcpListenState — push_accepted enqueues completed connections
// =============================================================================

pub fn test_push_accepted_basic() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = make_listen(4);

    // Push an accepted connection directly.
    let accepted = crate::tcp::listener::AcceptedConn {
        tuple: crate::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 10],
            remote_port: 50000,
        },
        iss: 1000,
        irs: 2000,
        peer_mss: 1460,
        sack_permitted: false,
        peer_tsval: None,
    };

    let ok = listen.push_accepted(accepted);
    assert_test!(ok, "push_accepted should succeed when queue has room");
    assert_eq_test!(
        listen.accept_queue_len(),
        1,
        "accept queue should have 1 entry"
    );

    // Accept should dequeue it.
    let dequeued = listen.accept();
    assert_test!(
        dequeued.is_some(),
        "accept should return the pushed connection"
    );
    let conn = dequeued.unwrap();
    assert_eq_test!(conn.tuple.remote_port, 50000, "remote port should match");
    assert_eq_test!(conn.iss, 1000, "ISS should match");
    assert_eq_test!(conn.irs, 2000, "IRS should match");
    assert_eq_test!(conn.peer_mss, 1460, "peer MSS should match");

    assert_eq_test!(
        listen.accept_queue_len(),
        0,
        "accept queue should be empty after dequeue"
    );

    pass!()
}

// =============================================================================
// TcpListenState — push_accepted respects backlog
// =============================================================================

pub fn test_push_accepted_respects_backlog() -> TestResult {
    reset_syn_entry_keys();
    let backlog = 2usize;
    let mut listen = make_listen(backlog);

    // Fill to backlog.
    for i in 0..backlog as u16 {
        let accepted = crate::tcp::listener::AcceptedConn {
            tuple: crate::tcp::TcpTuple {
                local_ip: [10, 0, 0, 1],
                local_port: 8080,
                remote_ip: [192, 168, 1, i as u8],
                remote_port: 40000 + i,
            },
            iss: 1000 + i as u32,
            irs: 2000 + i as u32,
            peer_mss: 1460,
            sack_permitted: false,
            peer_tsval: None,
        };
        let ok = listen.push_accepted(accepted);
        assert_test!(ok, "push_accepted should succeed within backlog");
    }
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue at backlog"
    );

    // Next push should fail (queue full).
    let overflow = crate::tcp::listener::AcceptedConn {
        tuple: crate::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 99],
            remote_port: 59999,
        },
        iss: 9999,
        irs: 8888,
        peer_mss: 1460,
        sack_permitted: false,
        peer_tsval: None,
    };
    let rejected = listen.push_accepted(overflow);
    assert_test!(
        !rejected,
        "push_accepted should fail when accept queue is full"
    );
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue unchanged after overflow"
    );

    // Drain one, then push again — should succeed.
    let _ = listen.accept();
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog - 1,
        "accept queue drained by 1"
    );

    let ok = listen.push_accepted(overflow);
    assert_test!(ok, "push_accepted should succeed after draining");
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue back to backlog"
    );

    pass!()
}

// =============================================================================
// TcpListenState — backlog clamping
// =============================================================================

pub fn test_listen_state_backlog_clamping() -> TestResult {
    reset_syn_entry_keys();

    // Backlog 0 should clamp to BACKLOG_MIN (1).
    let listen_min = make_listen(0);
    assert_eq_test!(
        listen_min.backlog(),
        crate::tcp::listener::BACKLOG_MIN,
        "backlog=0 should clamp to BACKLOG_MIN"
    );

    // Backlog 999 should clamp to BACKLOG_MAX.
    let listen_max = make_listen(999);
    assert_eq_test!(
        listen_max.backlog(),
        crate::tcp::listener::BACKLOG_MAX,
        "backlog=999 should clamp to BACKLOG_MAX"
    );

    // Normal backlog should pass through.
    let listen_normal = make_listen(16);
    assert_eq_test!(
        listen_normal.backlog(),
        16,
        "backlog=16 should pass through"
    );

    pass!()
}

// =============================================================================
// TcpListenState — accept returns FIFO order
// =============================================================================

pub fn test_accept_fifo_order() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = make_listen(8);

    // Push 3 connections with distinct remote ports.
    for i in 0..3u16 {
        let accepted = crate::tcp::listener::AcceptedConn {
            tuple: crate::tcp::TcpTuple {
                local_ip: [10, 0, 0, 1],
                local_port: 8080,
                remote_ip: [192, 168, 1, 1],
                remote_port: 40000 + i,
            },
            iss: 1000 + i as u32,
            irs: 2000 + i as u32,
            peer_mss: 1460,
            sack_permitted: false,
            peer_tsval: None,
        };
        listen.push_accepted(accepted);
    }

    // Accept should return in FIFO order.
    for i in 0..3u16 {
        let conn = listen.accept();
        assert_test!(conn.is_some(), "accept should return connection");
        assert_eq_test!(
            conn.unwrap().tuple.remote_port,
            40000 + i,
            "accept should return in FIFO order"
        );
    }

    // Queue should be empty now.
    let none = listen.accept();
    assert_test!(none.is_none(), "accept on empty queue returns None");

    pass!()
}

// =============================================================================
// TcpListenState — clear wipes both queues
// =============================================================================

pub fn test_listen_state_clear() -> TestResult {
    reset_syn_entry_keys();
    let mut syn = make_syn_queue();
    let mut listen = make_listen(16);

    // Add something to SYN queue.
    let _ = syn.on_syn(client_addr(0), 1000, 1460, false, 0, None);
    assert_eq_test!(syn.len(), 1, "SYN queue has 1 entry");

    // Push to accept queue.
    let accepted = crate::tcp::listener::AcceptedConn {
        tuple: crate::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 50],
            remote_port: 55000,
        },
        iss: 3000,
        irs: 4000,
        peer_mss: 1460,
        sack_permitted: false,
        peer_tsval: None,
    };
    listen.push_accepted(accepted);
    assert_eq_test!(listen.accept_queue_len(), 1, "accept queue has 1 entry");

    // Each queue clears its own half now.
    syn.clear();
    listen.clear();
    assert_eq_test!(syn.len(), 0, "SYN queue cleared");
    assert_eq_test!(listen.accept_queue_len(), 0, "accept queue cleared");

    pass!()
}

// =============================================================================
// TCP Send/Recv/Shutdown — FIN handling and shutdown semantics
// =============================================================================

use crate::tcp::{self, ConnId, TCP_FLAG_ACK, TCP_FLAG_FIN, TcpHeader, TcpState};
use crate::tests::tcp_common;

/// Legacy tuple form of [`tcp_common::establish_connection`], kept so the
/// test bodies below don't need destructuring rewrites during the P1.2 move.
///
/// The canonical helper in `tcp_common` targets `REMOTE_IP = 10.0.0.2` with
/// `REMOTE_PORT = 80`, which matches the addresses this suite used before.
fn establish_connection() -> (ConnId, u16, u32) {
    let c = tcp_common::establish_connection();
    (c.id, c.local_port, c.our_iss)
}

/// Helper: deliver a FIN from the remote peer to a connection.
fn deliver_peer_fin(id: ConnId) {
    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let fin_hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
        window_size: 65535,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(tuple.remote_ip, tuple.local_ip, &fin_hdr, &[], &[], 0);
}

/// Helper: deliver data from the remote peer.
fn deliver_peer_data(id: ConnId, data: &[u8]) {
    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 65535,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(tuple.remote_ip, tuple.local_ip, &hdr, &[], data, 0);
}

// =============================================================================
// 5.T4: FIN handling — peer FIN transitions to CloseWait, recv returns 0 (EOF)
//       after buffered data is drained.
// =============================================================================

pub fn test_fin_handling_eof() -> TestResult {
    tcp::reset_all();

    let (id, _lp, _iss) = establish_connection();

    // Peer sends some data, then FIN.
    deliver_peer_data(id, b"hello");
    deliver_peer_fin(id);

    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::CloseWait),
        "should be CLOSE_WAIT after peer FIN"
    );

    // Drain the buffered data.
    let mut buf = [0u8; 64];
    let n = tcp::recv(id, &mut buf).expect("recv should succeed");
    assert_eq_test!(n, 5, "should read 5 bytes of buffered data");
    assert_eq_test!(&buf[..5], b"hello", "buffered data should match");

    // Next recv should return 0 (EOF) — buffer empty + peer closed.
    let n2 = tcp::recv(id, &mut buf).expect("recv should succeed");
    assert_eq_test!(n2, 0, "recv after drain + FIN should return 0 (EOF)");

    // is_peer_closed should be true.
    assert_test!(
        tcp::is_peer_closed(id),
        "tcp_is_peer_closed should be true in CLOSE_WAIT"
    );

    pass!()
}

// =============================================================================
// 5.T5: shutdown(SHUT_WR) — sends FIN (Established→FinWait1) but recv still
//       works for buffered data.
// =============================================================================

pub fn test_shutdown_write_sends_fin() -> TestResult {
    tcp::reset_all();

    let (id, _lp, _iss) = establish_connection();

    // Peer sends data before our shutdown.
    deliver_peer_data(id, b"world");

    // Shutdown write half — should send FIN.
    let result = tcp::shutdown_write(id);
    assert_test!(result.is_ok(), "tcp_shutdown_write should succeed");
    let seg = result.unwrap();
    assert_test!(seg.is_some(), "should produce a FIN segment");
    let seg = seg.unwrap();
    assert_test!(
        seg.flags & TCP_FLAG_FIN != 0,
        "segment should have FIN flag"
    );

    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::FinWait1),
        "should transition to FIN_WAIT_1"
    );

    // Recv should still work — we only shut down writing.
    let mut buf = [0u8; 64];
    let n = tcp::recv(id, &mut buf).expect("recv should still work");
    assert_eq_test!(n, 5, "should read 5 bytes");
    assert_eq_test!(&buf[..5], b"world", "data should match");

    // Sending should fail (InvalidState — no longer Established/CloseWait).
    let send_result = tcp::send(id, b"test");
    assert_test!(send_result.is_err(), "send after SHUT_WR should fail");

    pass!()
}

// =============================================================================
// 5.T6: shutdown(SHUT_RD) — recv buffer is cleared, is_peer_closed check.
// =============================================================================

pub fn test_shutdown_read_discards_buffer() -> TestResult {
    tcp::reset_all();

    let (id, _lp, _iss) = establish_connection();

    // Peer sends data.
    deliver_peer_data(id, b"discard me");

    // Verify data is in the buffer.
    assert_test!(
        tcp::recv_available(id) > 0,
        "recv buffer should have data before discard"
    );

    // Discard recv buffer (simulates SHUT_RD at tcp layer).
    tcp::recv_discard(id);

    // Buffer should be empty now.
    assert_eq_test!(
        tcp::recv_available(id),
        0,
        "recv buffer should be empty after discard"
    );

    // State should still be Established (shutdown read doesn't change state).
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::Established),
        "state unchanged after recv discard"
    );

    pass!()
}

// =============================================================================
// 5.T9: TCP data round-trip — send data, simulate peer echo, recv it back.
// =============================================================================

pub fn test_tcp_data_roundtrip() -> TestResult {
    tcp::reset_all();

    let (id, _lp, _iss) = establish_connection();

    // Write data into the send buffer.
    let written = tcp::send(id, b"ping").expect("tcp_send should succeed");
    assert_eq_test!(written, 4, "should write 4 bytes");

    // Poll transmit to get the outgoing segment.
    let mut tx_buf: KBox<[u8; 1500]> = KBox::zeroed().expect("alloc");
    let result = tcp::poll_transmit(id, &mut *tx_buf, 0);
    assert_test!(result.is_some(), "should have data to transmit");
    let (seg, payload_len, _) = result.unwrap();
    assert_eq_test!(payload_len, 4, "transmitted payload should be 4 bytes");
    assert_eq_test!(&tx_buf[..4], b"ping", "payload should be 'ping'");

    // Simulate peer ACK + echo data back.
    let (tuple, rcv_nxt) = tcp::with_pcb(id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let ack_hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: seg.seq_num.wrapping_add(payload_len as u32),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 65535,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(tuple.remote_ip, tuple.local_ip, &ack_hdr, &[], b"pong", 0);

    // Read the echoed data.
    let mut buf = [0u8; 64];
    let n = tcp::recv(id, &mut buf).expect("recv should succeed");
    assert_eq_test!(n, 4, "should read 4 bytes");
    assert_eq_test!(&buf[..4], b"pong", "data should be 'pong'");

    pass!()
}

// =============================================================================
// 5.T10: TCP send buffer space and flow control.
// =============================================================================

pub fn test_tcp_send_buffer_space() -> TestResult {
    tcp::reset_all();

    let (id, _lp, _iss) = establish_connection();

    let initial_space = tcp::send_buffer_space(id);
    assert_test!(initial_space > 0, "initial send buffer should have space");

    // Fill some of the buffer.
    let data = [0xABu8; 1024];
    let written = tcp::send(id, &data).expect("tcp_send should succeed");
    assert_eq_test!(written, 1024, "should write 1024 bytes");

    let remaining = tcp::send_buffer_space(id);
    assert_eq_test!(
        remaining,
        initial_space - 1024,
        "send buffer space should decrease"
    );

    // has_pending_data should be true.
    assert_test!(
        tcp::has_pending_data(id),
        "should have pending data after send"
    );

    pass!()
}

// =============================================================================
// 5.T11: Full FIN teardown — active close (Established→FinWait1→FinWait2→
//        TimeWait) and passive close (Established→CloseWait→LastAck→Closed).
// =============================================================================

pub fn test_fin_full_teardown() -> TestResult {
    tcp::reset_all();

    // --- Active close path: Established → FIN_WAIT_1 → FIN_WAIT_2 → TIME_WAIT ---
    let (id, _lp, _iss) = establish_connection();

    // Initiate close (sends FIN).
    let close_result = tcp::close(id);
    assert_test!(close_result.is_ok(), "tcp_close should succeed");
    let fin_seg = close_result.unwrap();
    assert_test!(fin_seg.is_some(), "should produce FIN segment");
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::FinWait1),
        "should be FIN_WAIT_1 after close"
    );

    // Peer ACKs our FIN → FIN_WAIT_2.
    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let fin_ack_hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt, // ACKs our FIN
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 65535,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(tuple.remote_ip, tuple.local_ip, &fin_ack_hdr, &[], &[], 0);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::FinWait2),
        "should be FIN_WAIT_2 after FIN ack"
    );

    // Peer sends FIN → TIME_WAIT.
    deliver_peer_fin(id);
    assert_eq_test!(
        tcp::get_state(id),
        Some(TcpState::TimeWait),
        "should be TIME_WAIT after peer FIN"
    );

    // --- Passive close path: Established → CLOSE_WAIT → LAST_ACK → CLOSED ---
    tcp::reset_all();
    let (id2, _lp2, _iss2) = establish_connection();

    // Peer sends FIN → CLOSE_WAIT.
    deliver_peer_fin(id2);
    assert_eq_test!(
        tcp::get_state(id2),
        Some(TcpState::CloseWait),
        "should be CLOSE_WAIT after peer FIN"
    );

    // We close → sends our FIN → LAST_ACK.
    let close_result2 = tcp::close(id2);
    assert_test!(close_result2.is_ok(), "tcp_close should succeed");
    assert_eq_test!(
        tcp::get_state(id2),
        Some(TcpState::LastAck),
        "should be LAST_ACK after close from CLOSE_WAIT"
    );

    // Peer ACKs our FIN → CLOSED (released).
    let (tuple2, rcv_nxt2, snd_nxt2) = tcp::with_pcb(id2, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let final_ack_hdr = TcpHeader {
        src_port: tuple2.remote_port,
        dst_port: tuple2.local_port,
        seq_num: rcv_nxt2,
        ack_num: snd_nxt2,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 65535,
        checksum: 0,
        urgent_ptr: 0,
    };
    tcp::input(
        tuple2.remote_ip,
        tuple2.local_ip,
        &final_ack_hdr,
        &[],
        &[],
        0,
    );
    // Connection should be released.
    assert_test!(
        tcp::get_state(id2).is_none(),
        "should be released after LAST_ACK acked"
    );

    pass!()
}

// tests
slopos_testing::stest!(name = test_syn_queue_overflow, suite = tcp_socket);
slopos_testing::stest!(name = test_accept_queue_overflow, suite = tcp_socket);
slopos_testing::stest!(
    name = test_syn_ack_retransmit_exhaustion,
    suite = tcp_socket
);
slopos_testing::stest!(name = test_duplicate_syn_retransmits, suite = tcp_socket);
// tests
slopos_testing::stest!(name = test_push_accepted_basic, suite = tcp_socket);
slopos_testing::stest!(
    name = test_push_accepted_respects_backlog,
    suite = tcp_socket
);
slopos_testing::stest!(
    name = test_listen_state_backlog_clamping,
    suite = tcp_socket
);
slopos_testing::stest!(name = test_accept_fifo_order, suite = tcp_socket);
slopos_testing::stest!(name = test_listen_state_clear, suite = tcp_socket);
// tests
slopos_testing::stest!(name = test_fin_handling_eof, suite = tcp_socket);
slopos_testing::stest!(name = test_shutdown_write_sends_fin, suite = tcp_socket);
slopos_testing::stest!(
    name = test_shutdown_read_discards_buffer,
    suite = tcp_socket
);
slopos_testing::stest!(name = test_tcp_data_roundtrip, suite = tcp_socket);
slopos_testing::stest!(name = test_tcp_send_buffer_space, suite = tcp_socket);
slopos_testing::stest!(name = test_fin_full_teardown, suite = tcp_socket);

/// A listener's established children are released with it.
///
/// A child installed on the final ACK holds a shard slot and names its
/// listener as its socket. Nothing else reclaims those once the listener is
/// gone — they would sit until a RST, a FIN or TIME_WAIT expiry, which is a
/// slow leak of the machine-wide table driven entirely by a remote peer.
pub fn test_listener_release_reclaims_its_children() -> TestResult {
    use crate::tcp::SocketId;

    tcp_common::reset_all();

    let server_ip = [10, 0, 0, 1];
    let client_ip = [10, 0, 0, 2];
    let owner = SocketId(7);

    let listen_id = match tcp::listen(server_ip, 8080) {
        Ok(id) => id,
        Err(e) => return fail!("listen failed: {:?}", e),
    };
    // Children inherit the listener's socket, which is the link the release
    // walks.
    tcp::set_socket_idx(listen_id, Some(owner));
    let before = tcp::active_count();

    for i in 0..2u16 {
        let port = 51000 + i;
        let syn = TcpHeader {
            src_port: port,
            dst_port: 8080,
            seq_num: 1000 + i as u32,
            ack_num: 0,
            data_offset: 5,
            flags: crate::tcp::TCP_FLAG_SYN,
            window_size: 32768,
            checksum: 0,
            urgent_ptr: 0,
        };
        let r = tcp::input(client_ip, server_ip, &syn, &[], &[], 0);
        let Some(syn_ack) = r.segments().next().cloned() else {
            return fail!("no SYN+ACK for handshake {}", i);
        };
        let ack = TcpHeader {
            src_port: port,
            dst_port: 8080,
            seq_num: 1001 + i as u32,
            ack_num: syn_ack.seq_num.wrapping_add(1),
            data_offset: 5,
            flags: TCP_FLAG_ACK,
            window_size: 32768,
            checksum: 0,
            urgent_ptr: 0,
        };
        let _ = tcp::input(client_ip, server_ip, &ack, &[], &[], 0);
    }

    if tcp::active_count() != before + 2 {
        return fail!(
            "expected {} active connections after two handshakes, got {}",
            before + 2,
            tcp::active_count()
        );
    }

    let orphans = tcp::release_children_of(owner);
    assert_eq_test!(orphans.len(), 2, "both children reported for reset");
    assert_eq_test!(
        tcp::active_count(),
        before,
        "the children kept their table slots"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_listener_release_reclaims_its_children,
    suite = tcp_socket
);
