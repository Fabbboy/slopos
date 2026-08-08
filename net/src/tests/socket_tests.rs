use slopos_abi::net::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::socket::*;
use crate::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader, TcpState};
use crate::types::NetError;

fn reset() {
    socket_reset_all();
}

fn connect_and_establish() -> Result<(u32, tcp::ConnId), &'static str> {
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    if sock < 0 {
        return Err("socket_create failed");
    }
    let sock = sock as u32;

    // Use non-blocking connect so the test doesn't block waiting for a
    // SYN-ACK that can only arrive from manual injection below.
    socket_set_nonblocking(sock, true);

    let rc = socket_connect(sock, [10, 0, 0, 2], 80);
    // Non-blocking connect returns EINPROGRESS (-115) or 0.
    if rc < 0 && rc != -115 {
        return Err("socket_connect failed");
    }

    let Some(tcp_id) = socket_lookup_tcp_idx(sock) else {
        return Err("no tcp idx");
    };
    let (tuple, iss) = tcp::with_pcb(tcp_id, |pcb| {
        let iss = match &pcb.state {
            tcp::PcbState::SynSent(s) => s.iss.raw(),
            tcp::PcbState::Data(d) => d.iss.raw(),
            _ => return Err("unexpected PCB state"),
        };
        Ok((pcb.tuple, iss))
    })
    .ok_or("no tcp conn")??;

    let syn_ack = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: 9000,
        ack_num: iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(tuple.remote_ip, tuple.local_ip, &syn_ack, &[], &[], 0);
    socket_notify_tcp_activity(&result);

    Ok((sock, tcp_id))
}

pub fn test_socket_create_tcp() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_test!(idx >= 0, "tcp socket create succeeds");
    pass!()
}

pub fn test_socket_create_udp() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0, SocketOwner::UNOWNED);
    assert_test!(idx >= 0, "udp socket create succeeds");
    pass!()
}

pub fn test_socket_create_invalid_domain() -> TestResult {
    reset();
    let idx = socket_create(1, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_test!(idx < 0, "invalid domain fails");
    pass!()
}

pub fn test_socket_create_invalid_type() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, 99, 0, SocketOwner::UNOWNED);
    assert_test!(idx < 0, "invalid type fails");
    pass!()
}

pub fn test_socket_table_full() -> TestResult {
    reset();
    // SlabSocketTable grows on demand up to MAX_CAPACITY (1024).
    // Verify we can allocate beyond the initial 64-slot capacity.
    for i in 0..128 {
        if socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) < 0 {
            return fail!("socket allocation failed at index {}", i);
        }
    }
    // 129th socket should still succeed (table grows to 256).
    assert_test!(
        socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) >= 0,
        "129th socket succeeds (growable table)"
    );
    pass!()
}

pub fn test_socket_bind_valid() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    pass!()
}

pub fn test_socket_bind_specific_addr() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(idx, [10, 0, 0, 1], 80), 0);
    pass!()
}

pub fn test_socket_bind_invalid_idx() -> TestResult {
    reset();
    assert_test!(
        socket_bind(999, [0, 0, 0, 0], 8080) < 0,
        "invalid socket idx fails"
    );
    pass!()
}

pub fn test_socket_bind_already_bound() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    assert_test!(
        socket_bind(idx, [0, 0, 0, 0], 8081) < 0,
        "double bind fails"
    );
    pass!()
}

pub fn test_socket_listen_after_bind() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_listen(idx, 16), 0);
    pass!()
}

pub fn test_socket_listen_without_bind() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_test!(socket_listen(idx, 16) < 0, "listen without bind fails");
    pass!()
}

pub fn test_socket_listen_on_connected() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    assert_test!(
        socket_listen(sock, 4) < 0,
        "listen on connected socket fails"
    );
    pass!()
}

pub fn test_socket_connect_creates_tcp_connection() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    socket_set_nonblocking(sock, true);
    let rc = socket_connect(sock, [10, 0, 0, 2], 80);
    assert_test!(
        rc == 0 || rc == -115,
        "non-blocking connect should succeed or EINPROGRESS"
    );
    let tcp_id = socket_lookup_tcp_idx(sock).unwrap();
    assert_eq_test!(tcp::get_state(tcp_id), Some(TcpState::SynSent));
    pass!()
}

pub fn test_socket_connect_invalid_socket() -> TestResult {
    reset();
    assert_test!(
        socket_connect(12345, [10, 0, 0, 2], 80) < 0,
        "invalid connect fails"
    );
    pass!()
}

pub fn test_socket_connect_already_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    socket_set_nonblocking(sock, true);
    let rc = socket_connect(sock, [10, 0, 0, 2], 80);
    assert_test!(rc == 0 || rc == -115, "first non-blocking connect");
    assert_test!(
        socket_connect(sock, [10, 0, 0, 2], 80) < 0,
        "double connect fails"
    );
    pass!()
}

pub fn test_socket_send_returns_error_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    let payload = [1u8, 2, 3];
    assert_test!(
        socket_send(sock, &payload) < 0,
        "send without connect fails"
    );
    pass!()
}

pub fn test_socket_recv_returns_error_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    let mut buf = [0u8; 8];
    assert_test!(
        socket_recv(sock, &mut buf) < 0,
        "recv without connect fails"
    );
    pass!()
}

pub fn test_socket_send_buffer_space() -> TestResult {
    reset();
    let (sock, tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    let probe = socket_max_send_probe(sock, 1024);
    assert_test!(probe >= 0, "send probe succeeds");
    assert_test!(
        probe as usize <= tcp::send_buffer_space(tcp_id),
        "probe <= tcp space"
    );
    pass!()
}

pub fn test_socket_recv_empty() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    let mut buf = [0u8; 16];
    let n = socket_recv(sock, &mut buf);
    assert_test!(n == 0 || n < 0, "recv empty returns 0 or error");
    pass!()
}

pub fn test_socket_close_valid() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_close(sock), 0);
    pass!()
}

pub fn test_socket_close_invalid() -> TestResult {
    reset();
    assert_test!(socket_close(4444) < 0, "close invalid fails");
    pass!()
}

pub fn test_socket_close_frees_slot() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_eq_test!(socket_close(sock as u32), 0);
    let next = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_eq_test!(next, sock);
    pass!()
}

pub fn test_socket_poll_readable_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_poll_readable(sock), 0);
    pass!()
}

pub fn test_socket_poll_writable_connected() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    assert_eq_test!(
        socket_poll_writable(sock) & slopos_abi::syscall::POLLOUT as u32,
        slopos_abi::syscall::POLLOUT as u32
    );
    pass!()
}

pub fn test_socket_state_after_create() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_get_state(sock), Some(SocketState::Unbound));
    pass!()
}

pub fn test_socket_state_after_bind() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_get_state(sock), Some(SocketState::Bound));
    pass!()
}

pub fn test_socket_reset_all() -> TestResult {
    reset();
    let _ = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    let _ = socket_create(AF_INET, SOCK_DGRAM, 0, SocketOwner::UNOWNED);
    assert_test!(socket_count_active() >= 2, "active before reset");
    socket_reset_all();
    assert_eq_test!(socket_count_active(), 0);
    assert_eq_test!(tcp::active_count(), 0);
    pass!()
}

pub fn test_socket_accept_no_pending_returns_eagain() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_listen(sock, 8), 0);
    assert_test!(
        socket_accept(sock, core::ptr::null_mut(), core::ptr::null_mut()) < 0,
        "accept without pending fails"
    );
    pass!()
}

pub fn test_bounded_queue_push_pop_capacity() -> TestResult {
    let mut q = BoundedQueue::new(3);
    assert_eq_test!(q.capacity(), 3);
    assert_test!(q.is_empty(), "queue starts empty");

    assert_test!(q.push(10), "push first element");
    assert_test!(q.push(20), "push second element");
    assert_test!(q.push(30), "push third element");
    assert_test!(q.is_full(), "queue should be full");

    assert_eq_test!(q.pop(), Some(10));
    assert_eq_test!(q.pop(), Some(20));
    assert_eq_test!(q.pop(), Some(30));
    assert_eq_test!(q.pop(), None);
    pass!()
}

pub fn test_bounded_queue_overflow_returns_false() -> TestResult {
    let mut q = BoundedQueue::new(1);
    assert_test!(q.push(1), "first push succeeds");
    assert_test!(!q.push(2), "overflow push must fail");
    assert_eq_test!(q.len(), 1);
    assert_eq_test!(q.pop(), Some(1));
    pass!()
}

pub fn test_bounded_queue_clear_and_resize() -> TestResult {
    let mut q = BoundedQueue::new(3);
    let _ = q.push(1);
    let _ = q.push(2);
    let _ = q.push(3);
    q.clear();

    assert_test!(q.is_empty(), "clear empties queue");
    assert_eq_test!(q.pop(), None);

    let _ = q.push(4);
    let _ = q.push(5);
    let _ = q.push(6);
    q.resize(2);
    assert_eq_test!(q.capacity(), 2);
    assert_eq_test!(q.len(), 2);
    assert_eq_test!(q.pop(), Some(4));
    assert_eq_test!(q.pop(), Some(5));
    assert_eq_test!(q.pop(), None);
    pass!()
}

pub fn test_slab_socket_table_alloc_free_get_get_mut() -> TestResult {
    let mut table = SlabSocketTable::new(2, 8);
    let idx = table
        .alloc(
            SocketInner::Tcp(TcpSocketInner {
                conn_id: None,
                listen: None,
            }),
            SocketOwner::UNOWNED,
        )
        .unwrap();

    assert_eq_test!(idx, 0);
    assert_eq_test!(table.count_active(), 1);
    assert_test!(table.get(idx).is_some(), "allocated socket is retrievable");

    {
        let sock = table.get_mut(idx).unwrap();
        sock.set_nonblocking(true);
        assert_test!(sock.is_nonblocking(), "mutable access updates flags");
    }

    table.free(idx);
    assert_test!(table.get(idx).is_none(), "freed slot is empty");
    assert_eq_test!(table.count_active(), 0);
    pass!()
}

pub fn test_slab_socket_table_grows_and_enforces_max() -> TestResult {
    let mut table = SlabSocketTable::new(2, 4);
    assert_eq_test!(table.capacity(), 2);

    for _ in 0..4 {
        let idx = table.alloc(SocketInner::Udp(UdpSocketInner), SocketOwner::UNOWNED);
        assert_test!(idx.is_some(), "allocation within max should succeed");
    }

    assert_eq_test!(table.capacity(), 4);
    assert_test!(
        table
            .alloc(SocketInner::Raw(RawSocketInner), SocketOwner::UNOWNED)
            .is_none(),
        "allocation beyond max must fail"
    );
    pass!()
}

pub fn test_ephemeral_port_allocator_alloc_release_round_robin() -> TestResult {
    let mut alloc =
        slopos_ostd::KBox::try_init(EphemeralPortAllocator::init_default()).expect("alloc");

    let p1 = alloc.alloc().unwrap();
    let p2 = alloc.alloc().unwrap();
    assert_eq_test!(p1.0, EphemeralPortAllocator::EPHEMERAL_PORT_START);
    assert_eq_test!(p2.0, EphemeralPortAllocator::EPHEMERAL_PORT_START + 1);

    alloc.release(p1);
    let p3 = alloc.alloc().unwrap();
    assert_eq_test!(p3.0, EphemeralPortAllocator::EPHEMERAL_PORT_START + 2);

    assert_test!(!alloc.is_in_use(p1), "released port should be free");
    assert_test!(alloc.is_in_use(p2), "second allocated port is still in use");
    assert_test!(alloc.is_in_use(p3), "newly allocated port is in use");
    pass!()
}

pub fn test_ephemeral_port_allocator_exhaustion_and_no_duplicates() -> TestResult {
    let mut alloc =
        slopos_ostd::KBox::try_init(EphemeralPortAllocator::init_default()).expect("alloc");
    let mut first_ports = [0u16; 64];

    for i in 0..first_ports.len() {
        let p = alloc.alloc().unwrap();
        first_ports[i] = p.0;
        for prev in first_ports.iter().take(i) {
            assert_test!(*prev != p.0, "ephemeral allocation must be unique");
        }
    }

    let mut total = first_ports.len();
    while alloc.alloc().is_some() {
        total += 1;
    }
    assert_eq_test!(total, EphemeralPortAllocator::EPHEMERAL_PORT_COUNT);
    assert_test!(
        alloc.alloc().is_none(),
        "allocator should report exhaustion"
    );
    assert_eq_test!(alloc.available(), 0);
    pass!()
}

pub fn test_socket_options_defaults_and_validation() -> TestResult {
    let opts = SocketOptions::new();
    assert_test!(!opts.reuse_addr, "reuse_addr default false");
    assert_eq_test!(opts.recv_buf_size, SocketOptions::RECV_BUF_DEFAULT);
    assert_eq_test!(opts.send_buf_size, SocketOptions::SEND_BUF_DEFAULT);
    assert_eq_test!(opts.recv_timeout, None);
    assert_eq_test!(opts.send_timeout, None);
    assert_test!(!opts.keepalive, "keepalive default false");
    assert_test!(!opts.tcp_nodelay, "tcp_nodelay default false");

    assert_eq_test!(
        SocketOptions::validate_recv_buf_size(SocketOptions::RECV_BUF_MIN),
        Ok(SocketOptions::RECV_BUF_MIN)
    );
    assert_eq_test!(
        SocketOptions::validate_send_buf_size(SocketOptions::SEND_BUF_MAX),
        Ok(SocketOptions::SEND_BUF_MAX)
    );
    assert_eq_test!(
        SocketOptions::validate_recv_buf_size(SocketOptions::RECV_BUF_MIN - 1),
        Err(NetError::InvalidArgument)
    );
    assert_eq_test!(
        SocketOptions::validate_send_buf_size(SocketOptions::SEND_BUF_MAX + 1),
        Err(NetError::InvalidArgument)
    );
    pass!()
}

pub fn test_socket_flags_set_clear_contains() -> TestResult {
    let mut flags = SocketFlags::NONE;
    assert_test!(
        !flags.contains(SocketFlags::O_NONBLOCK),
        "starts without nonblocking"
    );

    flags.set(SocketFlags::O_NONBLOCK);
    flags.set(SocketFlags::SHUT_RD);
    assert_test!(flags.contains(SocketFlags::O_NONBLOCK), "nonblocking set");
    assert_test!(flags.contains(SocketFlags::SHUT_RD), "read shutdown set");

    flags.clear(SocketFlags::O_NONBLOCK);
    assert_test!(
        !flags.contains(SocketFlags::O_NONBLOCK),
        "nonblocking cleared"
    );
    assert_eq_test!(
        SocketFlags::from_bits(flags.bits()),
        SocketFlags::from_bits(SocketFlags::SHUT_RD.bits())
    );
    pass!()
}

pub fn test_socket_new_defaults_and_helpers() -> TestResult {
    let mut sock = Socket::new(SocketInner::Udp(UdpSocketInner));

    assert_eq_test!(sock.state, SocketState::Unbound);
    assert_test!(sock.local_addr.is_none(), "local addr starts unset");
    assert_test!(sock.remote_addr.is_none(), "remote addr starts unset");
    assert_eq_test!(
        sock.recv_queue.capacity(),
        Socket::RECV_QUEUE_DEFAULT_CAPACITY
    );
    assert_eq_test!(sock.pending_error, None);
    assert_test!(!sock.is_nonblocking(), "socket starts blocking");

    sock.set_nonblocking(true);
    assert_test!(sock.is_nonblocking(), "set_nonblocking enables flag");

    sock.flags.set(SocketFlags::SHUT_RD);
    sock.flags.set(SocketFlags::SHUT_WR);
    assert_test!(
        sock.is_read_shutdown(),
        "read shutdown helper reflects flag"
    );
    assert_test!(
        sock.is_write_shutdown(),
        "write shutdown helper reflects flag"
    );

    sock.pending_error = Some(NetError::WouldBlock);
    assert_eq_test!(sock.take_pending_error(), Some(NetError::WouldBlock));
    assert_eq_test!(sock.take_pending_error(), None);
    pass!()
}

pub fn test_tcp_send_on_established_returns_bytes() -> TestResult {
    reset();
    let (sock, _tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    socket_set_nonblocking(sock, true);
    let payload = [0xAA_u8; 64];
    let n = socket_send(sock, &payload);
    assert_test!(n > 0, "send on established should return bytes written");
    assert_test!(
        n as usize <= payload.len(),
        "should not write more than requested"
    );
    pass!()
}

pub fn test_tcp_recv_after_peer_data() -> TestResult {
    reset();
    let (sock, tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    socket_set_nonblocking(sock, true);

    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(tcp_id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let data_hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let payload = b"hello";
    let _ = tcp::input(tuple.remote_ip, tuple.local_ip, &data_hdr, &[], payload, 0);

    let mut buf = [0u8; 32];
    let n = socket_recv(sock, &mut buf);
    assert_test!(n > 0, "recv should return data");
    assert_eq_test!(n as usize, 5);
    assert_eq_test!(&buf[..5], b"hello");
    pass!()
}

pub fn test_tcp_shutdown_wr_transitions_to_fin_wait1() -> TestResult {
    reset();
    let (sock, tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    use slopos_abi::syscall::SHUT_WR;
    assert_eq_test!(socket_shutdown(sock, SHUT_WR), 0);
    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::FinWait1),
        "SHUT_WR should transition Established -> FinWait1"
    );
    pass!()
}

pub fn test_tcp_shutdown_wr_recv_still_works() -> TestResult {
    reset();
    let (sock, tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    socket_set_nonblocking(sock, true);
    use slopos_abi::syscall::SHUT_WR;
    assert_eq_test!(socket_shutdown(sock, SHUT_WR), 0);

    let (tuple, rcv_nxt, snd_nxt) = tcp::with_pcb(tcp_id, |pcb| match &pcb.state {
        tcp::PcbState::Data(d) => (pcb.tuple, d.rcv_nxt.raw(), d.snd_nxt.raw()),
        other => panic!("expected Data state, got {}", other.name()),
    })
    .expect("PCB should exist");
    let data_hdr = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: rcv_nxt,
        ack_num: snd_nxt,
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(
        tuple.remote_ip,
        tuple.local_ip,
        &data_hdr,
        &[],
        b"post-fin",
        0,
    );

    let mut buf = [0u8; 32];
    let n = socket_recv(sock, &mut buf);
    assert_test!(n > 0, "recv after SHUT_WR should still work");
    assert_eq_test!(n as usize, 8);
    pass!()
}

pub fn test_tcp_send_after_shutdown_wr_fails() -> TestResult {
    reset();
    let (sock, _tcp_id) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    use slopos_abi::syscall::SHUT_WR;
    assert_eq_test!(socket_shutdown(sock, SHUT_WR), 0);
    let payload = [1u8; 4];
    let n = socket_send(sock, &payload);
    assert_test!(n < 0, "send after SHUT_WR should fail");
    pass!()
}

/// Regression test: reproduces the "nc: send failed (broken pipe)" scenario.
///
/// The old `socket_connect()` returned immediately without waiting for the
/// TCP 3WHS. A subsequent `socket_send()` found the socket still in
/// `Connecting` state and returned `ENOTCONN` — reported as "broken pipe".
///
/// This test uses non-blocking connect + manual 3WHS injection to verify
/// that after establishment, send works correctly.
pub fn test_tcp_send_after_blocking_connect() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_test!(sock >= 0, "socket_create succeeds");
    let sock = sock as u32;

    socket_set_nonblocking(sock, true);
    let rc = socket_connect(sock, [10, 0, 0, 2], 80);
    assert_test!(rc == 0 || rc == -115, "non-blocking connect");

    let Some(tcp_id) = socket_lookup_tcp_idx(sock) else {
        return fail!("no tcp connection after connect");
    };
    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::SynSent),
        "should be in SynSent"
    );
    let Some((tuple, iss)) = tcp::with_pcb(tcp_id, |pcb| match &pcb.state {
        tcp::PcbState::SynSent(s) => Some((pcb.tuple, s.iss.raw())),
        _ => None,
    })
    .flatten() else {
        return fail!("expected SynSent state");
    };

    let syn_ack = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: 5000,
        ack_num: iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(tuple.remote_ip, tuple.local_ip, &syn_ack, &[], &[], 0);
    socket_notify_tcp_activity(&result);

    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::Established),
        "TCP should be Established after SYN-ACK"
    );

    // Socket state should now be Connected (via sync_socket_state or
    // notify path) — this is the state nc expects before send.
    let payload = b"Hello World\n";
    let n = socket_send(sock, &payload[..]);
    assert_test!(
        n > 0,
        "send after connect + 3WHS must succeed (was: broken pipe)"
    );
    assert_eq_test!(n as usize, payload.len(), "all bytes should be accepted");

    pass!()
}

/// Regression test: verify that send on a TCP socket that has not completed
/// the 3WHS returns an error (ENOTCONN), not EPIPE.
pub fn test_tcp_send_before_handshake_complete() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    assert_test!(sock >= 0, "socket_create succeeds");
    let sock = sock as u32;

    // Force non-blocking so connect returns immediately.
    socket_set_nonblocking(sock, true);

    // Connect sends SYN but returns immediately (non-blocking).
    let rc = socket_connect(sock, [10, 0, 0, 2], 80);
    // Non-blocking connect should return 0 (SYN sent) or EINPROGRESS.
    assert_test!(rc == 0 || rc == -115, "non-blocking connect");

    // Verify TCP is in SynSent (handshake NOT complete).
    let Some(tcp_id) = socket_lookup_tcp_idx(sock) else {
        return fail!("no tcp idx after connect");
    };
    assert_eq_test!(
        tcp::get_state(tcp_id),
        Some(TcpState::SynSent),
        "TCP should be SynSent"
    );

    // Try to send data BEFORE the 3WHS completes.
    let payload = b"Hello World\n";
    let n = socket_send(sock, &payload[..]);
    assert_test!(n < 0, "send before 3WHS completion must fail (ENOTCONN)");

    // Now complete the handshake.
    let Some((tuple, iss)) = tcp::with_pcb(tcp_id, |pcb| match &pcb.state {
        tcp::PcbState::SynSent(s) => Some((pcb.tuple, s.iss.raw())),
        _ => None,
    })
    .flatten() else {
        return fail!("expected SynSent state");
    };
    let syn_ack = TcpHeader {
        src_port: tuple.remote_port,
        dst_port: tuple.local_port,
        seq_num: 7000,
        ack_num: iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::input(tuple.remote_ip, tuple.local_ip, &syn_ack, &[], &[], 0);
    socket_notify_tcp_activity(&result);

    // After 3WHS, send should now succeed.
    let n2 = socket_send(sock, &payload[..]);
    assert_test!(n2 > 0, "send after 3WHS completion must succeed");

    pass!()
}

pub fn test_tcp_listen_accept_incoming_syn() -> TestResult {
    reset();
    let listen_sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED) as u32;
    assert_eq_test!(socket_bind(listen_sock, [10, 0, 0, 1], 80), 0);
    assert_eq_test!(socket_listen(listen_sock, 4), 0);

    socket_set_nonblocking(listen_sock, true);

    let syn_hdr = TcpHeader {
        src_port: 5000,
        dst_port: 80,
        seq_num: 1000,
        ack_num: 0,
        data_offset: 5,
        flags: TCP_FLAG_SYN,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let syn_result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &syn_hdr, &[], &[], 0);

    // The SYN-ACK was sent; get the ISS from the outbound segment.
    let syn_ack_seg = match syn_result.segments().next() {
        Some(seg) => seg.clone(),
        None => return fail!("no SYN-ACK segment after SYN"),
    };

    let ack_hdr = TcpHeader {
        src_port: 5000,
        dst_port: 80,
        seq_num: 1001,
        ack_num: syn_ack_seg.seq_num.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let ack_result = tcp::input([10, 0, 0, 2], [10, 0, 0, 1], &ack_hdr, &[], &[], 0);
    socket_notify_tcp_activity(&ack_result);

    let mut peer_addr = [0u8; 4];
    let mut peer_port = 0u16;
    let new_sock = socket_accept(
        listen_sock,
        &mut peer_addr as *mut _,
        &mut peer_port as *mut _,
    );
    assert_test!(new_sock >= 0, "accept should return a valid socket fd");
    assert_eq_test!(peer_addr, [10, 0, 0, 2]);
    assert_eq_test!(peer_port, 5000);

    assert_eq_test!(
        socket_get_state(new_sock as u32),
        Some(SocketState::Connected)
    );
    pass!()
}

slopos_testing::stest!(name = test_socket_create_tcp, suite = socket);
slopos_testing::stest!(name = test_socket_create_udp, suite = socket);
slopos_testing::stest!(name = test_socket_create_invalid_domain, suite = socket);
slopos_testing::stest!(name = test_socket_create_invalid_type, suite = socket);
slopos_testing::stest!(name = test_socket_table_full, suite = socket);
slopos_testing::stest!(name = test_socket_bind_valid, suite = socket);
slopos_testing::stest!(name = test_socket_bind_specific_addr, suite = socket);
slopos_testing::stest!(name = test_socket_bind_invalid_idx, suite = socket);
slopos_testing::stest!(name = test_socket_bind_already_bound, suite = socket);
slopos_testing::stest!(name = test_socket_listen_after_bind, suite = socket);
slopos_testing::stest!(name = test_socket_listen_without_bind, suite = socket);
slopos_testing::stest!(name = test_socket_listen_on_connected, suite = socket);
slopos_testing::stest!(
    name = test_socket_connect_creates_tcp_connection,
    suite = socket
);
slopos_testing::stest!(name = test_socket_connect_invalid_socket, suite = socket);
slopos_testing::stest!(name = test_socket_connect_already_connected, suite = socket);
slopos_testing::stest!(
    name = test_socket_send_returns_error_not_connected,
    suite = socket
);
slopos_testing::stest!(
    name = test_socket_recv_returns_error_not_connected,
    suite = socket
);
slopos_testing::stest!(name = test_socket_send_buffer_space, suite = socket);
slopos_testing::stest!(name = test_socket_recv_empty, suite = socket);
slopos_testing::stest!(name = test_socket_close_valid, suite = socket);
slopos_testing::stest!(name = test_socket_close_invalid, suite = socket);
slopos_testing::stest!(name = test_socket_close_frees_slot, suite = socket);
slopos_testing::stest!(
    name = test_socket_poll_readable_not_connected,
    suite = socket
);
slopos_testing::stest!(name = test_socket_poll_writable_connected, suite = socket);
slopos_testing::stest!(name = test_socket_state_after_create, suite = socket);
slopos_testing::stest!(name = test_socket_state_after_bind, suite = socket);
slopos_testing::stest!(name = test_socket_reset_all, suite = socket);
slopos_testing::stest!(
    name = test_socket_accept_no_pending_returns_eagain,
    suite = socket
);
slopos_testing::stest!(name = test_bounded_queue_push_pop_capacity, suite = socket);
slopos_testing::stest!(
    name = test_bounded_queue_overflow_returns_false,
    suite = socket
);
slopos_testing::stest!(name = test_bounded_queue_clear_and_resize, suite = socket);
slopos_testing::stest!(
    name = test_slab_socket_table_alloc_free_get_get_mut,
    suite = socket
);
slopos_testing::stest!(
    name = test_slab_socket_table_grows_and_enforces_max,
    suite = socket
);
slopos_testing::stest!(
    name = test_ephemeral_port_allocator_alloc_release_round_robin,
    suite = socket
);
slopos_testing::stest!(
    name = test_ephemeral_port_allocator_exhaustion_and_no_duplicates,
    suite = socket
);
slopos_testing::stest!(
    name = test_socket_options_defaults_and_validation,
    suite = socket
);
slopos_testing::stest!(name = test_socket_flags_set_clear_contains, suite = socket);
slopos_testing::stest!(name = test_socket_new_defaults_and_helpers, suite = socket);
slopos_testing::stest!(
    name = test_tcp_send_on_established_returns_bytes,
    suite = socket
);
slopos_testing::stest!(name = test_tcp_recv_after_peer_data, suite = socket);
slopos_testing::stest!(
    name = test_tcp_shutdown_wr_transitions_to_fin_wait1,
    suite = socket
);
slopos_testing::stest!(name = test_tcp_shutdown_wr_recv_still_works, suite = socket);
slopos_testing::stest!(name = test_tcp_send_after_shutdown_wr_fails, suite = socket);
slopos_testing::stest!(name = test_tcp_send_after_blocking_connect, suite = socket);
slopos_testing::stest!(
    name = test_tcp_send_before_handshake_complete,
    suite = socket
);
slopos_testing::stest!(name = test_tcp_listen_accept_incoming_syn, suite = socket);
