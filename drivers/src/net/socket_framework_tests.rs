use alloc::vec::Vec;

use slopos_abi::net::{AF_INET, SOCK_DGRAM};
use slopos_abi::syscall::{ERRNO_EAGAIN, SHUT_RD, SO_RCVBUF, SO_REUSEADDR, SOL_SOCKET};
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, fail, pass};

use super::packetbuf::PacketBuf;
use super::socket::*;
use super::types::{Ipv4Addr, Port, SockAddr};

fn errno_i64(errno: u64) -> i64 {
    errno as i64
}

fn reset() {
    socket_reset_all();
}

pub fn test_slab_alloc_free_cycle() -> TestResult {
    reset();

    let mut sockets = Vec::new();
    for _ in 0..100 {
        let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
        if idx < 0 {
            return fail!("socket_create failed before reaching 100 allocations");
        }
        sockets.push(idx as u32);
    }
    assert_eq_test!(socket_count_active(), 100, "100 active after first wave");

    for sock in sockets.iter().take(50) {
        assert_eq_test!(socket_close(*sock), 0);
    }
    assert_eq_test!(socket_count_active(), 50, "50 active after closing half");

    for _ in 0..50 {
        let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
        assert_test!(idx >= 0, "re-allocation succeeds");
    }
    assert_eq_test!(socket_count_active(), 100, "count restored to 100");
    pass!()
}

pub fn test_ephemeral_port_exhaustion() -> TestResult {
    reset();

    let mut alloc = EPHEMERAL_PORTS.lock();
    let mut released = None;
    for i in 0..EphemeralPortAllocator::EPHEMERAL_PORT_COUNT {
        let Some(port) = alloc.alloc() else {
            return fail!("allocator exhausted too early at {}", i);
        };
        if released.is_none() {
            released = Some(port);
        }
    }

    assert_test!(alloc.alloc().is_none(), "allocator reports exhaustion");
    let Some(release_port) = released else {
        return fail!("no port captured for release test");
    };
    alloc.release(release_port);
    assert_test!(alloc.alloc().is_some(), "allocator works after release");
    pass!()
}

pub fn test_udp_demux_dispatch() -> TestResult {
    reset();

    let a = socket_create(AF_INET, SOCK_DGRAM, 0);
    let b = socket_create(AF_INET, SOCK_DGRAM, 0);
    if a < 0 || b < 0 {
        return fail!("socket_create failed");
    }
    let a = a as u32;
    let b = b as u32;

    assert_eq_test!(socket_set_nonblocking(a, true), 0);
    assert_eq_test!(socket_set_nonblocking(b, true), 0);
    assert_eq_test!(socket_bind(a, [10, 0, 2, 15], 41000), 0);
    assert_eq_test!(socket_bind(b, [10, 0, 2, 15], 42000), 0);

    socket_deliver_udp_from_dispatch([1, 1, 1, 1], [10, 0, 2, 15], 1111, 41000, &[0xAA]);
    socket_deliver_udp_from_dispatch([2, 2, 2, 2], [10, 0, 2, 15], 2222, 42000, &[0xBB, 0xCC]);

    let mut out_a = [0u8; 4];
    let mut src_a = [0u8; 4];
    let mut src_port_a = 0u16;
    let n_a = socket_recvfrom(
        a,
        out_a.as_mut_ptr(),
        out_a.len(),
        &mut src_a as *mut [u8; 4],
        &mut src_port_a as *mut u16,
    );
    assert_eq_test!(n_a, 1, "socket A got its datagram");
    assert_eq_test!(out_a[0], 0xAA);
    assert_eq_test!(src_a, [1, 1, 1, 1]);
    assert_eq_test!(src_port_a, 1111);

    let mut out_b = [0u8; 4];
    let n_b = socket_recvfrom(
        b,
        out_b.as_mut_ptr(),
        out_b.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(n_b, 2, "socket B got its datagram");
    assert_eq_test!(&out_b[..2], &[0xBB, 0xCC]);
    pass!()
}

pub fn test_inaddr_any_wildcard() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket_create failed");
    }
    let sock = sock as u32;

    assert_eq_test!(socket_set_nonblocking(sock, true), 0);
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 43000), 0);
    socket_deliver_udp_from_dispatch([9, 9, 9, 9], [10, 0, 2, 15], 3333, 43000, &[0x5A]);

    let mut out = [0u8; 2];
    let n = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(n, 1, "wildcard socket receives destination-matched packet");
    assert_eq_test!(out[0], 0x5A);
    pass!()
}

pub fn test_recv_queue_overflow() -> TestResult {
    reset();

    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(idx) = table.alloc(SocketInner::Udp(UdpSocketInner)) else {
        return fail!("slab alloc failed");
    };
    let Some(sock) = table.get_mut(idx) else {
        return fail!("allocated socket missing");
    };

    sock.recv_queue.resize(2);
    let p1 = PacketBuf::from_raw_copy(&[1])
        .ok_or(())
        .map_err(|_| TestResult::Fail)
        .ok();
    let p2 = PacketBuf::from_raw_copy(&[2])
        .ok_or(())
        .map_err(|_| TestResult::Fail)
        .ok();
    let p3 = PacketBuf::from_raw_copy(&[3])
        .ok_or(())
        .map_err(|_| TestResult::Fail)
        .ok();
    let Some(p1) = p1 else {
        return fail!("packet alloc failed");
    };
    let Some(p2) = p2 else {
        return fail!("packet alloc failed");
    };
    let Some(p3) = p3 else {
        return fail!("packet alloc failed");
    };

    let src = SockAddr::new(Ipv4Addr([1, 2, 3, 4]), Port(1234));
    assert_test!(sock.recv_queue.push((p1, src)), "first enqueue succeeds");
    assert_test!(sock.recv_queue.push((p2, src)), "second enqueue succeeds");
    assert_test!(
        !sock.recv_queue.push((p3, src)),
        "overflow enqueue returns false"
    );
    pass!()
}

pub fn test_so_reuseaddr() -> TestResult {
    reset();

    let a = socket_create(AF_INET, SOCK_DGRAM, 0);
    let b = socket_create(AF_INET, SOCK_DGRAM, 0);
    if a < 0 || b < 0 {
        return fail!("socket_create failed");
    }
    let a = a as u32;
    let b = b as u32;

    assert_eq_test!(socket_bind(a, [10, 0, 2, 15], 44000), 0);
    assert_test!(
        socket_bind(b, [10, 0, 2, 15], 44000) < 0,
        "bind fails without reuse"
    );

    let one: i32 = 1;
    assert_eq_test!(
        socket_setsockopt(b, SOL_SOCKET, SO_REUSEADDR, &one.to_ne_bytes()),
        0
    );
    assert_eq_test!(socket_bind(b, [10, 0, 2, 15], 44000), 0);
    pass!()
}

pub fn test_so_rcvbuf_resize() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket_create failed");
    }
    let sock = sock as u32;

    let size: u32 = 256;
    assert_eq_test!(
        socket_setsockopt(sock, SOL_SOCKET, SO_RCVBUF, &size.to_ne_bytes()),
        0
    );

    let mut out = [0u8; 4];
    let got = socket_getsockopt(sock, SOL_SOCKET, SO_RCVBUF, &mut out);
    assert_eq_test!(got, 4);
    assert_eq_test!(u32::from_ne_bytes(out), 256);

    let table = NEW_SOCKET_TABLE.lock();
    let Some(sock_ref) = table.get(sock as usize) else {
        return fail!("socket missing");
    };
    assert_eq_test!(sock_ref.recv_queue.capacity(), 256, "recv_queue resized");
    pass!()
}

pub fn test_shutdown_read_behavior() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket_create failed");
    }
    let sock = sock as u32;

    assert_eq_test!(socket_set_nonblocking(sock, true), 0);
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 45000), 0);
    assert_eq_test!(socket_shutdown(sock, SHUT_RD), 0);

    let mut out = [0u8; 8];
    let rc = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(rc, 0, "recvfrom after SHUT_RD returns EOF");

    let recv_rc = socket_recv(sock, out.as_mut_ptr(), out.len());
    assert_eq_test!(recv_rc, 0, "recv after SHUT_RD returns EOF for UDP");
    let eagain = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_test!(
        eagain == 0 || eagain == errno_i64(ERRNO_EAGAIN),
        "read side remains shut down"
    );
    pass!()
}

slopos_lib::define_test_suite!(
    socket_framework,
    [
        test_slab_alloc_free_cycle,
        test_ephemeral_port_exhaustion,
        test_udp_demux_dispatch,
        test_inaddr_any_wildcard,
        test_recv_queue_overflow,
        test_so_reuseaddr,
        test_so_rcvbuf_resize,
        test_shutdown_read_behavior,
    ]
);
