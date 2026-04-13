//! Integration tests — Two-Queue Listen Model & TCP Demux Table.
//!
//! Tests the SYN queue, accept queue, SYN-ACK retransmission, overflow
//! behavior of [`TcpListenState`], and the [`TcpDemuxTable`] for fast
//! connection/listener lookup.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::listener::{
    SYN_QUEUE_MAX, SYN_RETRIES_MAX, TcpDemuxTable, TcpListenState, reset_syn_entry_keys,
};
use crate::types::{Ipv4Addr, Port, SockAddr};
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
    let mut listen = TcpListenState::new(64, local_addr());

    // Fill the SYN queue to capacity.
    for i in 0..SYN_QUEUE_MAX as u16 {
        let client = client_addr(i);
        let result = listen.on_syn(client, 1000 + i as u32, 1460, 0, None);
        assert_test!(
            result.is_some(),
            "SYN {} should succeed (queue not full yet)"
        );
    }

    assert_eq_test!(
        listen.syn_queue_len(),
        SYN_QUEUE_MAX,
        "SYN queue at capacity"
    );

    // Next SYN should be silently dropped (no RST).
    let overflow_client = client_addr(SYN_QUEUE_MAX as u16);
    let overflow_result = listen.on_syn(overflow_client, 9999, 1460, 0, None);
    assert_test!(
        overflow_result.is_none(),
        "SYN queue full -> silently dropped (no RST)"
    );

    // Queue length unchanged.
    assert_eq_test!(
        listen.syn_queue_len(),
        SYN_QUEUE_MAX,
        "SYN queue still at capacity after overflow"
    );

    pass!()
}

// =============================================================================
// T2: Accept queue overflow — backlog=2, complete 3 connections, 3rd stays in
//     SYN queue
// =============================================================================

pub fn test_accept_queue_overflow() -> TestResult {
    reset_syn_entry_keys();
    let backlog = 2usize;
    let mut listen = TcpListenState::new(backlog, local_addr());

    // Send 3 SYNs.
    for i in 0..3u16 {
        let client = client_addr(i);
        let syn_ack = listen.on_syn(client, 1000 + i as u32, 1460, 0, None);
        assert_test!(syn_ack.is_some(), "SYN should succeed");
    }
    assert_eq_test!(listen.syn_queue_len(), 3, "3 entries in SYN queue");

    // Complete first two with ACK (should move to accept queue).
    for i in 0..2u16 {
        let client = client_addr(i);
        // The SYN-ACK's ack_num will be iss+1; we need to know the ISS.
        // Each on_syn generates a unique ISS. We need to get the ISS from the
        // returned SYN-ACK. But we already consumed those. Re-approach: send a
        // duplicate SYN to get the SYN-ACK back (which retransmits existing).
        let retransmit = listen.on_syn(client, 1000 + i as u32, 1460, 0, None);
        let syn_ack = retransmit.expect("duplicate SYN retransmits SYN-ACK");
        let iss = syn_ack.seq_num;
        let ack_num = iss.wrapping_add(1);

        let accepted = listen.on_ack(client, ack_num);
        assert_test!(
            accepted.is_some(),
            "connection should complete (accept queue has room)"
        );
    }

    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue has 2");
    assert_eq_test!(listen.syn_queue_len(), 1, "1 remaining in SYN queue");

    // Complete third — accept queue is full, should stay in SYN queue.
    let client2 = client_addr(2);
    let retransmit2 = listen.on_syn(client2, 1002, 1460, 0, None);
    let syn_ack2 = retransmit2.expect("duplicate SYN retransmits SYN-ACK");
    let iss2 = syn_ack2.seq_num;
    let ack_num2 = iss2.wrapping_add(1);

    let overflow = listen.on_ack(client2, ack_num2);
    assert_test!(
        overflow.is_none(),
        "accept queue full -> connection stays in SYN queue"
    );
    assert_eq_test!(listen.syn_queue_len(), 1, "still 1 in SYN queue");
    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue still full");

    // Drain one from accept queue, then retry ACK — should succeed now.
    let _ = listen.accept();
    assert_eq_test!(listen.accept_queue_len(), 1, "accept queue drained to 1");

    let accepted_now = listen.on_ack(client2, ack_num2);
    assert_test!(
        accepted_now.is_some(),
        "after draining accept queue, 3rd connection completes"
    );
    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue back to 2");
    assert_eq_test!(listen.syn_queue_len(), 0, "SYN queue drained");

    pass!()
}

// =============================================================================
// T3: SYN-ACK retransmission — verify 5 retransmissions with backoff, then
//     removal
// =============================================================================

pub fn test_syn_ack_retransmit_exhaustion() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(16, local_addr());

    let client = client_addr(0);
    let syn_ack = listen.on_syn(client, 5000, 1460, 0, None);
    assert_test!(syn_ack.is_some(), "initial SYN accepted");
    assert_eq_test!(listen.syn_queue_len(), 1, "1 entry in SYN queue");

    // The SYN-ACK returned by on_syn tells us the ISS.
    let syn_ack = syn_ack.unwrap();
    let original_iss = syn_ack.seq_num;

    // Determine the entry's key — it's the first key allocated (we reset keys).
    // Key=1 because we reset_syn_entry_keys() above.
    let entry_key = 1u32;

    // Simulate 5 retransmit timer firings — each should return a SYN-ACK.
    for _retry in 1..=SYN_RETRIES_MAX {
        let retransmit = listen.on_retransmit(entry_key);
        assert_test!(retransmit.is_some(), "retransmit should succeed on retry");
        let seg = retransmit.unwrap();
        assert_eq_test!(
            seg.seq_num,
            original_iss,
            "ISS unchanged across retransmits"
        );
        assert_eq_test!(listen.syn_queue_len(), 1, "entry still in SYN queue");
    }

    // 6th retransmit (retry > SYN_RETRIES_MAX) — entry should be removed.
    let exhausted = listen.on_retransmit(entry_key);
    assert_test!(
        exhausted.is_none(),
        "retransmit returns None after max retries"
    );
    assert_eq_test!(
        listen.syn_queue_len(),
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
    let mut listen = TcpListenState::new(16, local_addr());

    let client = client_addr(42);
    let first = listen.on_syn(client, 7000, 1460, 0, None);
    assert_test!(first.is_some(), "first SYN accepted");
    let first_iss = first.unwrap().seq_num;

    // Send duplicate SYN — should retransmit the same SYN-ACK.
    let dup = listen.on_syn(client, 7000, 1460, 100, None);
    assert_test!(dup.is_some(), "duplicate SYN triggers SYN-ACK retransmit");
    let dup_iss = dup.unwrap().seq_num;

    assert_eq_test!(
        first_iss,
        dup_iss,
        "duplicate SYN returns same ISS (same entry)"
    );
    assert_eq_test!(listen.syn_queue_len(), 1, "no duplicate entry created");

    pass!()
}

// =============================================================================
// TcpDemuxTable — established connection lookup
// =============================================================================

pub fn test_demux_register_established_lookup() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);
    let conn_id = 7u32;

    let result = demux.register_established(local_ip, local_port, remote_ip, remote_port, conn_id);
    assert_test!(result.is_ok(), "register_established should succeed");

    // Exact 4-tuple match should find the connection.
    let found = demux.lookup_established(local_ip, local_port, remote_ip, remote_port);
    assert_eq_test!(
        found,
        Some(conn_id),
        "lookup should return the registered conn_id"
    );

    // Different remote port should NOT match.
    let not_found = demux.lookup_established(local_ip, local_port, remote_ip, Port(40001));
    assert_test!(
        not_found.is_none(),
        "different remote port should not match"
    );

    // Different remote IP should NOT match.
    let not_found2 = demux.lookup_established(
        local_ip,
        local_port,
        Ipv4Addr([192, 168, 1, 101]),
        remote_port,
    );
    assert_test!(not_found2.is_none(), "different remote IP should not match");

    pass!()
}

// =============================================================================
// TcpDemuxTable — duplicate established registration rejected
// =============================================================================

pub fn test_demux_established_duplicate_rejected() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);

    let r1 = demux.register_established(local_ip, local_port, remote_ip, remote_port, 1);
    assert_test!(r1.is_ok(), "first registration should succeed");

    let r2 = demux.register_established(local_ip, local_port, remote_ip, remote_port, 2);
    assert_test!(r2.is_err(), "duplicate 4-tuple registration should fail");

    pass!()
}

// =============================================================================
// TcpDemuxTable — unregister established
// =============================================================================

pub fn test_demux_unregister_established() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);
    let conn_id = 3u32;

    let _ = demux.register_established(local_ip, local_port, remote_ip, remote_port, conn_id);

    // Should find it before unregister.
    assert_test!(
        demux
            .lookup_established(local_ip, local_port, remote_ip, remote_port)
            .is_some(),
        "should find before unregister"
    );

    demux.unregister_established(conn_id);

    // Should NOT find it after unregister.
    assert_test!(
        demux
            .lookup_established(local_ip, local_port, remote_ip, remote_port)
            .is_none(),
        "should not find after unregister"
    );

    pass!()
}

// =============================================================================
// TcpDemuxTable — listener registration and lookup
// =============================================================================

pub fn test_demux_register_listener_lookup() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(8080);
    let sock_idx = 5u32;

    let result = demux.register_listener(local_ip, local_port, sock_idx);
    assert_test!(result.is_ok(), "register_listener should succeed");

    // Exact 2-tuple match.
    let found = demux.lookup_listener(local_ip, local_port);
    assert_eq_test!(
        found,
        Some(sock_idx),
        "lookup should return registered sock_idx"
    );

    // Different port should NOT match.
    let not_found = demux.lookup_listener(local_ip, Port(9090));
    assert_test!(not_found.is_none(), "different port should not match");

    pass!()
}

// =============================================================================
// TcpDemuxTable — listener wildcard (0.0.0.0) fallback
// =============================================================================

pub fn test_demux_listener_wildcard_fallback() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let wildcard = Ipv4Addr::UNSPECIFIED;
    let local_port = Port(80);
    let sock_idx = 10u32;

    let result = demux.register_listener(wildcard, local_port, sock_idx);
    assert_test!(
        result.is_ok(),
        "wildcard listener registration should succeed"
    );

    // Lookup with a specific IP should fall back to wildcard.
    let found = demux.lookup_listener(Ipv4Addr([10, 0, 0, 1]), local_port);
    assert_eq_test!(found, Some(sock_idx), "wildcard fallback should match");

    // Exact match takes priority over wildcard.
    let specific_sock = 20u32;
    let specific_ip = Ipv4Addr([10, 0, 0, 1]);
    let r2 = demux.register_listener(specific_ip, local_port, specific_sock);
    assert_test!(r2.is_ok(), "specific listener registration should succeed");

    let found2 = demux.lookup_listener(specific_ip, local_port);
    assert_eq_test!(
        found2,
        Some(specific_sock),
        "exact IP should take priority over wildcard"
    );

    // Other IPs should still fall back to wildcard.
    let found3 = demux.lookup_listener(Ipv4Addr([10, 0, 0, 2]), local_port);
    assert_eq_test!(
        found3,
        Some(sock_idx),
        "non-matching IP should fall back to wildcard"
    );

    pass!()
}

// =============================================================================
// TcpDemuxTable — unregister listener
// =============================================================================

pub fn test_demux_unregister_listener() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(8080);
    let sock_idx = 5u32;

    let _ = demux.register_listener(local_ip, local_port, sock_idx);

    assert_test!(
        demux.lookup_listener(local_ip, local_port).is_some(),
        "should find before unregister"
    );

    demux.unregister_listener(sock_idx);

    assert_test!(
        demux.lookup_listener(local_ip, local_port).is_none(),
        "should not find after unregister"
    );

    pass!()
}

// =============================================================================
// TcpDemuxTable — clear wipes all entries
// =============================================================================

pub fn test_demux_clear() -> TestResult {
    let mut demux = TcpDemuxTable::new();
    demux.clear();

    let _ = demux.register_established(
        Ipv4Addr([10, 0, 0, 1]),
        Port(80),
        Ipv4Addr([192, 168, 1, 1]),
        Port(40000),
        1,
    );
    let _ = demux.register_listener(Ipv4Addr([10, 0, 0, 1]), Port(8080), 5);

    demux.clear();

    assert_test!(
        demux
            .lookup_established(
                Ipv4Addr([10, 0, 0, 1]),
                Port(80),
                Ipv4Addr([192, 168, 1, 1]),
                Port(40000),
            )
            .is_none(),
        "established should be cleared"
    );
    assert_test!(
        demux
            .lookup_listener(Ipv4Addr([10, 0, 0, 1]), Port(8080))
            .is_none(),
        "listener should be cleared"
    );

    pass!()
}

// =============================================================================
// TcpListenState — push_accepted enqueues completed connections
// =============================================================================

pub fn test_push_accepted_basic() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(4, local_addr());

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
    let mut listen = TcpListenState::new(backlog, local_addr());

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
    let listen_min = TcpListenState::new(0, local_addr());
    assert_eq_test!(
        listen_min.backlog(),
        crate::tcp::listener::BACKLOG_MIN,
        "backlog=0 should clamp to BACKLOG_MIN"
    );

    // Backlog 999 should clamp to BACKLOG_MAX (128).
    let listen_max = TcpListenState::new(999, local_addr());
    assert_eq_test!(
        listen_max.backlog(),
        crate::tcp::listener::BACKLOG_MAX,
        "backlog=999 should clamp to BACKLOG_MAX"
    );

    // Normal backlog should pass through.
    let listen_normal = TcpListenState::new(16, local_addr());
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
    let mut listen = TcpListenState::new(8, local_addr());

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
    let mut listen = TcpListenState::new(16, local_addr());

    // Add something to SYN queue.
    let _ = listen.on_syn(client_addr(0), 1000, 1460, 0, None);
    assert_eq_test!(listen.syn_queue_len(), 1, "SYN queue has 1 entry");

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

    // Clear both queues.
    listen.clear();
    assert_eq_test!(listen.syn_queue_len(), 0, "SYN queue cleared");
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
    let mut tx_buf = [0u8; 1500];
    let result = tcp::poll_transmit(id, &mut tx_buf, 0);
    assert_test!(result.is_some(), "should have data to transmit");
    let (seg, payload_len) = result.unwrap();
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

slopos_testing::define_test_suite!(
    tcp_socket,
    [
        // tests
        test_syn_queue_overflow,
        test_accept_queue_overflow,
        test_syn_ack_retransmit_exhaustion,
        test_duplicate_syn_retransmits,
        // tests
        test_demux_register_established_lookup,
        test_demux_established_duplicate_rejected,
        test_demux_unregister_established,
        test_demux_register_listener_lookup,
        test_demux_listener_wildcard_fallback,
        test_demux_unregister_listener,
        test_demux_clear,
        // tests
        test_push_accepted_basic,
        test_push_accepted_respects_backlog,
        test_listen_state_backlog_clamping,
        test_accept_fifo_order,
        test_listen_state_clear,
        // tests
        test_fin_handling_eof,
        test_shutdown_write_sends_fin,
        test_shutdown_read_discards_buffer,
        test_tcp_data_roundtrip,
        test_tcp_send_buffer_space,
        test_fin_full_teardown,
    ]
);
