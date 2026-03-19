//! Tests for network type-safe primitives.
//!
//! Covers: Ipv4Addr methods, Port byte-order conversions, MacAddr properties,
//! DevIndex identity, NetError errno mapping, SockAddr user conversion,
//! EtherType/IpProtocol parsing.

use slopos_abi::net::{AF_INET, SockAddrIn};
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::types::*;

// =============================================================================
// 1.T6 — Ipv4Addr methods
// =============================================================================

pub fn test_ipv4_addr_constants() -> TestResult {
    assert_test!(
        Ipv4Addr::UNSPECIFIED.is_unspecified(),
        "UNSPECIFIED is unspecified"
    );
    assert_test!(Ipv4Addr::BROADCAST.is_broadcast(), "BROADCAST is broadcast");
    assert_test!(Ipv4Addr::LOCALHOST.is_loopback(), "LOCALHOST is loopback");
    assert_test!(
        !Ipv4Addr::UNSPECIFIED.is_loopback(),
        "UNSPECIFIED is not loopback"
    );
    assert_test!(
        !Ipv4Addr::LOCALHOST.is_broadcast(),
        "LOCALHOST is not broadcast"
    );
    pass!()
}

pub fn test_ipv4_addr_loopback() -> TestResult {
    assert_test!(
        Ipv4Addr([127, 0, 0, 1]).is_loopback(),
        "127.0.0.1 is loopback"
    );
    assert_test!(
        Ipv4Addr([127, 255, 255, 254]).is_loopback(),
        "127.255.255.254 is loopback"
    );
    assert_test!(
        !Ipv4Addr([128, 0, 0, 1]).is_loopback(),
        "128.0.0.1 is not loopback"
    );
    assert_test!(
        !Ipv4Addr([10, 0, 0, 1]).is_loopback(),
        "10.0.0.1 is not loopback"
    );
    pass!()
}

pub fn test_ipv4_addr_broadcast() -> TestResult {
    assert_test!(
        Ipv4Addr([255, 255, 255, 255]).is_broadcast(),
        "255.255.255.255 is broadcast"
    );
    assert_test!(
        !Ipv4Addr([255, 255, 255, 0]).is_broadcast(),
        "255.255.255.0 is not broadcast"
    );
    assert_test!(
        !Ipv4Addr([0, 0, 0, 0]).is_broadcast(),
        "0.0.0.0 is not broadcast"
    );
    pass!()
}

pub fn test_ipv4_addr_multicast() -> TestResult {
    assert_test!(
        Ipv4Addr([224, 0, 0, 1]).is_multicast(),
        "224.0.0.1 is multicast"
    );
    assert_test!(
        Ipv4Addr([239, 255, 255, 255]).is_multicast(),
        "239.255.255.255 is multicast"
    );
    assert_test!(
        !Ipv4Addr([223, 255, 255, 255]).is_multicast(),
        "223.x is not multicast"
    );
    assert_test!(
        !Ipv4Addr([240, 0, 0, 1]).is_multicast(),
        "240.x is not multicast"
    );
    pass!()
}

pub fn test_ipv4_addr_in_subnet() -> TestResult {
    let net = Ipv4Addr([192, 168, 1, 0]);
    let mask = Ipv4Addr([255, 255, 255, 0]);

    assert_test!(
        Ipv4Addr::in_subnet(Ipv4Addr([192, 168, 1, 100]), net, mask),
        "192.168.1.100 in /24"
    );
    assert_test!(
        Ipv4Addr::in_subnet(Ipv4Addr([192, 168, 1, 0]), net, mask),
        "192.168.1.0 in /24"
    );
    assert_test!(
        Ipv4Addr::in_subnet(Ipv4Addr([192, 168, 1, 255]), net, mask),
        "192.168.1.255 in /24"
    );
    assert_test!(
        !Ipv4Addr::in_subnet(Ipv4Addr([192, 168, 2, 1]), net, mask),
        "192.168.2.1 not in /24"
    );
    assert_test!(
        !Ipv4Addr::in_subnet(Ipv4Addr([10, 0, 0, 1]), net, mask),
        "10.0.0.1 not in /24"
    );

    // /16 subnet
    let net16 = Ipv4Addr([10, 0, 0, 0]);
    let mask16 = Ipv4Addr([255, 255, 0, 0]);
    assert_test!(
        Ipv4Addr::in_subnet(Ipv4Addr([10, 0, 99, 1]), net16, mask16),
        "10.0.99.1 in /16"
    );
    assert_test!(
        !Ipv4Addr::in_subnet(Ipv4Addr([10, 1, 0, 1]), net16, mask16),
        "10.1.0.1 not in 10.0.0.0/16"
    );
    pass!()
}

pub fn test_ipv4_addr_byte_conversions() -> TestResult {
    let addr = Ipv4Addr([192, 168, 1, 1]);
    let as_u32 = addr.to_u32_be();
    let roundtrip = Ipv4Addr::from_u32_be(as_u32);
    assert_eq_test!(addr, roundtrip, "u32_be round-trip");

    // Known value: 192.168.1.1 = 0xC0A80101
    assert_eq_test!(as_u32, 0xC0A8_0101u32, "192.168.1.1 = 0xC0A80101");

    let from_bytes = Ipv4Addr::from_bytes([10, 0, 2, 15]);
    assert_eq_test!(from_bytes.0, [10, 0, 2, 15], "from_bytes preserves bytes");
    assert_eq_test!(from_bytes.as_bytes(), &[10, 0, 2, 15], "as_bytes accessor");
    pass!()
}

pub fn test_ipv4_addr_unspecified() -> TestResult {
    assert_test!(
        Ipv4Addr([0, 0, 0, 0]).is_unspecified(),
        "0.0.0.0 is unspecified"
    );
    assert_test!(
        !Ipv4Addr([0, 0, 0, 1]).is_unspecified(),
        "0.0.0.1 is not unspecified"
    );
    pass!()
}

// =============================================================================
// 1.T7 — Port byte-order conversions
// =============================================================================

pub fn test_port_network_bytes_roundtrip() -> TestResult {
    let port = Port::new(8080);
    let net_bytes = port.to_network_bytes();
    let roundtrip = Port::from_network_bytes(net_bytes);
    assert_eq_test!(port, roundtrip, "network bytes round-trip");

    // 8080 in big-endian = [0x1F, 0x90]
    assert_eq_test!(net_bytes, [0x1F, 0x90], "8080 network bytes");
    pass!()
}

pub fn test_port_well_known_values() -> TestResult {
    // Port 80 in big-endian = [0x00, 0x50]
    let http = Port::new(80);
    assert_eq_test!(http.to_network_bytes(), [0x00, 0x50], "port 80 bytes");

    // Port 443 in big-endian = [0x01, 0xBB]
    let https = Port::new(443);
    assert_eq_test!(https.to_network_bytes(), [0x01, 0xBB], "port 443 bytes");

    // Port 53 (DNS) in big-endian = [0x00, 0x35]
    let dns = Port::new(53);
    assert_eq_test!(dns.to_network_bytes(), [0x00, 0x35], "port 53 bytes");
    pass!()
}

pub fn test_port_ranges() -> TestResult {
    assert_test!(Port::new(0).is_privileged(), "port 0 is privileged");
    assert_test!(Port::new(80).is_privileged(), "port 80 is privileged");
    assert_test!(Port::new(1023).is_privileged(), "port 1023 is privileged");
    assert_test!(
        !Port::new(1024).is_privileged(),
        "port 1024 is not privileged"
    );
    assert_test!(
        !Port::new(8080).is_privileged(),
        "port 8080 is not privileged"
    );

    assert_test!(Port::new(49152).is_ephemeral(), "port 49152 is ephemeral");
    assert_test!(Port::new(65535).is_ephemeral(), "port 65535 is ephemeral");
    assert_test!(
        !Port::new(49151).is_ephemeral(),
        "port 49151 is not ephemeral"
    );
    assert_test!(
        !Port::new(1024).is_ephemeral(),
        "port 1024 is not ephemeral"
    );
    pass!()
}

pub fn test_port_as_u16() -> TestResult {
    assert_eq_test!(Port::new(12345).as_u16(), 12345u16, "as_u16 accessor");
    assert_eq_test!(Port::new(0).as_u16(), 0u16, "port 0 as_u16");
    pass!()
}

// =============================================================================
// MacAddr tests
// =============================================================================

pub fn test_mac_addr_constants() -> TestResult {
    assert_test!(MacAddr::BROADCAST.is_broadcast(), "BROADCAST is broadcast");
    assert_test!(MacAddr::ZERO.is_zero(), "ZERO is zero");
    assert_test!(!MacAddr::ZERO.is_broadcast(), "ZERO is not broadcast");
    assert_test!(!MacAddr::BROADCAST.is_zero(), "BROADCAST is not zero");
    pass!()
}

pub fn test_mac_addr_multicast() -> TestResult {
    // Broadcast is also multicast (bit 0 of first octet is set)
    assert_test!(MacAddr::BROADCAST.is_multicast(), "broadcast is multicast");

    // Multicast: first octet has LSB set
    assert_test!(
        MacAddr([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]).is_multicast(),
        "01:00:5e:00:00:01 is multicast"
    );

    // Unicast: first octet has LSB clear
    assert_test!(
        !MacAddr([0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]).is_multicast(),
        "unicast MAC is not multicast"
    );
    assert_test!(
        !MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).is_multicast(),
        "locally-administered unicast is not multicast"
    );
    pass!()
}

pub fn test_mac_addr_as_bytes() -> TestResult {
    let mac = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    assert_eq_test!(
        mac.as_bytes(),
        &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        "as_bytes accessor"
    );
    pass!()
}

// =============================================================================
// DevIndex tests
// =============================================================================

pub fn test_dev_index_equality() -> TestResult {
    assert_eq_test!(DevIndex(0), DevIndex(0), "same index equal");
    assert_test!(DevIndex(0) != DevIndex(1), "different indices not equal");
    pass!()
}

// =============================================================================
// NetError tests
// =============================================================================

pub fn test_net_error_errno_mapping() -> TestResult {
    // Spot-check critical errno values against Linux constants
    assert_eq_test!(NetError::WouldBlock.to_errno(), -11, "EAGAIN");
    assert_eq_test!(NetError::ConnectionRefused.to_errno(), -111, "ECONNREFUSED");
    assert_eq_test!(NetError::ConnectionReset.to_errno(), -104, "ECONNRESET");
    assert_eq_test!(NetError::ConnectionAborted.to_errno(), -103, "ECONNABORTED");
    assert_eq_test!(NetError::TimedOut.to_errno(), -110, "ETIMEDOUT");
    assert_eq_test!(NetError::AddressInUse.to_errno(), -98, "EADDRINUSE");
    assert_eq_test!(
        NetError::AddressNotAvailable.to_errno(),
        -99,
        "EADDRNOTAVAIL"
    );
    assert_eq_test!(NetError::NotConnected.to_errno(), -107, "ENOTCONN");
    assert_eq_test!(NetError::AlreadyConnected.to_errno(), -106, "EISCONN");
    assert_eq_test!(NetError::NetworkUnreachable.to_errno(), -101, "ENETUNREACH");
    assert_eq_test!(NetError::HostUnreachable.to_errno(), -113, "EHOSTUNREACH");
    assert_eq_test!(NetError::PermissionDenied.to_errno(), -1, "EPERM");
    assert_eq_test!(NetError::InvalidArgument.to_errno(), -22, "EINVAL");
    assert_eq_test!(NetError::NoBufferSpace.to_errno(), -105, "ENOBUFS");
    assert_eq_test!(
        NetError::ProtocolNotSupported.to_errno(),
        -93,
        "EPROTONOSUPPORT"
    );
    assert_eq_test!(
        NetError::AddressFamilyNotSupported.to_errno(),
        -97,
        "EAFNOSUPPORT"
    );
    assert_eq_test!(
        NetError::SocketNotBound.to_errno(),
        -22,
        "EINVAL (not bound)"
    );
    assert_eq_test!(NetError::InProgress.to_errno(), -115, "EINPROGRESS");
    assert_eq_test!(
        NetError::OperationNotSupported.to_errno(),
        -95,
        "EOPNOTSUPP"
    );
    assert_eq_test!(NetError::Shutdown.to_errno(), -32, "EPIPE");
    pass!()
}

pub fn test_net_error_all_negative() -> TestResult {
    // Every errno must be negative
    let all = [
        NetError::WouldBlock,
        NetError::ConnectionRefused,
        NetError::ConnectionReset,
        NetError::ConnectionAborted,
        NetError::TimedOut,
        NetError::AddressInUse,
        NetError::AddressNotAvailable,
        NetError::NotConnected,
        NetError::AlreadyConnected,
        NetError::NetworkUnreachable,
        NetError::HostUnreachable,
        NetError::PermissionDenied,
        NetError::InvalidArgument,
        NetError::NoBufferSpace,
        NetError::ProtocolNotSupported,
        NetError::AddressFamilyNotSupported,
        NetError::SocketNotBound,
        NetError::InProgress,
        NetError::OperationNotSupported,
        NetError::Shutdown,
    ];
    for err in &all {
        assert_test!(err.to_errno() < 0, "errno must be negative");
    }
    pass!()
}

// =============================================================================
// SockAddr tests
// =============================================================================

pub fn test_sock_addr_from_user_valid() -> TestResult {
    let raw = SockAddrIn {
        family: AF_INET,
        port: 8080u16.to_be(), // network byte order
        addr: [10, 0, 2, 15],
        _pad: [0; 8],
    };
    let addr = match SockAddr::from_user(&raw) {
        Ok(a) => a,
        Err(_) => return slopos_lib::fail!("from_user failed on valid input"),
    };
    assert_eq_test!(addr.ip, Ipv4Addr([10, 0, 2, 15]), "ip parsed correctly");
    assert_eq_test!(addr.port, Port::new(8080), "port parsed correctly");
    pass!()
}

pub fn test_sock_addr_from_user_invalid_family() -> TestResult {
    let raw = SockAddrIn {
        family: 99, // not AF_INET
        port: 80u16.to_be(),
        addr: [1, 2, 3, 4],
        _pad: [0; 8],
    };
    assert_test!(
        SockAddr::from_user(&raw).is_err(),
        "non-AF_INET family rejected"
    );
    match SockAddr::from_user(&raw) {
        Err(NetError::AddressFamilyNotSupported) => {}
        _ => return slopos_lib::fail!("expected AddressFamilyNotSupported"),
    }
    pass!()
}

pub fn test_sock_addr_to_user_roundtrip() -> TestResult {
    let addr = SockAddr::new(Ipv4Addr([192, 168, 1, 1]), Port::new(443));
    let raw = addr.to_user();
    assert_eq_test!(raw.family, AF_INET, "family is AF_INET");
    assert_eq_test!(raw.addr, [192, 168, 1, 1], "addr preserved");
    // Port should be in network byte order in the SockAddrIn
    assert_eq_test!(raw.port, 443u16.to_be(), "port in network byte order");

    // Full round-trip: to_user -> from_user should give back the original
    let roundtrip = match SockAddr::from_user(&raw) {
        Ok(a) => a,
        Err(_) => return slopos_lib::fail!("round-trip from_user failed"),
    };
    assert_eq_test!(roundtrip, addr, "round-trip preserves address");
    pass!()
}

pub fn test_sock_addr_unspecified() -> TestResult {
    let addr = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port::new(0));
    let raw = addr.to_user();
    assert_eq_test!(raw.addr, [0, 0, 0, 0], "unspecified addr");
    assert_eq_test!(raw.port, 0u16, "port 0");
    pass!()
}

// =============================================================================
// EtherType tests
// =============================================================================

pub fn test_ether_type_from_u16() -> TestResult {
    assert_eq_test!(
        EtherType::from_u16(0x0800),
        Some(EtherType::Ipv4),
        "0x0800 = IPv4"
    );
    assert_eq_test!(
        EtherType::from_u16(0x0806),
        Some(EtherType::Arp),
        "0x0806 = ARP"
    );
    assert_eq_test!(
        EtherType::from_u16(0x86DD),
        Some(EtherType::Ipv6),
        "0x86DD = IPv6"
    );
    assert_eq_test!(EtherType::from_u16(0x1234), None, "unknown type = None");
    assert_eq_test!(EtherType::from_u16(0x0000), None, "0x0000 = None");
    pass!()
}

pub fn test_ether_type_as_u16() -> TestResult {
    assert_eq_test!(EtherType::Ipv4.as_u16(), 0x0800u16, "IPv4 as u16");
    assert_eq_test!(EtherType::Arp.as_u16(), 0x0806u16, "ARP as u16");
    assert_eq_test!(EtherType::Ipv6.as_u16(), 0x86DDu16, "IPv6 as u16");
    pass!()
}

// =============================================================================
// IpProtocol tests
// =============================================================================

pub fn test_ip_protocol_from_u8() -> TestResult {
    assert_eq_test!(IpProtocol::from_u8(1), Some(IpProtocol::Icmp), "1 = ICMP");
    assert_eq_test!(IpProtocol::from_u8(6), Some(IpProtocol::Tcp), "6 = TCP");
    assert_eq_test!(IpProtocol::from_u8(17), Some(IpProtocol::Udp), "17 = UDP");
    assert_eq_test!(IpProtocol::from_u8(0), None, "0 = None");
    assert_eq_test!(IpProtocol::from_u8(255), None, "255 = None");
    assert_eq_test!(IpProtocol::from_u8(47), None, "47 (GRE) = None");
    pass!()
}

pub fn test_ip_protocol_as_u8() -> TestResult {
    assert_eq_test!(IpProtocol::Icmp.as_u8(), 1u8, "ICMP as u8");
    assert_eq_test!(IpProtocol::Tcp.as_u8(), 6u8, "TCP as u8");
    assert_eq_test!(IpProtocol::Udp.as_u8(), 17u8, "UDP as u8");
    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    net_types,
    [
        // Ipv4Addr (1.T6)
        test_ipv4_addr_constants,
        test_ipv4_addr_loopback,
        test_ipv4_addr_broadcast,
        test_ipv4_addr_multicast,
        test_ipv4_addr_in_subnet,
        test_ipv4_addr_byte_conversions,
        test_ipv4_addr_unspecified,
        // Port (1.T7)
        test_port_network_bytes_roundtrip,
        test_port_well_known_values,
        test_port_ranges,
        test_port_as_u16,
        // MacAddr
        test_mac_addr_constants,
        test_mac_addr_multicast,
        test_mac_addr_as_bytes,
        // DevIndex
        test_dev_index_equality,
        // NetError
        test_net_error_errno_mapping,
        test_net_error_all_negative,
        // SockAddr
        test_sock_addr_from_user_valid,
        test_sock_addr_from_user_invalid_family,
        test_sock_addr_to_user_roundtrip,
        test_sock_addr_unspecified,
        // EtherType
        test_ether_type_from_u16,
        test_ether_type_as_u16,
        // IpProtocol
        test_ip_protocol_from_u8,
        test_ip_protocol_as_u8,
    ]
);
