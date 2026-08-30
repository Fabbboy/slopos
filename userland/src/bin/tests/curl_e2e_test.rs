#![feature(restricted_std)]

//! End-to-end TCP proof: opens a connection to a peer off this machine, sends a
//! frame and asserts the same bytes come back inside the budget curl uses.
//!
//! The peer is QEMU's in-network echo responder (`slopos_userland::net::ECHO_PEER_ADDR`,
//! a `guestfwd` wired up by `scripts/qemu_run.sh`), not a public address. It is
//! reached over `eth0` through the ordinary route, ARP and source-selection
//! paths, so every kernel property here is exercised exactly as a public
//! destination would exercise it — while a host with no egress can still answer
//! the question. A failure is therefore about SlopOS, which is the only reason
//! this test is worth running.
//!
//! The reply is compared byte-for-byte against what was sent, so a truncated,
//! duplicated or reordered delivery fails.

use slopos_userland as _;

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use slopos_userland::net::{ECHO_PEER_ADDR, ECHO_PEER_PORT};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The peer is one emulated hop away with no internet in the path, so a
/// handshake that has not completed by now is a fault rather than slowness.
/// Deliberately far below the harness's 120 s silence budget: the 30 s the
/// kernel's blocking connect would otherwise spend is most of that budget for
/// a result already known.
const CONNECT_BUDGET: Duration = Duration::from_secs(2);

/// Distinctive and not a round length: a stack that returns a zeroed or
/// half-filled buffer fails the comparison rather than matching by accident.
const ECHO_PAYLOAD: &[u8] = b"slopos-echo-0123456789-abcdefghij";

fn peer_addr() -> SocketAddrV4 {
    SocketAddrV4::new(
        Ipv4Addr::new(
            ECHO_PEER_ADDR[0],
            ECHO_PEER_ADDR[1],
            ECHO_PEER_ADDR[2],
            ECHO_PEER_ADDR[3],
        ),
        ECHO_PEER_PORT,
    )
}

/// Connect to the echo peer, naming the elapsed time on both paths.
///
/// The connect is announced before it blocks: output on the wire is what resets
/// the harness's silence watchdog, and a failing connect is exactly the case
/// with nothing else to say.
///
/// Blocking `connect` on purpose — it is the path curl takes, and
/// `connect_timeout` layers std's nonblocking-poll plumbing on top, which can
/// hide a TCP-state regression behind a poll bug. The budget is enforced by
/// checking the elapsed time after the fact rather than by arming a timer, so
/// what is measured is the connect itself.
fn connect_peer() -> Result<(SocketAddrV4, TcpStream, u128), String> {
    let addr = peer_addr();
    println!("curl_e2e: connecting to {addr}");
    let started = Instant::now();
    match TcpStream::connect(&addr) {
        Ok(stream) => {
            let elapsed = started.elapsed();
            if elapsed > CONNECT_BUDGET {
                return Err(format!(
                    "connected to {} but took {} ms, over the {} ms budget for a peer one \
                     emulated hop away",
                    addr,
                    elapsed.as_millis(),
                    CONNECT_BUDGET.as_millis(),
                ));
            }
            Ok((addr, stream, elapsed.as_millis()))
        }
        Err(err) => Err(format!(
            "connect to {} failed after {} ms: kind={:?} raw={:?}",
            addr,
            started.elapsed().as_millis(),
            err.kind(),
            err.raw_os_error(),
        )),
    }
}

/// Read exactly `want` bytes, or say how many arrived before the stream stopped.
///
/// A single `read` returning fewer bytes is legal TCP, so looping is what makes
/// "the reply is complete" a real assertion rather than a property of how the
/// segments happened to land.
fn read_exact_echo(stream: &mut TcpStream, want: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(want);
    let mut chunk = [0u8; 256];
    while out.len() < want {
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(format!(
                    "peer closed after {} of {} byte(s)",
                    out.len(),
                    want
                ));
            }
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(err) => {
                return Err(format!(
                    "read failed with {} of {} byte(s) received: kind={:?} raw={:?}",
                    out.len(),
                    want,
                    err.kind(),
                    err.raw_os_error(),
                ));
            }
        }
    }
    Ok(out)
}

fn run_tcp_echo() -> (bool, String) {
    let (addr, mut stream, connect_ms) = match connect_peer() {
        Ok(v) => v,
        Err(diag) => return (false, diag),
    };

    if let Err(err) = stream.set_read_timeout(Some(IO_TIMEOUT)) {
        return (false, format!("set_read_timeout failed: {err:?}"));
    }
    if let Err(err) = stream.set_write_timeout(Some(IO_TIMEOUT)) {
        return (false, format!("set_write_timeout failed: {err:?}"));
    }

    let send_start = Instant::now();
    if let Err(err) = stream.write_all(ECHO_PAYLOAD) {
        return (
            false,
            format!(
                "write_all failed after {} ms: kind={:?} raw={:?}",
                send_start.elapsed().as_millis(),
                err.kind(),
                err.raw_os_error(),
            ),
        );
    }
    let send_ms = send_start.elapsed().as_millis();

    let recv_start = Instant::now();
    let echoed = read_exact_echo(&mut stream, ECHO_PAYLOAD.len());
    let recv_ms = recv_start.elapsed().as_millis();

    let echoed = match echoed {
        Ok(bytes) => bytes,
        Err(diag) => {
            return (
                false,
                format!("{diag} (connect={connect_ms} ms send={send_ms} ms recv={recv_ms} ms)"),
            );
        }
    };

    if echoed != ECHO_PAYLOAD {
        return (
            false,
            format!(
                "echo mismatch: sent {} byte(s), got {} byte(s) that differ — the bytes \
                 reached the recv buffer corrupted or out of order. sent={:?} got={:?}",
                ECHO_PAYLOAD.len(),
                echoed.len(),
                ECHO_PAYLOAD,
                echoed.as_slice(),
            ),
        );
    }

    (
        true,
        format!(
            "peer={} connect={} ms send={} ms recv={} ms echoed={} byte(s) verbatim",
            addr,
            connect_ms,
            send_ms,
            recv_ms,
            echoed.len(),
        ),
    )
}

fn run_tcp_echo_diag() -> (bool, String) {
    let (ok, diag) = run_tcp_echo();
    if !ok {
        let hint = if diag.contains("connect to") {
            " — SYN/SYN-ACK path broken (src_ip routing, ARP, or RX demux)"
        } else if diag.contains("write_all failed") {
            " — TX data path broken (TCP send buffer / poll_transmit)"
        } else if diag.contains("read failed") {
            " — RX data path broken (peer responded but kernel never enqueued into recv buffer)"
        } else if diag.contains("peer closed") {
            " — peer reset/FIN before echoing (often src_ip still bogus)"
        } else if diag.contains("echo mismatch") {
            " — bytes arrived corrupted: reassembly, sequence handling or buffer copy"
        } else {
            ""
        };
        (false, format!("{diag}{hint}"))
    } else {
        (ok, diag)
    }
}

/// A loopback `local_addr` on an outbound socket means `first_ipv4()` has
/// leaked into `socket_connect` in place of route-aware source selection.
///
/// The peer being on-link rather than past the gateway does not weaken this:
/// loopback registers as `DevIndex(0)`, ahead of the NIC, so a `first_ipv4()`
/// regression still yields `127.0.0.1` for any destination whose route names
/// `eth0`. Source selection for a destination reached *via the default route*
/// is pinned separately, in the kernel phase, by
/// `net::tests::tcp_live::test_source_ip_for_external_uses_nic`.
fn run_local_addr_is_not_loopback() -> (bool, String) {
    let (_addr, stream, _connect_ms) = match connect_peer() {
        Ok(v) => v,
        Err(diag) => return (false, format!("{diag} — cannot probe local_addr")),
    };

    let local = match stream.local_addr() {
        Ok(la) => la,
        Err(err) => return (false, format!("local_addr failed: {err:?}")),
    };
    let ip = match local {
        std::net::SocketAddr::V4(v4) => *v4.ip(),
        std::net::SocketAddr::V6(_) => {
            return (false, "got IPv6 local_addr; expected V4".to_string());
        }
    };
    if ip.is_loopback() {
        return (
            false,
            format!(
                "outbound socket's local_addr is {} — `first_ipv4()` regression has crept back in. \
                 source_ip_for(off-box) must return the NIC's DHCP-assigned address.",
                ip
            ),
        );
    }
    if ip.is_unspecified() {
        return (
            false,
            "outbound socket's local_addr is 0.0.0.0 — kernel never assigned a source IP"
                .to_string(),
        );
    }
    (true, format!("local_addr.ip = {} (non-loopback, OK)", ip))
}

fn main() {
    use slopos_slibc::test_harness::{TestStatus, report};

    let mut failed: u32 = 0;
    let cases: &[(&str, fn() -> (bool, String))] = &[
        (
            "tcp_local_addr_is_not_loopback",
            run_local_addr_is_not_loopback,
        ),
        ("tcp_recv_returns_data", run_tcp_echo_diag),
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

// Avoid unused-import warning when no helpers below need it.
#[allow(dead_code)]
fn _unused_kind() -> ErrorKind {
    ErrorKind::Other
}
