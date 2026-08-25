use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{ERRNO_EAGAIN, ERRNO_EINPROGRESS, POLLOUT};
use slopos_ostd::KBox;
use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::WaitQueue;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::napi::NapiContext;
use crate::socket;
use crate::tcp;
use crate::tests::env_wait::errno_i64;
use crate::tests::net_scope::NetTestScope;
use crate::tests::socket_tests;
use crate::tests::tcp_common::PEER_ISS;

/// Connect to the scope's sink and complete the handshake by injection: the
/// sink answers nothing, so the SYN+ACK has to be synthetic, and the peer is
/// TEST-NET-1 so no real reply can reach the resulting 4-tuple either.
fn connect_and_establish(scope: &NetTestScope) -> Result<(u32, tcp::ConnId), &'static str> {
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if sock < 0 {
        return Err("socket_create failed");
    }
    let sock = sock as u32;
    socket::socket_set_nonblocking(sock, true);

    let rc = socket::socket_connect(sock, scope.peer_ip(), scope.peer_port());
    if rc < 0 && rc != errno_i64(ERRNO_EINPROGRESS) as i32 {
        return Err("socket_connect failed");
    }

    let Some(tcp_id) = socket::socket_lookup_tcp_idx(sock) else {
        return Err("socket_lookup_tcp_idx failed");
    };
    if scope.inject_syn_ack(tcp_id, PEER_ISS).is_none() {
        return Err("no PCB in a handshake state for the synthetic SYN+ACK");
    }

    Ok((sock, tcp_id))
}

pub fn test_napi_budget_limiting() -> TestResult {
    let ctx = NapiContext::new(4);
    assert_test!(ctx.budget() == 4, "napi budget stored");
    assert_test!(ctx.processed() == 0, "napi processed starts at zero");
    ctx.add_processed(3);
    assert_test!(ctx.processed() == 3, "napi processed advances");
    ctx.add_processed(1);
    assert_test!(ctx.processed() == 4, "napi processed accumulates to budget");
    pass!()
}

/// Models the post-burst recheck where the IRQ races the kthread's re-park: an
/// unrearmed `wait` in that window would lose the wake-up.
pub fn test_napi_waker_rearm_short_circuits() -> TestResult {
    use crate::napi_waker::NapiWaker;
    static WAKER: NapiWaker = NapiWaker::new(
        "test-waker",
        lock_class!("test.napi_waker.waiters", LOCK_LEVEL_RESOURCE),
    );
    WAKER.rearm();
    assert_test!(
        WAKER.consume_edge_for_test(),
        "rearm must leave an edge to consume"
    );
    assert_test!(
        !WAKER.consume_edge_for_test(),
        "an armed edge is consumed exactly once"
    );
    // The IRQ path arms the same flag.
    WAKER.arm_and_wake();
    assert_test!(
        WAKER.consume_edge_for_test(),
        "arm_and_wake must leave an edge to consume"
    );
    pass!()
}

/// The submit enqueues and returns; it never waits for the device to complete
/// the descriptor. Asserted by depth rather than by a wall clock, which
/// measures the host: a submit that waited for its own completion could not
/// leave more than one frame outstanding, so `BURST` back-to-back submits
/// advancing `tx_packets` by `BURST` is the property, and it is the same
/// verdict on a machine of any speed.
pub fn test_tx_fire_and_forget() -> TestResult {
    const BURST: u64 = 8;

    let Some(driver) = crate::net_driver_service::net_driver() else {
        klog_info!("NAPI_TEST: SKIP - no net driver registered");
        return TestResult::Skipped;
    };
    if !(driver.virtio_net_is_ready)() {
        klog_info!("NAPI_TEST: SKIP - the net device is not ready");
        return TestResult::Skipped;
    }
    let Some(handle) = (driver.get_device_handle)() else {
        return fail!("a ready driver with no device handle");
    };

    // An empty frame is answered before the ring is touched, so it must not
    // move the counter and is the control for the burst below.
    let before_empty = handle.stats().tx_packets;
    assert_test!(
        (driver.virtio_net_transmit)(&[]),
        "an empty submit was refused"
    );
    assert_eq_test!(
        handle.stats().tx_packets,
        before_empty,
        "an empty submit reached the ring"
    );

    let before = handle.stats().tx_packets;
    for _ in 0..BURST {
        if !(driver.virtio_net_transmit)(&[0u8; 64]) {
            return fail!("the device refused a frame while ready");
        }
    }
    let advanced = handle.stats().tx_packets.wrapping_sub(before);
    assert_test!(
        advanced >= BURST,
        "{} submits advanced tx_packets by {} — the submit path is waiting on \
         its own completion",
        BURST,
        advanced
    );

    pass!()
}

pub fn test_waitqueue_basic() -> TestResult {
    static TEST_WQ: WaitQueue = WaitQueue::new(lock_class!("TEST_WQ.waiters", LOCK_LEVEL_RESOURCE));
    let before = TEST_WQ.generation();
    assert_test!(
        !TEST_WQ.wake_one(),
        "wake_one on empty wait queue returns false"
    );
    assert_test!(
        TEST_WQ.generation() == before,
        "generation unchanged on empty wake"
    );
    pass!()
}

pub fn test_blocking_recv() -> TestResult {
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let (sock, _tcp_id) = match connect_and_establish(&scope) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };
    let _ = socket::socket_set_nonblocking(sock, false);
    let _ = socket::socket_set_timeouts(sock, 1, 0);
    let mut buf = [0u8; 16];
    let rc = socket::socket_recv(sock, &mut buf);
    assert_test!(
        rc == errno_i64(ERRNO_EAGAIN),
        "blocking recv times out with eagain"
    );
    pass!()
}

pub fn test_blocking_accept() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED) as u32;
    let _ = socket::socket_bind(sock, [0, 0, 0, 0], 8080);
    let _ = socket::socket_listen(sock, 4);
    let _ = socket::socket_set_nonblocking(sock, false);
    let _ = socket::socket_set_timeouts(sock, 1, 0);
    let rc = socket::socket_accept(sock, core::ptr::null_mut(), core::ptr::null_mut());
    assert_test!(
        rc == errno_i64(ERRNO_EAGAIN) as i32,
        "blocking accept times out with eagain"
    );
    pass!()
}

pub fn test_socket_poll_flags() -> TestResult {
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let (sock, _) = match connect_and_establish(&scope) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };
    let writable = socket::socket_poll_writable(sock);
    assert_test!(
        (writable & POLLOUT as u32) != 0,
        "connected socket reports pollout"
    );
    pass!()
}

pub fn test_nonblocking_preserved() -> TestResult {
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let (sock, _) = match connect_and_establish(&scope) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };
    let _ = socket::socket_set_nonblocking(sock, true);
    let mut buf = [0u8; 32];
    let rc = socket::socket_recv(sock, &mut buf);
    assert_test!(
        rc == errno_i64(ERRNO_EAGAIN),
        "nonblocking recv returns eagain"
    );
    pass!()
}

pub fn test_recv_timeout() -> TestResult {
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let (sock, _) = match connect_and_establish(&scope) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };
    let _ = socket::socket_set_nonblocking(sock, false);
    let _ = socket::socket_set_timeouts(sock, 2, 0);
    let mut buf = [0u8; 8];
    let rc = socket::socket_recv(sock, &mut buf);
    assert_test!(rc == errno_i64(ERRNO_EAGAIN), "recv timeout expires");
    pass!()
}

pub fn test_send_backpressure() -> TestResult {
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let (sock, _) = match connect_and_establish(&scope) {
        Ok(v) => v,
        Err(e) => return fail!("{}", e),
    };
    let _ = socket::socket_set_nonblocking(sock, true);
    let mut payload: KBox<[u8; 20000]> = KBox::zeroed().expect("alloc");
    payload.iter_mut().for_each(|b| *b = 0x42);
    let first = socket::socket_send(sock, &payload[..]);
    assert_test!(first >= 0, "initial send makes forward progress");
    let second = socket::socket_send(sock, &payload[..]);
    assert_test!(
        second == errno_i64(ERRNO_EAGAIN) || second >= 0,
        "backpressure is surfaced"
    );
    pass!()
}

pub fn test_regression_existing() -> TestResult {
    match socket_tests::test_socket_create_tcp() {
        TestResult::Pass => pass!(),
        _ => TestResult::Fail,
    }
}

slopos_testing::stest!(name = test_napi_budget_limiting, suite = napi);
slopos_testing::stest!(name = test_napi_waker_rearm_short_circuits, suite = napi);
slopos_testing::stest!(name = test_tx_fire_and_forget, suite = napi);
slopos_testing::stest!(name = test_waitqueue_basic, suite = napi);
slopos_testing::stest!(name = test_blocking_recv, suite = napi);
slopos_testing::stest!(name = test_blocking_accept, suite = napi);
slopos_testing::stest!(name = test_socket_poll_flags, suite = napi);
slopos_testing::stest!(name = test_nonblocking_preserved, suite = napi);
slopos_testing::stest!(name = test_recv_timeout, suite = napi);
slopos_testing::stest!(name = test_send_backpressure, suite = napi);
slopos_testing::stest!(name = test_regression_existing, suite = napi);
