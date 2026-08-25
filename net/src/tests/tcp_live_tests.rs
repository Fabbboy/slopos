//! Tests that exercise the live NIC, its DHCP lease and the QEMU SLIRP peer.
//!
//! Nothing here writes to a shared table. The route table, the interface table
//! and the neighbour cache are what the live stack is using while these run, so
//! a test that installed its own topology would be asserting against a stack it
//! had just reconfigured out from under the boot lease.

use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::iface::{self, IfaceKind};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::DEVICE_REGISTRY;
use crate::route::{ROUTE_TABLE, RouteEntry};
use crate::socket;
use crate::tcp;
use crate::tests::env_wait::await_env;
use crate::tests::env_wait::errno_i32;
use crate::types::{DevIndex, Ipv4Addr};

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const GATEWAY_PORT: u16 = 7;

/// Failsafe on a wait whose other end is the environment rather than the
/// kernel. Not a budget for the exchange it covers — a SLIRP round trip is
/// sub-millisecond — only a bound on a wait that would otherwise not end.
const ENV_FAILSAFE_MS: u64 = 2_000;

/// How long each pass leaves the peer alone before draining the NIC again.
const POLL_INTERVAL_MS: u32 = 1;

fn nic_dev() -> Option<DevIndex> {
    let mut found = None;
    iface::for_each(|i| {
        if found.is_none() && i.kind == IfaceKind::Ethernet {
            found = Some(i.dev);
        }
    });
    found
}

/// Wait for the boot DHCP client to put an address on `dev`.
///
/// The client's state reaches `Bound` under its own lock and the lease is
/// applied after that lock is dropped, so the address — not the state — is what
/// says the interface is configured.
fn await_dhcp_addr(dev: DevIndex) -> Option<(Ipv4Addr, u64)> {
    await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        iface::our_ip(dev).filter(|ip| !ip.is_unspecified())
    })
}

fn default_route_on(dev: DevIndex) -> Option<RouteEntry> {
    let routes = ROUTE_TABLE.all_routes();
    routes
        .iter()
        .copied()
        .find(|r| r.prefix_len == 0 && r.dev == dev)
}

fn test_route_table_has_default() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };

    let Some((route, waited)) =
        await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || default_route_on(dev))
    else {
        return fail!(
            "dev {} has no default route after {}ms (DHCP state {:?}) — the environment's DHCP server did not answer",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };
    klog_info!("tcp_live: default route {:?} after {}ms", route, waited);

    assert_test!(
        !route.gateway.is_unspecified(),
        "the default route on dev {} is directly connected, so nothing is reachable off-link: {:?}",
        dev,
        route
    );
    pass!()
}

fn test_iface_has_ipv4() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };

    let Some((addr, waited)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — the environment's DHCP server did not answer",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };
    klog_info!(
        "tcp_live: our_ipv4={} on dev {} after {}ms",
        addr,
        dev,
        waited
    );

    assert_test!(
        !addr.is_loopback(),
        "the Ethernet interface took loopback address {}",
        addr
    );
    assert_eq_test!(
        iface::first_ipv4(),
        Some(addr),
        "first_ipv4 disagrees with the NIC's own address"
    );
    pass!()
}

fn test_arp_resolve_gateway() -> TestResult {
    let gw_ip = Ipv4Addr(GATEWAY_IP);

    let Some((dev, next_hop)) = ROUTE_TABLE.lookup(gw_ip) else {
        return fail!("no route to gateway {}", gw_ip);
    };
    klog_info!(
        "tcp_live: route to {} -> dev={} next_hop={}",
        gw_ip,
        dev,
        next_hop
    );

    if let Some(mac) = NEIGHBOR_CACHE.lookup(dev, next_hop) {
        klog_info!("tcp_live: {} already cached as {}", next_hop, mac);
        return pass!();
    }

    let Some(before) = DEVICE_REGISTRY.stats_by_index(dev) else {
        return fail!(
            "the route names dev {}, which the device registry does not hold",
            dev
        );
    };
    crate::arp::send_request_via_registry(dev, next_hop);
    let after = DEVICE_REGISTRY.stats_by_index(dev).unwrap_or(before);

    // The half of this test that does not depend on a peer. Other CPUs transmit
    // on this device too, so the counter is a floor rather than a count: it can
    // only fail when the request never reached the wire at all.
    assert_test!(
        after.tx_packets > before.tx_packets,
        "dev {} transmitted nothing for an ARP request for {}",
        dev,
        next_hop
    );

    let Some((mac, waited)) = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        NEIGHBOR_CACHE.lookup(dev, next_hop)
    }) else {
        return fail!(
            "{} did not answer ARP on dev {} within {}ms — the environment's gateway is not responding",
            next_hop,
            dev,
            ENV_FAILSAFE_MS
        );
    };
    klog_info!(
        "tcp_live: {} resolved to {} after {}ms",
        next_hop,
        mac,
        waited
    );
    pass!()
}

/// An external destination must take its source IP from the NIC.
///
/// `first_ipv4()` returns registration order and loopback registers before any
/// NIC, so sourcing that way sends external traffic with `src_ip = 127.0.0.1`,
/// whose replies QEMU SLIRP's TCP forwarder drops.
fn test_source_ip_for_external_uses_nic() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    let Some((nic_addr, _)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — source selection has nothing to pick",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };

    let external = Ipv4Addr(GATEWAY_IP);
    let Some(src) = iface::source_ip_for(external) else {
        return fail!("source_ip_for({}) returned None", external);
    };
    klog_info!("tcp_live: source_ip_for({}) -> {}", external, src);

    assert_eq_test!(
        src,
        nic_addr,
        "source_ip_for(external dst) must pick the NIC's address; outbound TCP sourced from anywhere else never receives replies"
    );
    pass!()
}

/// Loopback destinations must still resolve through the loopback interface:
/// `source_ip_for` is route-aware, not blanket-blacklisting loopback.
fn test_source_ip_for_loopback_uses_loopback() -> TestResult {
    let lo = Ipv4Addr([127, 0, 0, 1]);
    let src = match iface::source_ip_for(lo) {
        Some(ip) => ip,
        None => return fail!("source_ip_for(127.0.0.1) returned None"),
    };
    klog_info!("tcp_live: source_ip_for({}) -> {}", lo, src);
    assert_test!(
        src.is_loopback(),
        "source_ip_for(loopback dst) returned {} — loopback traffic should use 127.0.0.0/8 source",
        src
    );
    pass!()
}

fn test_tcp_syn_transmit() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    let Some((our_ip, _)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — a SYN has no source address",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };

    let (tcp_id, syn) = match tcp::connect(our_ip.0, GATEWAY_IP, GATEWAY_PORT) {
        Ok(r) => r,
        Err(e) => return fail!("tcp_connect failed: {:?}", e),
    };

    klog_info!(
        "tcp_live: SYN built id={} seq={} local_port={}",
        tcp_id,
        syn.seq_num,
        syn.tuple.local_port,
    );

    let send_rc = socket::socket_send_tcp_segment(&syn, &[]);
    klog_info!("tcp_live: send_tcp_segment returned {}", send_rc);

    let _ = tcp::abort(tcp_id);

    assert_test!(send_rc == 0, "send_tcp_segment failed with {}", send_rc);
    pass!()
}

fn test_tcp_nonblocking_connect_returns_einprogress() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};
    use slopos_abi::syscall::ERRNO_EINPROGRESS;

    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    if await_dhcp_addr(dev).is_none() {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — connect has no source address",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    }

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;
    let _ = socket::socket_set_nonblocking(sock_idx, true);

    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    klog_info!("tcp_live: nonblocking connect returned {}", rc);

    let _ = socket::socket_close(sock_idx);

    assert_test!(
        rc == 0 || rc == errno_i32(ERRNO_EINPROGRESS),
        "nonblocking connect: expected 0 or EINPROGRESS, got {}",
        rc,
    );
    pass!()
}

slopos_testing::stest!(name = test_route_table_has_default, suite = tcp_live);
slopos_testing::stest!(name = test_iface_has_ipv4, suite = tcp_live);
slopos_testing::stest!(name = test_arp_resolve_gateway, suite = tcp_live);
slopos_testing::stest!(
    name = test_source_ip_for_external_uses_nic,
    suite = tcp_live
);
slopos_testing::stest!(
    name = test_source_ip_for_loopback_uses_loopback,
    suite = tcp_live
);
slopos_testing::stest!(name = test_tcp_syn_transmit, suite = tcp_live);
slopos_testing::stest!(
    name = test_tcp_nonblocking_connect_returns_einprogress,
    suite = tcp_live
);
