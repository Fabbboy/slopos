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
pub mod netinfo;
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

pub const ETH_HEADER_LEN: usize = 14;
pub const ETH_ADDR_LEN: usize = 6;

// =============================================================================
// IPv4
// =============================================================================

pub const IPV4_HEADER_LEN: usize = 20;
