use slopos_abi::net::{AF_INET, SOCK_DGRAM};
use slopos_abi::syscall::{ERRNO_EAGAIN, POLLIN, POLLOUT};
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, fail, pass};

use crate::socket::*;

fn reset() {
    socket_reset_all();
}

fn errno_i64(errno: u64) -> i64 {
    errno as i64
}

pub fn test_udp_t1_receive_queue_push_pop_overflow() -> TestResult {
    reset();

    let mut q = UdpReceiveQueue::new();
    assert_test!(q.is_empty(), "queue starts empty");

    for i in 0..(UDP_RX_QUEUE_SIZE + 2) {
        let mut d = UdpDatagram::empty();
        d.src_ip = [10, 0, 0, 1];
        d.src_port = i as u16;
        d.len = 1;
        d.data[0] = i as u8;
        q.push(&d);
    }

    assert_eq_test!(q.len(), UDP_RX_QUEUE_SIZE, "overflow keeps fixed size");

    let first = match q.pop() {
        Some(v) => v,
        None => return fail!("expected first datagram"),
    };
    assert_eq_test!(first.src_port, 2, "oldest two entries were dropped");

    while q.pop().is_some() {}
    assert_test!(q.pop().is_none(), "empty pop returns None");
    pass!()
}

pub fn test_udp_t2_dispatch_delivery_and_unbound_drop() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 40000), 0);

    let payload = [0x11u8, 0x22, 0x33];
    socket_deliver_udp_from_dispatch([1, 1, 1, 1], [10, 0, 2, 15], 1234, 40000, &payload);

    let mut out = [0u8; 8];
    let mut src_ip = [0u8; 4];
    let mut src_port = 0u16;
    let got = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        &mut src_ip as *mut [u8; 4],
        &mut src_port as *mut u16,
    );
    assert_eq_test!(got, payload.len() as i64, "bound port receives datagram");
    assert_eq_test!(src_ip, [1, 1, 1, 1], "source ip propagated");
    assert_eq_test!(src_port, 1234, "source port propagated");

    socket_deliver_udp_from_dispatch([2, 2, 2, 2], [10, 0, 2, 15], 5555, 49999, &payload);
    let empty = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(
        empty,
        errno_i64(ERRNO_EAGAIN),
        "unbound destination is dropped"
    );

    pass!()
}

pub fn test_udp_t3_generic_udp_tx_no_crash() -> TestResult {
    reset();

    let payload = [1u8, 2, 3, 4];
    let ok = crate::net_driver_service::net_driver()
        .map(|d| (d.transmit_udp_packet)([10, 0, 2, 15], [8, 8, 8, 8], 50000, 53, &payload))
        .unwrap_or(false);
    assert_test!(ok || !ok, "transmit call returns without panic");

    pass!()
}

pub fn test_udp_t4_sendto_recvfrom_kernel_level() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;

    let payload = [9u8, 8, 7, 6];
    let sent = socket_sendto(sock, payload.as_ptr(), payload.len(), [1, 1, 1, 1], 9999);
    assert_test!(sent > 0, "sendto returns positive length");

    let mut out = [0u8; 16];
    let rc = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(rc, errno_i64(ERRNO_EAGAIN), "empty recvfrom returns EAGAIN");

    pass!()
}

pub fn test_udp_t5_connected_udp_send_and_peer_filter_recv() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;

    assert_eq_test!(socket_connect(sock, [7, 7, 7, 7], 7000), 0);

    let tx = [1u8, 2, 3];
    let sent = socket_send(sock, tx.as_ptr(), tx.len());
    assert_eq_test!(
        sent,
        tx.len() as i64,
        "connected UDP send uses default destination"
    );

    let bad = [0xBAu8, 0xD0];
    let good = [0xAAu8, 0x55, 0x11];
    socket_deliver_udp(sock, [9, 9, 9, 9], 9000, &bad);
    socket_deliver_udp(sock, [7, 7, 7, 7], 7000, &good);

    let mut out = [0u8; 8];
    let got = socket_recv(sock, out.as_mut_ptr(), out.len());
    assert_eq_test!(
        got,
        good.len() as i64,
        "recv drops non-peer datagram when connected"
    );
    assert_eq_test!(
        &out[..good.len()],
        &good,
        "recv payload matches peer packet"
    );

    pass!()
}

pub fn test_udp_t6_poll_readiness_for_udp() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;

    assert_eq_test!(socket_poll_readable(sock), 0, "initially not readable");
    assert_eq_test!(
        socket_poll_writable(sock),
        POLLOUT as u32,
        "udp always writable"
    );

    socket_deliver_udp(sock, [1, 2, 3, 4], 4040, &[0x44, 0x55]);
    assert_eq_test!(
        socket_poll_readable(sock) & POLLIN as u32,
        POLLIN as u32,
        "readable after enqueue"
    );

    let mut out = [0u8; 4];
    let _ = socket_recv(sock, out.as_mut_ptr(), out.len());
    assert_eq_test!(
        socket_poll_readable(sock) & POLLIN as u32,
        0,
        "not readable after dequeue"
    );

    pass!()
}

pub fn test_udp_t7_nonblocking_recvfrom_eagain() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;
    assert_eq_test!(socket_set_nonblocking(sock, true), 0);

    let mut out = [0u8; 4];
    let rc = socket_recvfrom(
        sock,
        out.as_mut_ptr(),
        out.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(
        rc,
        errno_i64(ERRNO_EAGAIN),
        "nonblocking recvfrom returns EAGAIN"
    );

    pass!()
}

pub fn test_udp_t8_sendto_auto_bind_ephemeral_port() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;

    let payload = [0x10u8, 0x20];
    let sent = socket_sendto(sock, payload.as_ptr(), payload.len(), [10, 0, 2, 1], 8080);
    assert_test!(sent > 0, "sendto succeeds");

    let snap = match socket_snapshot(sock) {
        Some(s) => s,
        None => return fail!("socket snapshot missing"),
    };
    assert_test!(snap.local_port != 0, "auto-bind assigned local port");

    pass!()
}

pub fn test_udp_t9_parse_udp_header_valid_invalid() -> TestResult {
    reset();

    let valid = [
        0x12, 0x34, 0x56, 0x78, 0x00, 0x0C, 0x00, 0x00, 0xAA, 0xBB, 0xCC, 0xDD,
    ];
    let parsed = match crate::udp::parse_udp_header(&valid) {
        Some(v) => v,
        None => return fail!("valid UDP header should parse"),
    };
    assert_eq_test!(parsed.0, 0x1234, "src port");
    assert_eq_test!(parsed.1, 0x5678, "dst port");
    assert_eq_test!(parsed.2, &[0xAA, 0xBB, 0xCC, 0xDD], "payload slice");

    let too_short = [0u8; 7];
    assert_test!(
        crate::udp::parse_udp_header(&too_short).is_none(),
        "short header rejected"
    );

    let bad_len = [0x00, 0x01, 0x00, 0x02, 0x00, 0x20, 0x00, 0x00, 0xAA];
    assert_test!(
        crate::udp::parse_udp_header(&bad_len).is_none(),
        "oversized UDP length rejected"
    );

    pass!()
}

pub fn test_udp_t10_reset_clears_udp_queues() -> TestResult {
    reset();

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0);
    if sock < 0 {
        return fail!("socket create failed");
    }
    let sock = sock as u32;

    socket_deliver_udp(sock, [4, 3, 2, 1], 3030, &[1, 2, 3]);
    assert_test!(
        socket_poll_readable(sock) & POLLIN as u32 != 0,
        "queue has data"
    );

    socket_reset_all();

    let fresh = socket_create(AF_INET, SOCK_DGRAM, 0);
    if fresh < 0 {
        return fail!("socket recreate failed");
    }
    let fresh = fresh as u32;
    assert_eq_test!(socket_poll_readable(fresh), 0, "queue is empty after reset");

    pass!()
}

slopos_lib::define_test_suite!(
    udp_socket,
    [
        test_udp_t1_receive_queue_push_pop_overflow,
        test_udp_t2_dispatch_delivery_and_unbound_drop,
        test_udp_t3_generic_udp_tx_no_crash,
        test_udp_t4_sendto_recvfrom_kernel_level,
        test_udp_t5_connected_udp_send_and_peer_filter_recv,
        test_udp_t6_poll_readiness_for_udp,
        test_udp_t7_nonblocking_recvfrom_eagain,
        test_udp_t8_sendto_auto_bind_ephemeral_port,
        test_udp_t9_parse_udp_header_valid_invalid,
        test_udp_t10_reset_clears_udp_queues,
    ]
);
