#![feature(restricted_std)]

//! Regression test for `curl: receive failed` on SlopOS.
//!
//! `scripts/patch_std.sh` installs a dedicated `slopos.rs` io-error decoder
//! (from `slibc/std_pal/io_error/slopos.rs`); std's generic decoder maps every
//! errno to `ErrorKind::Uncategorized`, which curl reports as a receive
//! failure. These cases assert the kind mapping, so an accidental fallback to
//! the generic decoder or a missed errno in the table fails CI.

use slopos_userland as _;

use std::fs::File;
use std::io::{Error, ErrorKind};
use std::time::{Duration, Instant};

const ENOENT: i32 = 2;
const EINTR: i32 = 4;

/// `NotFound`, not the `Uncategorized` the generic decoder would give.
fn errno_kind_decoded_as_not_found() -> bool {
    let path = "/nonexistent_curl_recv_repro_target";
    match File::open(path) {
        Ok(_) => {
            eprintln!("curl_recv_repro: unexpected open() success for {path}");
            false
        }
        Err(err) => {
            let kind = err.kind();
            let raw = err.raw_os_error();
            eprintln!(
                "curl_recv_repro: open({path}) -> kind={:?} raw={:?}",
                kind, raw,
            );
            if raw != Some(ENOENT) {
                eprintln!("curl_recv_repro: kernel did not return ENOENT");
                return false;
            }
            kind == ErrorKind::NotFound
        }
    }
}

/// The recv-side twin of the case above.
fn recv_on_unconnected_socket_is_not_connected() -> bool {
    use slopos_userland::syscall::net as syscall_net;

    const AF_INET: u16 = 2;
    const SOCK_STREAM: u16 = 1;

    let fd_owned = match syscall_net::socket(AF_INET, SOCK_STREAM, 0) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("curl_recv_repro: socket() failed: errno={}", err.errno());
            return false;
        }
    };
    let raw_fd: i32 = fd_owned.raw();

    let mut buf = [0u8; 32];
    let rc = syscall_net::recv(raw_fd, &mut buf, 0);
    drop(fd_owned);

    let (kind, raw) = match rc {
        Ok(_) => {
            eprintln!("curl_recv_repro: unexpected recv() success on unconnected socket");
            return false;
        }
        Err(err) => {
            // Reconstructed the way `std::sys::pal::slopos::cvt` does.
            let io_err = Error::from_raw_os_error(err.errno());
            (io_err.kind(), io_err.raw_os_error())
        }
    };
    eprintln!(
        "curl_recv_repro: recv-on-unconnected -> kind={:?} raw={:?}",
        kind, raw,
    );
    kind == ErrorKind::NotConnected
}

extern "C" fn sigint_noop(_signum: i32) {}

fn main() {
    // `test_harness::run` reports only pass/fail with no payload, which makes
    // a kind mismatch opaque under `tests.verbosity=summary`.
    use slopos_slibc::test_harness::{TestStatus, report};
    let mut failed: u32 = 0;
    let cases: &[(&str, fn() -> (bool, String))] = &[
        (
            "errno_kind_decoded_as_not_found",
            errno_kind_decoded_as_not_found_diag,
        ),
        (
            "recv_on_unconnected_socket_is_not_connected",
            recv_on_unconnected_socket_is_not_connected_diag,
        ),
        (
            "recv_returns_eintr_on_pending_signal",
            recv_returns_eintr_on_pending_signal_diag,
        ),
    ];
    for (name, f) in cases {
        let (ok, diag) = f();
        let status = if ok {
            TestStatus::Pass
        } else {
            TestStatus::Fail
        };
        report(status, name, &diag);
        if !ok {
            failed = failed.saturating_add(1);
        }
    }
    let exit_code = failed.min(255) as i32;
    slopos_userland::syscall::core::exit_with_code(exit_code)
}

fn errno_kind_decoded_as_not_found_diag() -> (bool, String) {
    let ok = errno_kind_decoded_as_not_found();
    let err = File::open("/nonexistent_curl_recv_repro_target").err();
    let diag = match err {
        Some(e) => format!("kind={:?} raw={:?}", e.kind(), e.raw_os_error()),
        None => "(no error captured)".to_string(),
    };
    (ok, diag)
}

fn recv_on_unconnected_socket_is_not_connected_diag() -> (bool, String) {
    let ok = recv_on_unconnected_socket_is_not_connected();
    (ok, "see report".to_string())
}

fn recv_returns_eintr_on_pending_signal_diag() -> (bool, String) {
    let (ok, diag) = recv_returns_eintr_on_pending_signal_with_diag();
    (ok, diag)
}

fn recv_returns_eintr_on_pending_signal_with_diag() -> (bool, String) {
    use slopos_userland::syscall::net as syscall_net;
    use slopos_userland::syscall::process;

    if process::set_signal_handler(slopos_abi::signal::SIGINT, sigint_noop) != 0 {
        return (false, "set_signal_handler failed".to_string());
    }

    const AF_INET: u16 = 2;
    const SOCK_DGRAM: u16 = 2;

    let fd_owned = match syscall_net::socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(fd) => fd,
        Err(err) => return (false, format!("udp socket errno={}", err.errno())),
    };
    let raw_fd: i32 = fd_owned.raw();
    if syscall_net::bind_any(raw_fd, 0).is_err() {
        return (false, "udp bind_any failed".to_string());
    }

    let tv = slopos_abi::syscall::Timeval::from_millis(1500);
    let mut tv_bytes = [0u8; core::mem::size_of::<slopos_abi::syscall::Timeval>()];
    if !tv.to_bytes(&mut tv_bytes) {
        return (false, "timeval serialise failed".to_string());
    }
    if let Err(err) = syscall_net::setsockopt(
        raw_fd,
        slopos_abi::syscall::SOL_SOCKET,
        slopos_abi::syscall::SO_RCVTIMEO,
        &tv_bytes,
    ) {
        return (false, format!("setsockopt errno={}", err.errno()));
    }

    // 150 ms leaves the main thread firmly parked in `socket_recv`'s wait
    // queue, so a wait that ignores the signal burns the full 1500 ms.
    let pid = process::getpid() as i32;
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let _ = process::kill_pid(pid, slopos_abi::signal::SIGINT);
    });

    let start = Instant::now();
    let mut buf = [0u8; 64];
    let res = syscall_net::recvfrom(raw_fd, &mut buf, 0, None);
    let elapsed = start.elapsed();
    drop(fd_owned);
    let _ = watchdog.join();

    let raw = res.as_ref().err().map(|e| e.errno());
    let diag = format!("elapsed_ms={} raw_errno={:?}", elapsed.as_millis(), raw);
    let ok = raw == Some(EINTR) && elapsed < Duration::from_millis(750);
    (ok, diag)
}
