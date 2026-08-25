//! ICMP socket tests, including echo exchanges with the QEMU SLIRP gateway.
//!
//! The end-to-end cases read the live route table rather than installing one:
//! the boot DHCP lease is the only thing that should author the topology the
//! live stack forwards on, and a test that wrote its own would leave whatever
//! it invented behind for every later test.

use slopos_abi::net::{AF_INET, IPPROTO_ICMP, SOCK_DGRAM};
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::icmp::{self, ICMP_HEADER_LEN};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::route::ROUTE_TABLE;
use crate::socket;
use crate::tests::env_wait::{await_env, pump_rx};
use crate::types::{DevIndex, Ipv4Addr, Port, SockAddr};

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Failsafe on a wait whose other end is the environment rather than the
/// kernel. Not a budget for the exchange it covers — a SLIRP round trip is
/// sub-millisecond — only a bound on a wait that would otherwise not end.
const ENV_FAILSAFE_MS: u64 = 3_000;

/// How long each pass leaves the peer alone before draining the NIC again.
const POLL_INTERVAL_MS: u32 = 1;

/// Drain RX for `ms`. For a reply that lands in a branch with no observable
/// effect there is no condition to wait on, so the window is the whole test.
fn drain_rx_for(ms: u64) {
    let start = slopos_kernel_services::clock::uptime_ms();
    while slopos_kernel_services::clock::uptime_ms().saturating_sub(start) < ms {
        pump_rx();
        slopos_kernel_services::platform::timer_poll_delay_ms(POLL_INTERVAL_MS);
    }
}

const REPLY_DRAIN_MS: u64 = 100;

/// The device and next hop the live route table picks for the gateway, once
/// DHCP has installed a route to it.
fn await_gateway_route() -> Option<((DevIndex, Ipv4Addr), u64)> {
    await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP))
    })
}

/// Closes the socket on the early return `assert_*!` takes, which a trailing
/// `socket_close` would skip: a bound identifier left in the ICMP demux catches
/// a later test's replies.
struct SocketGuard(u32);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = socket::socket_close(self.0);
    }
}

fn test_icmp_socket_create() -> TestResult {
    socket::socket_reset_all();
    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
    assert_test!(fd >= 0, "ICMP socket create failed: {}", fd);
    let _guard = SocketGuard(fd as u32);
    pass!()
}

fn test_icmp_socket_bind() -> TestResult {
    socket::socket_reset_all();
    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
    assert_test!(fd >= 0, "create failed: {}", fd);
    let sock = fd as u32;
    let _guard = SocketGuard(sock);

    let rc = socket::socket_bind(sock, [0, 0, 0, 0], 0x7070);
    assert_eq_test!(rc, 0, "bind failed");

    let demux_hit = icmp::ICMP_DEMUX.lock().lookup(0x7070);
    assert_test!(demux_hit.is_some(), "identifier not in ICMP_DEMUX");
    assert_eq_test!(demux_hit.unwrap(), sock, "demux points to wrong socket");
    pass!()
}

fn test_icmp_send_echo_raw() -> TestResult {
    let Some(((dev, next_hop), _)) = await_gateway_route() else {
        return fail!(
            "no route to {} after {}ms — DHCP did not configure the NIC",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS
        );
    };
    klog_info!(
        "icmp_test: gateway route -> dev={} next_hop={}",
        dev,
        next_hop
    );

    let payload = [0xAA; 8];
    match icmp::send_echo_request(GATEWAY_IP, 0xBEEF, 1, &payload) {
        Ok(n) => assert_eq_test!(n, payload.len(), "wrong byte count"),
        Err(e) => return fail!("send_echo_request failed: {:?}", e),
    }
    pass!()
}

fn test_icmp_ping_gateway_e2e() -> TestResult {
    socket::socket_reset_all();

    let Some(((dev, next_hop), _)) = await_gateway_route() else {
        return fail!(
            "no route to {} after {}ms — DHCP did not configure the NIC",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS
        );
    };
    klog_info!(
        "icmp_test: gateway route -> dev={} next_hop={}",
        dev,
        next_hop
    );

    let identifier: u16 = 0xCAFE;
    let sequence: u16 = 42;
    if let Err(e) = icmp::send_echo_request(GATEWAY_IP, identifier, sequence, &[0x53; 32]) {
        return fail!("send_echo_request failed: {:?}", e);
    }

    // No socket is bound for this identifier, so the reply exercises the
    // unmatched-reply drop branch in `icmp::handle_rx` and leaves nothing to
    // observe.
    drain_rx_for(REPLY_DRAIN_MS);
    pass!()
}

fn test_icmp_socket_sendto_recvfrom_e2e() -> TestResult {
    socket::socket_reset_all();

    let Some(((dev, next_hop), _)) = await_gateway_route() else {
        return fail!(
            "no route to {} after {}ms — DHCP did not configure the NIC",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS
        );
    };

    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
    assert_test!(fd >= 0, "socket create failed: {}", fd);
    let sock = fd as u32;
    let _guard = SocketGuard(sock);

    let identifier: u16 = 0xD00D;
    assert_eq_test!(
        socket::socket_bind(sock, [0, 0, 0, 0], identifier),
        0,
        "bind failed"
    );
    socket::socket_set_nonblocking(sock, true);

    let sequence: u16 = 7;
    let mut icmp_buf = [0u8; ICMP_HEADER_LEN + 32];
    icmp_buf[0] = 8;
    icmp_buf[4..6].copy_from_slice(&identifier.to_be_bytes());
    icmp_buf[6..8].copy_from_slice(&sequence.to_be_bytes());
    for byte in icmp_buf[ICMP_HEADER_LEN..].iter_mut() {
        *byte = 0x53;
    }

    let sent = socket::socket_sendto(sock, &icmp_buf, GATEWAY_IP, 0);
    assert_eq_test!(
        sent,
        icmp_buf.len() as i64,
        "sendto did not accept the whole datagram"
    );

    let mut recv_buf = [0u8; 256];
    let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
    let received = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        if socket::socket_poll_readable(sock) == 0 {
            return None;
        }
        let n = socket::socket_recvfrom(sock, &mut recv_buf, Some(&mut peer));
        (n > 0).then_some(n)
    });

    let Some((n, waited)) = received else {
        return fail!(
            "no ICMP echo reply from {} within {}ms (gateway {} on dev {} resolved to {:?}) — the environment did not answer",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS,
            next_hop,
            dev,
            NEIGHBOR_CACHE.lookup(dev, next_hop)
        );
    };
    klog_info!(
        "icmp_test: reply {} bytes from {} after {}ms",
        n,
        peer.ip,
        waited
    );

    assert_test!(
        n >= ICMP_HEADER_LEN as i64,
        "reply shorter than an ICMP header: {} bytes",
        n
    );
    assert_eq_test!(recv_buf[0], 0u8, "expected echo reply (type 0)");
    assert_eq_test!(
        u16::from_be_bytes([recv_buf[4], recv_buf[5]]),
        identifier,
        "identifier mismatch"
    );
    assert_eq_test!(
        u16::from_be_bytes([recv_buf[6], recv_buf[7]]),
        sequence,
        "sequence mismatch"
    );
    pass!()
}

/// The NAPI burst must drain the virtio used ring on explicit invocation and
/// feed the result through the ICMP demux.
fn test_icmp_napi_scheduling_e2e() -> TestResult {
    socket::socket_reset_all();

    let Some(((dev, next_hop), _)) = await_gateway_route() else {
        return fail!(
            "no route to {} after {}ms — DHCP did not configure the NIC",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS
        );
    };

    let Some(driver) = crate::net_driver_service::net_driver() else {
        return fail!("no NIC driver is registered");
    };

    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
    assert_test!(fd >= 0, "socket create failed: {}", fd);
    let sock = fd as u32;
    let _guard = SocketGuard(sock);

    let identifier: u16 = 0xBEEF;
    assert_eq_test!(
        socket::socket_bind(sock, [0, 0, 0, 0], identifier),
        0,
        "bind failed"
    );
    socket::socket_set_nonblocking(sock, true);

    let sequence: u16 = 99;
    let mut icmp_buf = [0u8; ICMP_HEADER_LEN + 32];
    icmp_buf[0] = 8;
    icmp_buf[4..6].copy_from_slice(&identifier.to_be_bytes());
    icmp_buf[6..8].copy_from_slice(&sequence.to_be_bytes());
    for byte in icmp_buf[ICMP_HEADER_LEN..].iter_mut() {
        *byte = 0x42;
    }

    let sent = socket::socket_sendto(sock, &icmp_buf, GATEWAY_IP, 0);
    assert_eq_test!(
        sent,
        icmp_buf.len() as i64,
        "sendto did not accept the whole datagram"
    );

    let mut recv_buf = [0u8; 256];
    let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
    let received = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        (driver.virtnet_force_napi_poll)();
        if socket::socket_poll_readable(sock) == 0 {
            return None;
        }
        let n = socket::socket_recvfrom(sock, &mut recv_buf, Some(&mut peer));
        (n > 0).then_some(n)
    });

    let Some((n, waited)) = received else {
        return fail!(
            "the NAPI burst produced no ICMP echo reply from {} within {}ms (gateway {} on dev {} resolved to {:?}) — the environment did not answer",
            Ipv4Addr(GATEWAY_IP),
            ENV_FAILSAFE_MS,
            next_hop,
            dev,
            NEIGHBOR_CACHE.lookup(dev, next_hop)
        );
    };
    klog_info!(
        "icmp_napi: reply {} bytes from {} after {}ms",
        n,
        peer.ip,
        waited
    );

    assert_test!(
        n >= ICMP_HEADER_LEN as i64,
        "reply shorter than an ICMP header: {} bytes",
        n
    );
    assert_eq_test!(recv_buf[0], 0u8, "expected echo reply (type 0)");
    assert_eq_test!(
        u16::from_be_bytes([recv_buf[4], recv_buf[5]]),
        identifier,
        "identifier mismatch"
    );
    pass!()
}

slopos_testing::stest!(name = test_icmp_socket_create, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_bind, suite = icmp);
slopos_testing::stest!(name = test_icmp_send_echo_raw, suite = icmp);
slopos_testing::stest!(name = test_icmp_ping_gateway_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_sendto_recvfrom_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_napi_scheduling_e2e, suite = icmp);
