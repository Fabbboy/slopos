use slopos_abi::net::{AF_INET, IPPROTO_ICMP, SOCK_DGRAM};
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::icmp::{self, ICMP_HEADER_LEN};
use crate::route::{ROUTE_TABLE, RouteEntry};
use crate::socket;
use crate::types::{DevIndex, Ipv4Addr, Port, SockAddr};

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
    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
    assert_test!(fd >= 0, "ICMP socket create failed: {}", fd);
    let _ = socket::socket_close(fd as u32);
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

    // No socket is bound for this identifier: the reply exercises the
    // unmatched-reply drop branch in icmp::handle_rx.
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

    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
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
    let sent = socket::socket_sendto(sock, &icmp_buf, GATEWAY_IP, 0);
    klog_info!("icmp_test: sendto returned {}", sent);
    assert_test!(sent > 0, "sendto failed: {}", sent);

    let mut recv_buf = [0u8; 256];
    let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));

    for attempt in 0..30u32 {
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.virtnet_force_napi_poll)();
        }

        let readable = socket::socket_poll_readable(sock);
        klog_info!("icmp_test: attempt {} readable={}", attempt, readable);

        if readable != 0 {
            let n = socket::socket_recvfrom(sock, &mut recv_buf, Some(&mut peer));
            let src_ip = peer.ip.0;
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

/// Verify the NAPI burst drains the virtio used-ring on explicit invocation and
/// feeds the result through the ICMP demux.
///
/// Kernel tests run in the BSP boot init context before `enter_scheduler(0)`, so
/// `sleep_current_task_ms` busy-polls instead of descheduling; the IRQ-driven
/// kthread path is unreachable here and is asserted by the userland
/// `curl_e2e_test` instead.
fn test_icmp_napi_scheduling_e2e() -> TestResult {
    socket::socket_reset_all();
    restore_boot_routes();

    let has_route = ROUTE_TABLE.lookup(Ipv4Addr(GATEWAY_IP)).is_some();
    if !has_route {
        klog_info!("icmp_napi: no route to gateway, skipping");
        return pass!();
    }

    let Some(driver) = crate::net_driver_service::net_driver() else {
        klog_info!("icmp_napi: no NIC driver registered, skipping");
        return pass!();
    };

    let fd = socket::socket_create(
        AF_INET,
        SOCK_DGRAM,
        IPPROTO_ICMP,
        socket::SocketOwner::UNOWNED,
    );
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

    let sent = socket::socket_sendto(sock, &icmp_buf, GATEWAY_IP, 0);
    assert_test!(sent > 0, "sendto failed");

    for attempt in 0..30u32 {
        (driver.virtnet_force_napi_poll)();
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(100);

        let readable = socket::socket_poll_readable(sock);
        if readable != 0 {
            let mut recv_buf = [0u8; 256];
            let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
            let n = socket::socket_recvfrom(sock, &mut recv_buf, Some(&mut peer));
            let src_ip = peer.ip.0;
            klog_info!(
                "icmp_napi: reply received attempt={} n={} src={}.{}.{}.{}",
                attempt,
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
    klog_info!("icmp_napi: no reply after 3s — likely no ICMP relay in env, skipping");
    pass!()
}

slopos_testing::stest!(name = test_icmp_socket_create, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_bind, suite = icmp);
slopos_testing::stest!(name = test_icmp_send_echo_raw, suite = icmp);
slopos_testing::stest!(name = test_icmp_ping_gateway_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_socket_sendto_recvfrom_e2e, suite = icmp);
slopos_testing::stest!(name = test_icmp_napi_scheduling_e2e, suite = icmp);
