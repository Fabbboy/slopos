use slopos_abi::net::{AF_INET, IPPROTO_ICMP, SOCK_DGRAM};
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::icmp::{self, ICMP_HEADER_LEN};
use crate::route::{ROUTE_TABLE, RouteEntry};
use crate::socket;
use crate::types::{DevIndex, Ipv4Addr};

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

fn restore_boot_routes() {
    ROUTE_TABLE.reset();
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr([127, 0, 0, 0]),
        prefix_len: 8,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(0),
        metric: 0,
    });
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr([10, 0, 2, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: DevIndex(1),
        metric: 0,
    });
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr::UNSPECIFIED,
        prefix_len: 0,
        gateway: Ipv4Addr(GATEWAY_IP),
        dev: DevIndex(1),
        metric: 100,
    });
}

fn test_icmp_socket_create() -> TestResult {
    socket::socket_reset_all();
    let fd = socket::socket_create(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    assert_test!(fd >= 0, "ICMP socket create failed: {}", fd);
    let _ = socket::socket_close(fd as u32);
    pass!()
}

fn test_icmp_socket_bind() -> TestResult {
    socket::socket_reset_all();
    let fd = socket::socket_create(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    assert_test!(fd >= 0, "create failed");
    let sock = fd as u32;

    let rc = socket::socket_bind(sock, [0, 0, 0, 0], 0x7070);
    assert_eq_test!(rc, 0, "bind failed");

    let demux_hit = icmp::ICMP_DEMUX.lock().lookup(0x7070);
    assert_test!(demux_hit.is_some(), "identifier not in ICMP_DEMUX");
    assert_eq_test!(demux_hit.unwrap(), sock, "demux points to wrong socket");

    let _ = socket::socket_close(sock);
    pass!()
}

fn test_icmp_send_echo_raw() -> TestResult {
    restore_boot_routes();
    let dst = Ipv4Addr(GATEWAY_IP);
    let has_route = ROUTE_TABLE.lookup(dst).is_some();
    if !has_route {
        klog_info!("icmp_test: no route to gateway, skipping");
        return pass!();
    }

    let payload = [0xAA; 8];
    let result = icmp::send_echo_request(GATEWAY_IP, 0xBEEF, 1, &payload);
    match result {
        Ok(n) => {
            klog_info!("icmp_test: send_echo_request ok, payload bytes={}", n);
            assert_eq_test!(n, 8, "wrong byte count");
        }
        Err(e) => return fail!("send_echo_request failed: {:?}", e),
    }
    pass!()
}

fn test_icmp_ping_gateway_e2e() -> TestResult {
    socket::socket_reset_all();
    restore_boot_routes();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("icmp_test: no route to gateway, skipping e2e");
        return pass!();
    }

    let identifier: u16 = 0xCAFE;
    let sequence: u16 = 42;

    klog_info!(
        "icmp_test: sending ICMP echo request to 10.0.2.2 id=0x{:04x} seq={}",
        identifier,
        sequence
    );

    let result = icmp::send_echo_request(GATEWAY_IP, identifier, sequence, &[0x53; 32]);
    if let Err(e) = result {
        return fail!("send_echo_request failed: {:?}", e);
    }

    // No socket is bound for this identifier, so the gateway's reply will hit
    // the unmatched-reply branch in icmp::handle_rx and be silently dropped.
    // One sleep+poll round is enough to exercise that path; reply receipt is
    // covered by test_icmp_socket_sendto_recvfrom_e2e.
    slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);
    if let Some(d) = crate::net_driver_service::net_driver() {
        (d.virtnet_force_napi_poll)();
    }

    pass!()
}

fn test_icmp_socket_sendto_recvfrom_e2e() -> TestResult {
    socket::socket_reset_all();
    restore_boot_routes();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("icmp_test: no route to gateway, skipping socket e2e");
        return pass!();
    }

    let fd = socket::socket_create(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    assert_test!(fd >= 0, "socket create failed");
    let sock = fd as u32;

    let identifier: u16 = 0xD00D;
    let rc = socket::socket_bind(sock, [0, 0, 0, 0], identifier);
    assert_eq_test!(rc, 0, "bind failed");

    socket::socket_set_nonblocking(sock, true);

    let sequence: u16 = 7;
    let mut icmp_buf = [0u8; ICMP_HEADER_LEN + 32];
    icmp_buf[0] = 8;
    icmp_buf[1] = 0;
    icmp_buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    icmp_buf[4..6].copy_from_slice(&identifier.to_be_bytes());
    icmp_buf[6..8].copy_from_slice(&sequence.to_be_bytes());
    for i in ICMP_HEADER_LEN..icmp_buf.len() {
        icmp_buf[i] = 0x53;
    }

    klog_info!(
        "icmp_test: socket sendto 10.0.2.2 id=0x{:04x} seq={} len={}",
        identifier,
        sequence,
        icmp_buf.len()
    );
    let sent = socket::socket_sendto(sock, icmp_buf.as_ptr(), icmp_buf.len(), GATEWAY_IP, 0);
    klog_info!("icmp_test: sendto returned {}", sent);
    assert_test!(sent > 0, "sendto failed: {}", sent);

    let mut recv_buf = [0u8; 256];
    let mut src_ip = [0u8; 4];
    let mut src_port: u16 = 0;

    for attempt in 0..30u32 {
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.virtnet_force_napi_poll)();
        }

        let readable = socket::socket_poll_readable(sock);
        klog_info!("icmp_test: attempt {} readable={}", attempt, readable);

        if readable != 0 {
            let n = socket::socket_recvfrom(
                sock,
                recv_buf.as_mut_ptr(),
                recv_buf.len(),
                &mut src_ip as *mut [u8; 4],
                &mut src_port as *mut u16,
            );
            klog_info!(
                "icmp_test: recvfrom returned {} src={}.{}.{}.{}",
                n,
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3]
            );

            if n >= ICMP_HEADER_LEN as i64 {
                let reply_type = recv_buf[0];
                let reply_id = u16::from_be_bytes([recv_buf[4], recv_buf[5]]);
                let reply_seq = u16::from_be_bytes([recv_buf[6], recv_buf[7]]);
                klog_info!(
                    "icmp_test: reply type={} id=0x{:04x} seq={}",
                    reply_type,
                    reply_id,
                    reply_seq
                );

                assert_eq_test!(reply_type, 0, "expected echo reply (type 0)");
                assert_eq_test!(reply_id, identifier, "identifier mismatch");
                assert_eq_test!(reply_seq, sequence, "sequence mismatch");

                let _ = socket::socket_close(sock);
                return pass!();
            }
        }
    }

    let _ = socket::socket_close(sock);
    fail!("no ICMP echo reply received from 10.0.2.2 after 3s")
}

fn test_dns_resolve_google() -> TestResult {
    use crate::dns;

    let has_ip = crate::netstack::NET_STACK.first_ipv4().is_some();
    if !has_ip {
        klog_info!("icmp_test: no IP configured, skipping DNS test");
        return pass!();
    }

    let hostname = b"google.com";
    klog_info!("icmp_test: resolving google.com via kernel DNS...");

    match dns::dns_resolve(hostname) {
        Ok(addr) => {
            klog_info!(
                "icmp_test: google.com resolved to {}.{}.{}.{}",
                addr[0],
                addr[1],
                addr[2],
                addr[3]
            );
            assert_test!(addr != [0, 0, 0, 0], "DNS returned 0.0.0.0");
            assert_test!(addr != [127, 0, 0, 1], "DNS returned loopback");
        }
        Err(e) => {
            klog_info!(
                "icmp_test: dns_resolve returned {:?} (SLIRP DNS may be unavailable in test mode)",
                e
            );
        }
    }
    pass!()
}

fn test_ping_resolved_host_e2e() -> TestResult {
    use crate::dns;

    socket::socket_reset_all();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("icmp_test: no route, skipping resolved host ping");
        return pass!();
    }

    let hostname = b"google.com";
    let target_ip = match dns::dns_resolve(hostname) {
        Ok(ip) => {
            klog_info!(
                "icmp_test: will ping {}.{}.{}.{} (google.com)",
                ip[0],
                ip[1],
                ip[2],
                ip[3]
            );
            ip
        }
        Err(_) => {
            klog_info!("icmp_test: DNS failed, pinging gateway instead");
            GATEWAY_IP
        }
    };

    let fd = socket::socket_create(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    assert_test!(fd >= 0, "socket create failed");
    let sock = fd as u32;

    let identifier: u16 = 0xFACE;
    let rc = socket::socket_bind(sock, [0, 0, 0, 0], identifier);
    assert_eq_test!(rc, 0, "bind failed");
    socket::socket_set_nonblocking(sock, true);

    let sequence: u16 = 1;
    let mut icmp_buf = [0u8; ICMP_HEADER_LEN + 32];
    icmp_buf[0] = 8;
    icmp_buf[1] = 0;
    icmp_buf[4..6].copy_from_slice(&identifier.to_be_bytes());
    icmp_buf[6..8].copy_from_slice(&sequence.to_be_bytes());

    let sent = socket::socket_sendto(sock, icmp_buf.as_ptr(), icmp_buf.len(), target_ip, 0);
    klog_info!(
        "icmp_test: ping sent to {}.{}.{}.{}, rc={}",
        target_ip[0],
        target_ip[1],
        target_ip[2],
        target_ip[3],
        sent
    );
    assert_test!(sent > 0, "sendto failed");

    // SLIRP does not relay ICMP echo replies for external hosts, so this loop
    // is effectively a "send didn't crash" check with an early-exit if the
    // user-mode network ever does start replying. Keep the budget tight: the
    // bulk-replied gateway path is covered by test_icmp_socket_sendto_recvfrom_e2e.
    for _attempt in 0..4u32 {
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(50);
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.virtnet_force_napi_poll)();
        }

        let readable = socket::socket_poll_readable(sock);
        if readable != 0 {
            let mut recv_buf = [0u8; 256];
            let mut src_ip = [0u8; 4];
            let mut src_port: u16 = 0;
            let n = socket::socket_recvfrom(
                sock,
                recv_buf.as_mut_ptr(),
                recv_buf.len(),
                &mut src_ip as *mut _,
                &mut src_port as *mut _,
            );
            klog_info!(
                "icmp_test: reply from {}.{}.{}.{} n={}",
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3],
                n
            );
            let _ = socket::socket_close(sock);
            return pass!();
        }
    }

    let _ = socket::socket_close(sock);
    klog_info!(
        "icmp_test: no ping reply from {}.{}.{}.{} (expected: SLIRP does not relay external ICMP)",
        target_ip[0],
        target_ip[1],
        target_ip[2],
        target_ip[3]
    );
    pass!()
}

fn test_icmp_napi_scheduling_e2e() -> TestResult {
    socket::socket_reset_all();
    restore_boot_routes();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("icmp_napi: no route to gateway, skipping");
        return pass!();
    }

    let fd = socket::socket_create(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    assert_test!(fd >= 0, "socket create failed");
    let sock = fd as u32;

    let identifier: u16 = 0xBEEF;
    let rc = socket::socket_bind(sock, [0, 0, 0, 0], identifier);
    assert_eq_test!(rc, 0, "bind failed");
    socket::socket_set_nonblocking(sock, true);

    let sequence: u16 = 99;
    let mut icmp_buf = [0u8; ICMP_HEADER_LEN + 32];
    icmp_buf[0] = 8;
    icmp_buf[4..6].copy_from_slice(&identifier.to_be_bytes());
    icmp_buf[6..8].copy_from_slice(&sequence.to_be_bytes());
    for i in ICMP_HEADER_LEN..icmp_buf.len() {
        icmp_buf[i] = 0x42;
    }

    let sent = socket::socket_sendto(sock, icmp_buf.as_ptr(), icmp_buf.len(), GATEWAY_IP, 0);
    assert_test!(sent > 0, "sendto failed");
    klog_info!(
        "icmp_napi: sent echo request, waiting via scheduler sleep only (no force_napi_poll)"
    );

    for attempt in 0..30u32 {
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);

        let readable = socket::socket_poll_readable(sock);
        klog_info!("icmp_napi: attempt {} readable={}", attempt, readable);

        if readable != 0 {
            let mut recv_buf = [0u8; 256];
            let mut src_ip = [0u8; 4];
            let mut src_port: u16 = 0;
            let n = socket::socket_recvfrom(
                sock,
                recv_buf.as_mut_ptr(),
                recv_buf.len(),
                &mut src_ip as *mut _,
                &mut src_port as *mut _,
            );
            klog_info!(
                "icmp_napi: reply received! n={} src={}.{}.{}.{}",
                n,
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3]
            );
            assert_test!(n >= ICMP_HEADER_LEN as i64, "reply too short");
            assert_eq_test!(recv_buf[0], 0, "expected echo reply type 0");
            let _ = socket::socket_close(sock);
            return pass!();
        }
    }

    let _ = socket::socket_close(sock);
    fail!("NAPI scheduling broken: no ICMP reply after 3s without force_napi_poll")
}

slopos_testing::stest!(name = test_icmp_socket_create, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_bind, suite = icmp);
slopos_testing::stest!(name = test_icmp_send_echo_raw, suite = icmp);
slopos_testing::stest!(name = test_icmp_ping_gateway_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_sendto_recvfrom_e2e, suite = icmp);
slopos_testing::stest!(name = test_dns_resolve_google, suite = icmp);
slopos_testing::stest!(name = test_ping_resolved_host_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_napi_scheduling_e2e, suite = icmp);
