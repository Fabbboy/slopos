#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Network subsystem.
//!
//! Core abstractions (types, pool, packet buffers, device trait) and protocol
//! modules (DHCP, DNS, TCP, UDP) shared across network drivers.
extern crate alloc;

pub mod net_driver_service;
pub mod netdev;
pub mod netinfo;
pub mod packetbuf;
pub mod pool;
#[cfg(feature = "itests")]
pub mod tests;
pub mod types;

pub mod arp;
pub mod checksum;
pub mod dhcp;
pub mod dns;
pub mod icmp;
pub mod ingress;
pub mod ipv4;
pub mod loopback;
pub mod napi;
pub mod neighbor;
pub mod netstack;
pub mod reassembly;
pub mod route;
pub mod socket;
pub mod socket_file_ops;
pub mod tcp;
pub mod timer;
pub mod udp;
pub mod unix_socket;
pub mod unix_socket_file_ops;

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
