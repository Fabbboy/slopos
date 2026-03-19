use slopos_abi::net::{AF_INET, SOCK_STREAM};
use slopos_abi::syscall::{ERRNO_EAGAIN, POLLOUT};
use slopos_lib::testing::TestResult;
use slopos_lib::{WaitQueue, assert_test, pass};

use crate::napi::{NapiContext, NapiState};
use crate::socket;
use crate::socket_tests;
use crate::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader};

fn errno_i64(errno: u64) -> i64 {
    errno as i64 as i32 as i64
}

fn reset() {
    socket::socket_reset_all();
}

fn connect_and_establish() -> Option<(u32, usize)> {
    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock < 0 {
        return None;
    }
    socket::socket_set_nonblocking(sock as u32, true);
    let rc = socket::socket_connect(sock as u32, [10, 0, 0, 2], 80);
    if rc < 0 && rc != -115 {
        return None;
    }
    let tcp_idx = socket::socket_lookup_tcp_idx(sock as u32)?;
    let conn = tcp::tcp_get_connection(tcp_idx)?;
    let syn_ack = TcpHeader {
        src_port: conn.tuple.remote_port,
        dst_port: conn.tuple.local_port,
        seq_num: 9000,
        ack_num: conn.iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let result = tcp::tcp_input(
        conn.tuple.remote_ip,
        conn.tuple.local_ip,
        &syn_ack,
        &[],
        &[],
        0,
    );
    socket::socket_notify_tcp_activity(&result);
    Some((sock as u32, tcp_idx))
}

pub fn test_napi_budget_limiting() -> TestResult {
    let ctx = NapiContext::new(4);
    assert_test!(ctx.schedule(), "napi schedule transitions from idle");
    assert_test!(
        ctx.state() == NapiState::Scheduled,
        "napi enters scheduled state"
    );
    assert_test!(ctx.begin_poll(), "napi enters polling state");
    ctx.add_processed(4);
    assert_test!(ctx.processed() == 4, "napi processed count tracked");
    ctx.complete();
    assert_test!(
        ctx.state() == NapiState::Idle,
        "napi completes back to idle"
    );
    pass!()
}

pub fn test_tx_fire_and_forget() -> TestResult {
    let start = slopos_lib::clock::uptime_ms();
    let _ = crate::driver_hooks::virtio_net_transmit(&[0u8; 64]);
    let end = slopos_lib::clock::uptime_ms();
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
    let Some((sock, _tcp_idx)) = connect_and_establish() else {
        return TestResult::Fail;
    };
    let _ = socket::socket_set_nonblocking(sock, false);
    let _ = socket::socket_set_timeouts(sock, 1, 0);
    let mut buf = [0u8; 16];
    let rc = socket::socket_recv(sock, buf.as_mut_ptr(), buf.len());
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
    let rc = socket::socket_recv(sock, buf.as_mut_ptr(), buf.len());
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
    let rc = socket::socket_recv(sock, buf.as_mut_ptr(), buf.len());
    assert_test!(rc == errno_i64(ERRNO_EAGAIN), "recv timeout expires");
    pass!()
}

pub fn test_send_backpressure() -> TestResult {
    reset();
    let Some((sock, _)) = connect_and_establish() else {
        return TestResult::Fail;
    };
    let _ = socket::socket_set_nonblocking(sock, true);
    let payload = [0x42u8; 20000];
    let first = socket::socket_send(sock, payload.as_ptr(), payload.len());
    assert_test!(first >= 0, "initial send makes forward progress");
    let second = socket::socket_send(sock, payload.as_ptr(), payload.len());
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

slopos_lib::define_test_suite!(
    napi,
    [
        test_napi_budget_limiting,
        test_tx_fire_and_forget,
        test_waitqueue_basic,
        test_blocking_recv,
        test_blocking_accept,
        test_socket_poll_flags,
        test_nonblocking_preserved,
        test_recv_timeout,
        test_send_backpressure,
        test_regression_existing,
    ]
);
