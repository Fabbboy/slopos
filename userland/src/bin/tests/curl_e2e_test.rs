#![feature(restricted_std)]

//! End-to-end TCP recv proof: sends a TCP-DNS query to a public resolver and
//! asserts a response byte arrives inside the 5-second budget curl uses.
//!
//! Target is 53/tcp on public resolvers: TCP-DNS is mandatory by RFC 7766 and
//! SLIRP NATs those addresses normally, unlike its own gateway and DNS-alias
//! IPs. Reachability is per address, so several are tried; every address
//! refusing is a failure, never a skip.

use slopos_userland as _;

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

/// Tried in order; the first that completes a handshake is the target.
const TEST_DST_IPS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(8, 8, 8, 8),
    Ipv4Addr::new(1, 1, 1, 1),
    Ipv4Addr::new(9, 9, 9, 9),
];
const TEST_DST_PORT: u16 = 53;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// 2-byte TCP-DNS length prefix + a minimal valid DNS query for `.`
/// (root, type=NS). The response is never parsed; resolver answers vary.
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

/// Connects to the first reachable target and names every refusal: one
/// filtered address and no route off the machine are different faults.
fn connect_any() -> Result<(SocketAddrV4, TcpStream, u128), String> {
    let mut refusals = String::new();
    for ip in TEST_DST_IPS {
        let addr = SocketAddrV4::new(ip, TEST_DST_PORT);
        // Blocking `connect` is the path curl takes; `connect_timeout` layers
        // std's nonblocking-poll plumbing on top and can mask a TCP-state
        // regression behind a poll bug.
        let connect_start = Instant::now();
        match TcpStream::connect(&addr) {
            Ok(stream) => return Ok((addr, stream, connect_start.elapsed().as_millis())),
            Err(err) => {
                if !refusals.is_empty() {
                    refusals.push_str("; ");
                }
                refusals.push_str(&format!(
                    "{} in {} ms kind={:?} raw={:?}",
                    addr,
                    connect_start.elapsed().as_millis(),
                    err.kind(),
                    err.raw_os_error(),
                ));
            }
        }
    }
    Err(format!("connect to every target failed: {refusals}"))
}

fn run_tcp_recv() -> (bool, String) {
    let (addr, mut stream, connect_ms) = match connect_any() {
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

    (
        true,
        format!(
            "peer={} connect={} ms send={} ms recv={} ms n={} first_byte=0x{:02x}",
            addr, connect_ms, send_ms, recv_ms, n, buf[0],
        ),
    )
}

fn run_tcp_recv_diag() -> (bool, String) {
    let (ok, diag) = run_tcp_recv();
    if !ok {
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

/// A loopback `local_addr` on an outbound socket means `first_ipv4()` has
/// leaked into `socket_connect` in place of route-aware source selection.
fn run_local_addr_is_not_loopback() -> (bool, String) {
    let (_addr, stream, _connect_ms) = match connect_any() {
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
