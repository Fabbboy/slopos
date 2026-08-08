#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

//! Network subsystem.
//!
//! Core abstractions (types, pool, packet buffers, device trait) and protocol
//! modules (DHCP, DNS, TCP, UDP) shared across network drivers.

// Self-alias so the `#[xdp_filter]` proc-macro (which emits fully-qualified
// `::slopos_net::xdp::…` paths) resolves when filters are authored inside this
// crate, exactly as it does from external crates.
extern crate self as slopos_net;

pub mod iface;
pub mod iface_ctl;
pub mod kconsole;
pub mod net_driver_service;
pub mod netdev;
pub mod netmon;
pub mod netmon_file_ops;
pub mod netseq;
pub mod packetbuf;
pub mod pool;
#[cfg(feature = "test-hooks")]
pub mod tests;
pub mod types;

pub mod arp;
pub mod checksum;
pub mod clock;
pub mod connectivity;
pub mod dhcp;
pub mod dns;
pub mod icmp;
pub mod ingress;
pub mod ipv4;
pub mod loopback;
pub mod napi;
pub mod napi_waker;
pub mod neighbor;
pub mod reassembly;
pub mod resolver;
pub mod route;
pub mod socket;
pub mod socket_file_ops;
pub mod tcp;
pub mod timer;
pub mod udp;
pub mod unix_socket;
pub mod unix_socket_file_ops;
pub mod xdp;

// Re-export key type-safe primitives for convenient access.
pub use iface::{Iface, IfaceAddr, IfaceKind, OperState};
pub use netdev::{DEVICE_REGISTRY, DeviceHandle, NetDevice, NetDeviceFeatures, NetDeviceStats};
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
