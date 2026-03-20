#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

//! Network subsystem.
//!
//! Core abstractions (types, pool, packet buffers, device trait) and protocol
//! modules (DHCP, DNS, TCP, UDP) shared across network drivers.
extern crate alloc;

#[cfg(feature = "itests")]
pub mod dns_tests;
#[cfg(feature = "itests")]
pub mod napi_tests;
#[cfg(feature = "itests")]
pub mod neighbor_tests;
pub mod net_driver_service;
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

pub mod arp;
pub mod checksum;
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
pub use types::{DevIndex, EtherType, IpProtocol, Ipv4Addr, MacAddr, NetError, Port, SockAddr};

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

pub use checksum::internet_checksum as ipv4_header_checksum;
pub use checksum::udp_checksum;
