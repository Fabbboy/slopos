use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::neighbor::NEIGHBOR_CACHE;
use crate::netstack::NET_STACK;
use crate::route::ROUTE_TABLE;
use crate::socket;
use crate::tcp;
use crate::types::{DevIndex, Ipv4Addr};

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const GATEWAY_PORT: u16 = 7;

fn restore_boot_routes() {
    use crate::route::RouteEntry;
    ROUTE_TABLE.reset();
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr([127, 0, 0, 0]),
        prefix_len: 8,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(0),
        metric: 0,
    });
    if let Some(cfg) = NET_STACK.iface_for_dev(DevIndex(1)) {
        let mask_u32 = cfg.netmask.to_u32_be();
        let prefix_len = mask_u32.leading_ones() as u8;
        let prefix = Ipv4Addr::from_u32_be(cfg.ipv4_addr.to_u32_be() & mask_u32);
        ROUTE_TABLE.add(RouteEntry {
            prefix,
            prefix_len,
            gateway: Ipv4Addr::UNSPECIFIED,
            dev: DevIndex(1),
            metric: 0,
        });
        if !cfg.gateway.is_unspecified() {
            ROUTE_TABLE.add(RouteEntry {
                prefix: Ipv4Addr::UNSPECIFIED,
                prefix_len: 0,
                gateway: cfg.gateway,
                dev: DevIndex(1),
                metric: 100,
            });
        }
    }
}

fn test_route_table_has_default() -> TestResult {
    restore_boot_routes();
    let routes = ROUTE_TABLE.all_routes();
    klog_info!("tcp_live: route_count={}", routes.len());
    for r in &routes {
        klog_info!("tcp_live:   {:?}", r);
    }
    let has_default = routes.iter().any(|r| r.prefix_len == 0);
    assert_test!(has_default, "no default route in table");
    pass!()
}

fn test_netstack_has_ipv4() -> TestResult {
    let ip = NET_STACK.first_ipv4();
    klog_info!("tcp_live: our_ipv4={:?}", ip);
    assert_test!(ip.is_some(), "no IPv4 address configured");
    let addr = ip.unwrap().0;
    assert_test!(addr != [0; 4], "IPv4 address is 0.0.0.0");
    pass!()
}

fn test_arp_resolve_gateway() -> TestResult {
    let gw_ip = Ipv4Addr(GATEWAY_IP);

    let (dev, next_hop) = match ROUTE_TABLE.lookup(gw_ip) {
        Some(r) => r,
        None => return fail!("no route to gateway {}", gw_ip),
    };
    klog_info!(
        "tcp_live: route to {} -> dev={} next_hop={}",
        gw_ip,
        dev,
        next_hop
    );

    if NEIGHBOR_CACHE.lookup(dev, next_hop).is_some() {
        klog_info!("tcp_live: gateway MAC already cached");
        return pass!();
    }

    klog_info!("tcp_live: gateway MAC not cached, sending ARP on all devices");
    let dev_count = crate::netdev::DEVICE_REGISTRY.device_count();
    for i in 0..dev_count {
        crate::arp::send_request_via_registry(DevIndex(i), next_hop);
    }

    for attempt in 0..20u32 {
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.virtnet_force_napi_poll)();
        }
        for i in 0..dev_count {
            if NEIGHBOR_CACHE.lookup(DevIndex(i), next_hop).is_some() {
                klog_info!(
                    "tcp_live: ARP resolved on dev {} after {}ms",
                    i,
                    (attempt + 1) * 100
                );
                return pass!();
            }
        }
    }

    fail!("ARP for gateway {} did not resolve in 2s", gw_ip)
}

/// Regression test for the curl-times-out bug.
///
/// Before the fix, `socket_connect` (and `socket_send` for UDP/ICMP)
/// picked the local source IP via `NET_STACK.first_ipv4()`, which
/// returns the first iface in registration order. The loopback iface
/// is registered before any NIC, so external connections went out
/// with `src_ip = 127.0.0.1` — QEMU SLIRP's TCP forwarder ignores
/// replies to 127.0.0.1, surfacing as a 5 s recv timeout in curl
/// after the std-side `decode_error_kind` patch translated EAGAIN to
/// `ErrorKind::TimedOut`. After the fix, `source_ip_for(dst)` does
/// a route lookup and returns the egress device's IP — the NIC's
/// DHCP-assigned address for external traffic.
fn test_source_ip_for_external_uses_nic() -> TestResult {
    let external = Ipv4Addr(GATEWAY_IP);
    let src = match NET_STACK.source_ip_for(external) {
        Some(ip) => ip,
        None => return fail!("source_ip_for({}) returned None", external),
    };
    klog_info!(
        "tcp_live: source_ip_for({}) -> {} (must NOT be 127.x)",
        external,
        src
    );
    assert_test!(
        !src.is_loopback(),
        "source_ip_for(external dst) returned loopback {} — outbound TCP would send with src=127.0.0.1 and never receive replies",
        src
    );
    assert_test!(
        !src.is_unspecified(),
        "source_ip_for(external dst) returned 0.0.0.0 — DHCP did not configure the NIC"
    );
    pass!()
}

/// Loopback destinations must still resolve through the loopback
/// interface, not the NIC. Confirms `source_ip_for` is route-aware
/// rather than blanket-blacklisting loopback.
fn test_source_ip_for_loopback_uses_loopback() -> TestResult {
    let lo = Ipv4Addr([127, 0, 0, 1]);
    let src = match NET_STACK.source_ip_for(lo) {
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
    let our_ip = match NET_STACK.first_ipv4() {
        Some(ip) => ip.0,
        None => return fail!("no local IP"),
    };

    let (tcp_id, syn) = match tcp::connect(our_ip, GATEWAY_IP, GATEWAY_PORT) {
        Ok(r) => r,
        Err(e) => return fail!("tcp_connect failed: {:?}", e),
    };

    klog_info!(
        "tcp_live: SYN built id={} seq={} local_port={}",
        tcp_id.0,
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

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;
    let _ = socket::socket_set_nonblocking(sock_idx, true);

    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    klog_info!("tcp_live: nonblocking connect returned {}", rc);

    let _ = socket::socket_close(sock_idx);

    assert_test!(
        rc == 0 || rc == -115,
        "nonblocking connect: expected 0 or EINPROGRESS(-115), got {}",
        rc,
    );
    pass!()
}

slopos_testing::stest!(name = test_route_table_has_default, suite = tcp_live);
slopos_testing::stest!(name = test_netstack_has_ipv4, suite = tcp_live);
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
