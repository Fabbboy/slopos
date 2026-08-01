use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{ERRNO_EAGAIN, POLLOUT};
use slopos_ostd::KBox;
use slopos_ostd::sync::WaitQueue;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, pass};

use crate::napi::NapiContext;
use crate::socket;
use crate::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader};
use crate::tests::socket_tests;

fn errno_i64(errno: u64) -> i64 {
    errno as i64 as i32 as i64
}

fn reset() {
    socket::socket_reset_all();
}

fn connect_and_establish() -> Option<(u32, tcp::ConnId)> {
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock < 0 {
        return None;
    }
    socket::socket_set_nonblocking(sock as u32, true);
    let rc = socket::socket_connect(sock as u32, [10, 0, 0, 2], 80);
    if rc < 0 && rc != -115 {
        return None;
    }
    let tcp_id = socket::socket_lookup_tcp_idx(sock as u32)?;
    let (tuple, iss) = tcp::with_pcb(tcp_id, |pcb| {
        let iss = match &pcb.state {
            tcp::PcbState::SynSent(s) => s.iss.raw(),
            tcp::PcbState::Data(d) => d.iss.raw(),
            _ => return None,
        };
        Some((pcb.tuple, iss))
    })
    .flatten()?;
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
    socket::socket_notify_tcp_activity(&result);
    Some((sock as u32, tcp_id))
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

/// Phase-2 regression: `NapiWaker::rearm` makes the next `wait`
/// short-circuit without parking. Models the post-burst recheck
/// where the IRQ races the kthread's re-park; an unrearmed `wait`
/// in that window would lose the wake-up.
pub fn test_napi_waker_rearm_short_circuits() -> TestResult {
    use crate::napi_waker::NapiWaker;
    static WAKER: NapiWaker = NapiWaker::new("test-waker");
    WAKER.rearm();
    // The park predicate consumes the armed flag, so an edge left by `rearm`
    // is what makes the next park return without blocking.
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

pub fn test_tx_fire_and_forget() -> TestResult {
    let start = slopos_kernel_services::clock::uptime_ms();
    let _ = crate::net_driver_service::net_driver()
        .map(|d| (d.virtio_net_transmit)(&[0u8; 64]))
        .unwrap_or(false);
    let end = slopos_kernel_services::clock::uptime_ms();
    assert_test!(
        end.saturating_sub(start) < 1000,
        "tx submit returns without long blocking"
    );
    pass!()
}

pub fn test_waitqueue_basic() -> TestResult {
    static TEST_WQ: WaitQueue = WaitQueue::new();
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
    reset();
    let Some((sock, _tcp_id)) = connect_and_establish() else {
        return TestResult::Fail;
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
    reset();
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0) as u32;
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
    reset();
    let Some((sock, _)) = connect_and_establish() else {
        return TestResult::Fail;
    };
    let writable = socket::socket_poll_writable(sock);
    assert_test!(
        (writable & POLLOUT as u32) != 0,
        "connected socket reports pollout"
    );
    pass!()
}

pub fn test_nonblocking_preserved() -> TestResult {
    reset();
    let Some((sock, _)) = connect_and_establish() else {
        return TestResult::Fail;
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
    reset();
    let Some((sock, _)) = connect_and_establish() else {
        return TestResult::Fail;
    };
    let _ = socket::socket_set_nonblocking(sock, false);
    let _ = socket::socket_set_timeouts(sock, 2, 0);
    let mut buf = [0u8; 8];
    let rc = socket::socket_recv(sock, &mut buf);
    assert_test!(rc == errno_i64(ERRNO_EAGAIN), "recv timeout expires");
    pass!()
}

pub fn test_send_backpressure() -> TestResult {
    reset();
    let Some((sock, _)) = connect_and_establish() else {
        return TestResult::Fail;
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
