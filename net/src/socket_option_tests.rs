use slopos_abi::net::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::*;
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, fail, pass};

use super::socket::*;

fn reset() {
    socket_reset_all();
}

pub fn test_so_reuseaddr_roundtrip() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    let mut buf = [0u8; 4];
    let rc = socket_getsockopt(sock_idx, SOL_SOCKET, SO_REUSEADDR, &mut buf);
    assert_eq_test!(rc, 4);
    assert_eq_test!(i32::from_ne_bytes(buf), 0, "default SO_REUSEADDR");

    let val: i32 = 1;
    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_REUSEADDR, &val.to_ne_bytes()),
        0
    );
    let rc = socket_getsockopt(sock_idx, SOL_SOCKET, SO_REUSEADDR, &mut buf);
    assert_eq_test!(rc, 4);
    assert_eq_test!(i32::from_ne_bytes(buf), 1, "SO_REUSEADDR after set");

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_socket_option_roundtrips() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    let rcvbuf: u32 = 8192;
    let sndbuf: u32 = 12288;
    let keepalive: i32 = 1;
    let rcvtimeo: u64 = 5000;
    let sndtimeo: u64 = 7000;
    let nodelay: i32 = 1;

    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_RCVBUF, &rcvbuf.to_ne_bytes()),
        0
    );
    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_SNDBUF, &sndbuf.to_ne_bytes()),
        0
    );
    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_KEEPALIVE, &keepalive.to_ne_bytes()),
        0
    );
    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_RCVTIMEO, &rcvtimeo.to_ne_bytes()),
        0
    );
    assert_eq_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_SNDTIMEO, &sndtimeo.to_ne_bytes()),
        0
    );
    assert_eq_test!(
        socket_setsockopt(sock_idx, IPPROTO_TCP, TCP_NODELAY, &nodelay.to_ne_bytes()),
        0
    );

    let mut i32buf = [0u8; 4];
    let mut u64buf = [0u8; 8];

    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_RCVBUF, &mut i32buf),
        4
    );
    assert_eq_test!(u32::from_ne_bytes(i32buf), rcvbuf);

    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_SNDBUF, &mut i32buf),
        4
    );
    assert_eq_test!(u32::from_ne_bytes(i32buf), sndbuf);

    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_KEEPALIVE, &mut i32buf),
        4
    );
    assert_eq_test!(i32::from_ne_bytes(i32buf), 1);

    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_RCVTIMEO, &mut u64buf),
        8
    );
    assert_eq_test!(u64::from_ne_bytes(u64buf), rcvtimeo);

    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_SNDTIMEO, &mut u64buf),
        8
    );
    assert_eq_test!(u64::from_ne_bytes(u64buf), sndtimeo);

    assert_eq_test!(
        socket_getsockopt(sock_idx, IPPROTO_TCP, TCP_NODELAY, &mut i32buf),
        4
    );
    assert_eq_test!(i32::from_ne_bytes(i32buf), 1);

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_so_rcvbuf_validation() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    let too_small: u32 = 100;
    let too_large: u32 = 500_000;
    assert_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_RCVBUF, &too_small.to_ne_bytes()) < 0,
        "SO_RCVBUF too small rejected"
    );
    assert_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, SO_RCVBUF, &too_large.to_ne_bytes()) < 0,
        "SO_RCVBUF too large rejected"
    );

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_so_error_clear_on_read() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    let mut buf = [0u8; 4];
    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_ERROR, &mut buf),
        4
    );
    assert_eq_test!(i32::from_ne_bytes(buf), 0);
    assert_eq_test!(
        socket_getsockopt(sock_idx, SOL_SOCKET, SO_ERROR, &mut buf),
        4
    );
    assert_eq_test!(i32::from_ne_bytes(buf), 0);

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_shutdown_read() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    assert_eq_test!(socket_bind(sock_idx, [0, 0, 0, 0], 55555), 0);
    assert_eq_test!(socket_shutdown(sock_idx, SHUT_RD), 0);

    let mut buf = [0u8; 64];
    let rc = socket_recvfrom(
        sock_idx,
        buf.as_mut_ptr(),
        buf.len(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    assert_eq_test!(rc, 0, "recv after SHUT_RD should return EOF");

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_shutdown_write() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    assert_eq_test!(socket_connect(sock_idx, [127, 0, 0, 1], 12345), 0);
    assert_eq_test!(socket_shutdown(sock_idx, SHUT_WR), 0);

    let data = b"hello";
    let rc = socket_send(sock_idx, data.as_ptr(), data.len());
    assert_test!(rc < 0, "send after SHUT_WR should fail");

    let _ = socket_close(sock_idx);
    pass!()
}

pub fn test_unknown_option_returns_einval() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    if idx < 0 {
        return fail!("socket_create failed");
    }
    let sock_idx = idx as u32;

    let val: i32 = 1;
    assert_test!(
        socket_setsockopt(sock_idx, SOL_SOCKET, 999, &val.to_ne_bytes()) < 0,
        "unknown option should fail"
    );

    let _ = socket_close(sock_idx);
    pass!()
}

slopos_lib::define_test_suite!(
    socket_option,
    [
        test_so_reuseaddr_roundtrip,
        test_socket_option_roundtrips,
        test_so_rcvbuf_validation,
        test_so_error_clear_on_read,
        test_shutdown_read,
        test_shutdown_write,
        test_unknown_option_returns_einval,
    ]
);
