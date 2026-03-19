#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

//! Network subsystem.
//!
//! Core abstractions (types, pool, packet buffers, device trait) and protocol
//! modules (DHCP, DNS, TCP, UDP) shared across network drivers.
extern crate alloc;

#[cfg(feature = "itests")]
pub mod dns_tests;
pub mod driver_hooks;
#[cfg(feature = "itests")]
pub mod napi_tests;
#[cfg(feature = "itests")]
pub mod neighbor_tests;
#[cfg(feature = "itests")]
pub mod net_types_tests;
pub mod netdev;
#[cfg(feature = "itests")]
pub mod netdev_tests;
#[cfg(feature = "itests")]
pub mod netstack_tests;
pub mod packetbuf;
#[cfg(feature = "itests")]
pub mod packetbuf_tests;
pub mod pool;
#[cfg(feature = "itests")]
pub mod route_tests;
#[cfg(feature = "itests")]
pub mod socket_tests;
#[cfg(feature = "itests")]
pub mod tcp_data_tests;
#[cfg(feature = "itests")]
pub mod tcp_tests;
#[cfg(feature = "itests")]
pub mod timer_tests;
pub mod types;
#[cfg(feature = "itests")]
pub mod udp_socket_tests;

pub mod arp;
pub mod dhcp;
pub mod dns;
pub mod icmp;
#[cfg(feature = "itests")]
pub mod icmp_tests;
pub mod ingress;
#[cfg(feature = "itests")]
pub mod ingress_tests;
pub mod ipv4;
pub mod loopback;
#[cfg(feature = "itests")]
pub mod loopback_tests;
pub mod napi;
pub mod neighbor;
pub mod netstack;
pub mod route;
pub mod socket;
#[cfg(feature = "itests")]
pub mod socket_framework_tests;
#[cfg(feature = "itests")]
pub mod socket_option_tests;
pub mod tcp;
#[cfg(feature = "itests")]
pub mod tcp_live_tests;
pub mod tcp_socket;
#[cfg(feature = "itests")]
pub mod tcp_socket_tests;
pub mod timer;
pub mod udp;
#[cfg(feature = "itests")]
pub mod udp_demux_tests;

// Re-export key type-safe primitives for convenient access.
pub use netdev::{DEVICE_REGISTRY, DeviceHandle, NetDevice, NetDeviceFeatures, NetDeviceStats};
pub use netstack::{IfaceConfig, NET_STACK, NetStack};
pub use packetbuf::PacketBuf;
pub use pool::{PACKET_POOL, PacketPool};
pub use route::{ROUTE_TABLE, RouteEntry, RouteTable};
pub use timer::{FiredTimer, NetTimerWheel, TimerKind, TimerToken};
pub use types::{
    DevIndex, EtherType, IoSlice, IoSliceMut, IpProtocol, Ipv4Addr, MacAddr, NetError, Port,
    SockAddr,
};

// =============================================================================
// Ethernet
// =============================================================================

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETH_HEADER_LEN: usize = 14;
pub const ETH_ADDR_LEN: usize = 6;
pub const ETH_BROADCAST: [u8; 6] = [0xff; 6];

// =============================================================================
// ARP (Ethernet + IPv4 only)
// =============================================================================

pub const ARP_HTYPE_ETHERNET: u16 = 1;
pub const ARP_PTYPE_IPV4: u16 = ETHERTYPE_IPV4;
pub const ARP_HLEN_ETHERNET: u8 = 6;
pub const ARP_PLEN_IPV4: u8 = 4;
pub const ARP_OPER_REQUEST: u16 = 1;
pub const ARP_OPER_REPLY: u16 = 2;
pub const ARP_HEADER_LEN: usize = 28;

// =============================================================================
// IPv4
// =============================================================================

pub const IPV4_HEADER_LEN: usize = 20;
pub const IPV4_BROADCAST: [u8; 4] = [255, 255, 255, 255];
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;
pub const IPPROTO_ICMP: u8 = 1;

pub fn parse_udp_header(payload: &[u8]) -> Option<(u16, u16, &[u8])> {
    if payload.len() < 8 {
        return None;
    }

    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    let udp_len = u16::from_be_bytes([payload[4], payload[5]]) as usize;

    if udp_len < 8 || udp_len > payload.len() {
        return None;
    }

    Some((src_port, dst_port, &payload[8..udp_len]))
}

/// Compute the one's-complement checksum (RFC 1071).
///
/// Works for IPv4 headers, ICMP messages, and any other protocol that
/// uses the standard internet checksum.  Handles odd-length data by
/// padding the trailing byte with a zero.
pub fn ipv4_header_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        sum = sum.wrapping_add(word);
        i += 2;
    }

    // Handle trailing odd byte (pad with zero on the right).
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }

    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }

    !(sum as u16)
}

pub fn udp_checksum(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> u16 {
    let udp_len = 8usize + payload.len();
    let mut sum = 0u32;

    let add_word = |sum: &mut u32, word: u16| {
        *sum = sum.wrapping_add(word as u32);
    };

    add_word(&mut sum, u16::from_be_bytes([src_ip[0], src_ip[1]]));
    add_word(&mut sum, u16::from_be_bytes([src_ip[2], src_ip[3]]));
    add_word(&mut sum, u16::from_be_bytes([dst_ip[0], dst_ip[1]]));
    add_word(&mut sum, u16::from_be_bytes([dst_ip[2], dst_ip[3]]));
    add_word(&mut sum, IPPROTO_UDP as u16);
    add_word(&mut sum, udp_len as u16);

    add_word(&mut sum, src_port);
    add_word(&mut sum, dst_port);
    add_word(&mut sum, udp_len as u16);
    add_word(&mut sum, 0);

    let mut i = 0usize;
    while i + 1 < payload.len() {
        add_word(&mut sum, u16::from_be_bytes([payload[i], payload[i + 1]]));
        i += 2;
    }
    if i < payload.len() {
        add_word(&mut sum, u16::from_be_bytes([payload[i], 0]));
    }

    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }

    let checksum = !(sum as u16);
    if checksum == 0 { 0xffff } else { checksum }
}
