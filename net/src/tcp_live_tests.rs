use slopos_lib::testing::TestResult;
use slopos_lib::{assert_test, fail, klog_info, pass};

use super::neighbor::NEIGHBOR_CACHE;
use super::route::ROUTE_TABLE;
use super::socket;
use super::tcp;
use super::types::Ipv4Addr;
use crate::netstack::NET_STACK;

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const GATEWAY_PORT: u16 = 7;

fn test_route_table_has_default() -> TestResult {
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

    let cached = NEIGHBOR_CACHE.lookup(dev, next_hop);
    klog_info!("tcp_live: neighbor cache for {}: {:?}", next_hop, cached);

    if cached.is_some() {
        klog_info!("tcp_live: gateway MAC already cached");
        return pass!();
    }

    klog_info!("tcp_live: gateway MAC not cached, sending ARP request");
    super::arp::send_request_via_registry(dev, next_hop);

    for attempt in 0..20u32 {
        slopos_lib::kernel_services::driver_runtime::sleep_current_task_ms(100);
        crate::driver_hooks::virtnet_force_napi_poll();
        let cached = NEIGHBOR_CACHE.lookup(dev, next_hop);
        if cached.is_some() {
            klog_info!("tcp_live: ARP resolved after {}ms", (attempt + 1) * 100);
            return pass!();
        }
    }

    fail!("ARP for gateway {} did not resolve in 2s", gw_ip)
}

fn test_tcp_syn_transmit() -> TestResult {
    let our_ip = match NET_STACK.first_ipv4() {
        Some(ip) => ip.0,
        None => return fail!("no local IP"),
    };

    let (tcp_idx, syn) = match tcp::tcp_connect(our_ip, GATEWAY_IP, GATEWAY_PORT) {
        Ok(r) => r,
        Err(e) => return fail!("tcp_connect failed: {:?}", e),
    };

    klog_info!(
        "tcp_live: SYN built idx={} seq={} local_port={}",
        tcp_idx,
        syn.seq_num,
        syn.tuple.local_port,
    );

    let send_rc = socket::socket_send_tcp_segment(&syn, &[]);
    klog_info!("tcp_live: send_tcp_segment returned {}", send_rc);

    let _ = tcp::tcp_abort(tcp_idx);

    assert_test!(send_rc == 0, "send_tcp_segment failed with {}", send_rc);
    pass!()
}

fn test_tcp_connect_does_not_hang() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;
    klog_info!("tcp_live: created socket idx={}", sock_idx);

    let _ = socket::socket_set_nonblocking(sock_idx, true);

    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    klog_info!("tcp_live: nonblocking connect returned {}", rc);

    assert_test!(
        rc == 0 || rc == -115,
        "nonblocking connect returned unexpected {}",
        rc,
    );

    for attempt in 0..30u32 {
        slopos_lib::kernel_services::driver_runtime::sleep_current_task_ms(100);
        crate::driver_hooks::virtnet_force_napi_poll();

        let readable = socket::socket_poll_readable(sock_idx);
        let writable = socket::socket_poll_writable(sock_idx);
        if readable != 0 || writable != 0 {
            klog_info!(
                "tcp_live: socket ready after {}ms (readable={} writable={})",
                (attempt + 1) * 100,
                readable,
                writable,
            );
            let _ = socket::socket_close(sock_idx);
            return pass!();
        }
    }

    let _ = socket::socket_close(sock_idx);
    fail!("tcp connect did not complete in 3s (stuck in SYN_SENT?)")
}

fn test_tcp_blocking_connect() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;

    let start = slopos_lib::clock::uptime_ms();
    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    let elapsed = slopos_lib::clock::uptime_ms().wrapping_sub(start);
    klog_info!(
        "tcp_live: blocking connect returned {} after {}ms",
        rc,
        elapsed
    );

    let start = slopos_lib::clock::uptime_ms();
    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    let elapsed = slopos_lib::clock::uptime_ms().wrapping_sub(start);
    klog_info!(
        "tcp_live: blocking connect returned {} after {}ms",
        rc,
        elapsed
    );

    if rc != 0 && rc != -111 && rc != -104 {
        let tcp_state = {
            let table = super::socket::NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get(sock_idx as usize) {
                if let super::socket::SocketInner::Tcp(ref tcp) = sock.inner {
                    tcp.conn_id.and_then(|id| tcp::tcp_get_state(id as usize))
                } else {
                    None
                }
            } else {
                None
            }
        };
        klog_info!(
            "tcp_live: tcp state={:?} (connect failed with {})",
            tcp_state,
            rc
        );
    }

    let _ = socket::socket_close(sock_idx);

    assert_test!(
        rc == 0 || rc == -111 || rc == -104,
        "blocking connect returned {} after {}ms (expected 0/-111/-104)",
        rc,
        elapsed,
    );
    pass!()
}

fn test_tcp_http_get_e2e() -> TestResult {
    use crate::dns;
    use slopos_abi::net::{AF_INET, SOCK_STREAM};

    socket::socket_reset_all();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("tcp_live: no route, skipping HTTP e2e");
        return pass!();
    }

    let target_ip = match dns::dns_resolve(b"google.com") {
        Some(ip) => {
            klog_info!(
                "tcp_live: HTTP target {}.{}.{}.{} (google.com)",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
            ip
        }
        None => {
            klog_info!("tcp_live: DNS failed, using gateway for HTTP target");
            GATEWAY_IP
        }
    };

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;

    let connect_rc = socket::socket_connect(sock_idx, target_ip, 80);
    if connect_rc != 0 {
        let _ = socket::socket_close(sock_idx);
        return fail!("socket_connect failed: {}", connect_rc);
    }

    let request = b"GET / HTTP/1.1\r\nHost: google.com\r\nConnection: close\r\n\r\n";
    let sent = socket::socket_send(sock_idx, request.as_ptr(), request.len());
    if sent < 0 {
        let _ = socket::socket_close(sock_idx);
        return fail!("socket_send failed: {}", sent);
    }
    assert_test!(
        sent as usize == request.len(),
        "partial HTTP request send: {}/{}",
        sent,
        request.len()
    );

    let _ = socket::socket_set_nonblocking(sock_idx, true);

    let mut response_prefix = [0u8; 32];
    let mut prefix_len = 0usize;
    let eagain = slopos_abi::syscall::ERRNO_EAGAIN as i64;

    for _attempt in 0..50u32 {
        slopos_lib::kernel_services::driver_runtime::sleep_current_task_ms(100);
        crate::driver_hooks::virtnet_force_napi_poll();

        let readable = socket::socket_poll_readable(sock_idx);
        if readable == 0 {
            continue;
        }

        let mut buf = [0u8; 512];
        let n = socket::socket_recv(sock_idx, buf.as_mut_ptr(), buf.len());
        if n == eagain {
            continue;
        }
        if n < 0 {
            let _ = socket::socket_close(sock_idx);
            return fail!("socket_recv failed: {}", n);
        }
        if n == 0 {
            break;
        }

        let n = n as usize;
        let take = core::cmp::min(response_prefix.len().saturating_sub(prefix_len), n);
        if take > 0 {
            response_prefix[prefix_len..prefix_len + take].copy_from_slice(&buf[..take]);
            prefix_len += take;
        }

        if prefix_len >= 7 && &response_prefix[..7] == b"HTTP/1." {
            let _ = socket::socket_close(sock_idx);
            return pass!();
        }
    }

    let _ = socket::socket_close(sock_idx);
    fail!("HTTP response did not start with 'HTTP/1.' within 5s")
}

slopos_lib::define_test_suite!(
    tcp_live,
    [
        test_route_table_has_default,
        test_netstack_has_ipv4,
        test_arp_resolve_gateway,
        test_tcp_syn_transmit,
        test_tcp_connect_does_not_hang,
        test_tcp_blocking_connect,
        test_tcp_http_get_e2e,
    ]
);
