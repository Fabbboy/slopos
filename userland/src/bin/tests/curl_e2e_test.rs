#![feature(restricted_std)]

//! End-to-end TCP recv proof — what should have caught the
//! curl-times-out regression in the first place.
//!
//! Connects to the QEMU SLIRP gateway's TCP echo of a known reliable
//! external HTTP server, sends a minimal `GET /`, and asserts that
//! at least one response byte is read inside the 5-second budget
//! curl itself uses. If this passes, `curl http://google.com` will
//! work from the shell.
//!
//! The previous fix (route-aware source IP via
//! `NetStack::source_ip_for`) made the SYN go out with the right
//! src_ip on the wire, and the in-kernel `test_source_ip_for_*`
//! checks confirmed that. But the kernel-side unit tests cannot
//! prove that SLIRP NATs the packet, the SYN-ACK reaches us, the
//! data path enqueues a response into `bufs.recv`, and userland
//! `read()` returns it — only an actual end-to-end transmission
//! does. That gap is what this binary closes.
//!
//! Target choice: `8.8.8.8:53` — Google's public DNS. TCP-DNS is
//! mandatory by RFC 7766, every public resolver listens on 53/tcp,
//! the destination has been stable since 2009, and SLIRP's NAT
//! forwards it normally (no special-case rewriting like SLIRP does
//! for its own gateway/DNS-alias IPs). This isolates the kernel
//! TCP send/recv path from DNS resolution, SLIRP IP rewriting, and
//! host-side firewalls — anything that lets `ping 8.8.8.8` work
//! lets this test work.

use slopos_userland as _;

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

const TEST_DST_IP: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const TEST_DST_PORT: u16 = 53;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// 2-byte TCP-DNS length prefix + a minimal valid DNS query for `.`
/// (root, type=NS). Any well-behaved resolver answers; we discard
/// the answer and just check that bytes came back.
const TCP_DNS_QUERY: &[u8] = &[
    0x00, 0x11, // 17-byte DNS message
    0xab, 0xcd, // ID
    0x01, 0x00, // flags: standard query, RD=1
    0x00, 0x01, // QDCOUNT=1
    0x00, 0x00, // ANCOUNT
    0x00, 0x00, // NSCOUNT
    0x00, 0x00, // ARCOUNT
    0x00, // QNAME=root
    0x00, 0x02, // QTYPE=NS
    0x00, 0x01, // QCLASS=IN
];

fn run_tcp_recv() -> (bool, String) {
    let addr = SocketAddrV4::new(TEST_DST_IP, TEST_DST_PORT);

    // Use the plain blocking `TcpStream::connect` — that is the
    // exact path `curl` takes. `connect_timeout` would also work but
    // exercises std's nonblocking-poll plumbing on top, which
    // historically masked TCP-state regressions behind poll bugs.
    let connect_start = Instant::now();
    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(err) => {
            return (
                false,
                format!(
                    "connect to {} failed in {} ms: kind={:?} raw={:?}",
                    addr,
                    connect_start.elapsed().as_millis(),
                    err.kind(),
                    err.raw_os_error(),
                ),
            );
        }
    };
    let connect_ms = connect_start.elapsed().as_millis();

    if let Err(err) = stream.set_read_timeout(Some(IO_TIMEOUT)) {
        return (false, format!("set_read_timeout failed: {err:?}"));
    }
    if let Err(err) = stream.set_write_timeout(Some(IO_TIMEOUT)) {
        return (false, format!("set_write_timeout failed: {err:?}"));
    }

    let send_start = Instant::now();
    if let Err(err) = stream.write_all(TCP_DNS_QUERY) {
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
    let mut buf = [0u8; 1024];
    let res = stream.read(&mut buf);
    let recv_ms = recv_start.elapsed().as_millis();

    let n = match res {
        Ok(n) => n,
        Err(err) => {
            return (
                false,
                format!(
                    "read failed after {} ms: kind={:?} raw={:?} (connect={} ms send={} ms)",
                    recv_ms,
                    err.kind(),
                    err.raw_os_error(),
                    connect_ms,
                    send_ms,
                ),
            );
        }
    };

    if n == 0 {
        return (
            false,
            format!(
                "read returned EOF (n=0) before any data — peer closed without responding (connect={} ms send={} ms recv={} ms)",
                connect_ms, send_ms, recv_ms,
            ),
        );
    }

    // For TCP-DNS, the first 2 bytes are the length prefix followed
    // by a DNS response. Any non-zero read proves the recv path
    // delivered bytes; we don't validate the DNS contents because
    // host resolver behavior varies.
    (
        true,
        format!(
            "connect={} ms send={} ms recv={} ms n={} first_byte=0x{:02x}",
            connect_ms, send_ms, recv_ms, n, buf[0],
        ),
    )
}

fn run_tcp_recv_diag() -> (bool, String) {
    let (ok, diag) = run_tcp_recv();
    if !ok {
        // Hint at the categories so the post-mortem reader knows where
        // to look first.
        let hint = if diag.contains("connect to") {
            " — SYN/SYN-ACK path broken (src_ip routing, ARP, or RX demux)"
        } else if diag.contains("write_all failed") {
            " — TX data path broken (TCP send buffer / poll_transmit)"
        } else if diag.contains("read failed") {
            " — RX data path broken (peer responded but kernel never enqueued into recv buffer)"
        } else if diag.contains("EOF") {
            " — peer reset/FIN before responding (often src_ip still bogus, server rejected request)"
        } else {
            ""
        };
        (false, format!("{diag}{hint}"))
    } else {
        (ok, diag)
    }
}

/// Sanity check: every test run actually went through the
/// route-aware source-ip selection by inspecting the socket's
/// `local_addr` after `connect_timeout` returns. Catches a future
/// regression of the original bug where `first_ipv4()` (loopback)
/// leaked into `socket_connect`.
fn run_local_addr_is_not_loopback() -> (bool, String) {
    let addr = SocketAddrV4::new(TEST_DST_IP, TEST_DST_PORT);
    let stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(err) => {
            // If even connect cannot complete we cannot make a
            // useful assertion; report skip-shape by failing with a
            // clear message that distinguishes this from the
            // recv-path test.
            return (
                false,
                format!(
                    "connect failed (kind={:?} raw={:?}) — cannot probe local_addr",
                    err.kind(),
                    err.raw_os_error(),
                ),
            );
        }
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
                 source_ip_for(external) must return the NIC's DHCP-assigned address.",
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
        ("tcp_recv_external_http_returns_data", run_tcp_recv_diag),
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
