//! Per-interface IPv4 configuration and centralised network stack state.
//!
//! # Architecture
//!
//! Every registered network device gets an [`IfaceConfig`] describing its IPv4
//! address, netmask, gateway, and DNS servers.  The [`NetStack`] struct
//! aggregates all interface configs behind an [`SpinLock`] and serves as the
//! single source of truth for "our IP" queries — replacing the ad-hoc
//! `virtio_net_ipv4_addr()` lookups that were scattered across the stack.
//!
//! # Concurrency
//!
//! All mutable state is behind an [`SpinLock`].  Reads and writes are
//! serialised.  The lock is held only briefly (no blocking I/O under lock).
//!
//! # Integration
//!
//! - **DHCP**: calls [`NetStack::configure`] when a lease is obtained.
//! - **ARP**: calls [`NetStack::our_ip`] to decide whether to respond to
//!   requests.
//! - **Socket layer**: calls [`NetStack::our_ip`] for source address selection.
//! - TODO: add routing table updates triggered by `configure()`.

use core::fmt;

use slopos_ostd::KVec;
use slopos_ostd::klog_debug;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::types::{DevIndex, Ipv4Addr};

// =============================================================================
// 3A.1 — IfaceConfig
// =============================================================================

/// Per-interface IPv4 configuration.
///
/// Created by [`NetStack::configure`] when a DHCP lease is obtained or a
/// static address is assigned.  One `IfaceConfig` exists per registered
/// network device that has been configured.
#[derive(Clone, Copy)]
pub struct IfaceConfig {
    /// Device index this config belongs to.
    pub dev_index: DevIndex,
    /// IPv4 address assigned to this interface.
    pub ipv4_addr: Ipv4Addr,
    /// Subnet mask (e.g. `255.255.255.0`).
    pub netmask: Ipv4Addr,
    /// Default gateway for this interface (`UNSPECIFIED` if none).
    pub gateway: Ipv4Addr,
    /// DNS server addresses (up to 2; unused slots are `UNSPECIFIED`).
    pub dns: [Ipv4Addr; 2],
    /// Whether the interface is administratively up.
    pub up: bool,
}

impl IfaceConfig {
    /// Compute the broadcast address from `ipv4_addr` and `netmask`.
    ///
    /// `broadcast = ipv4_addr | !netmask`
    #[inline]
    pub fn broadcast(&self) -> Ipv4Addr {
        let addr = self.ipv4_addr.to_u32_be();
        let mask = self.netmask.to_u32_be();
        Ipv4Addr::from_u32_be(addr | !mask)
    }

    /// Returns `true` if `ip` is on the directly connected subnet defined by
    /// this interface's address and netmask.
    #[inline]
    pub fn is_local(&self, ip: Ipv4Addr) -> bool {
        Ipv4Addr::in_subnet(ip, self.ipv4_addr, self.netmask)
    }

    /// Count the number of leading 1-bits in the netmask (prefix length).
    ///
    /// E.g. `255.255.255.0` → 24, `255.255.0.0` → 16.
    #[inline]
    pub fn prefix_len(&self) -> u8 {
        self.netmask.to_u32_be().leading_ones() as u8
    }
}

impl fmt::Debug for IfaceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "IfaceConfig {{ dev={}, ip={}/{}, gw={}, up={} }}",
            self.dev_index,
            self.ipv4_addr,
            self.prefix_len(),
            self.gateway,
            self.up,
        )
    }
}

impl fmt::Display for IfaceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dev{}: {}/{} gw {} dns {}/{}",
            self.dev_index,
            self.ipv4_addr,
            self.prefix_len(),
            self.gateway,
            self.dns[0],
            self.dns[1],
        )
    }
}

// =============================================================================
// 3A.2 — NetStack
// =============================================================================

/// Inner state of the network stack, behind [`SpinLock`].
struct NetStackInner {
    /// Per-interface configurations.  One entry per configured device.
    ifaces: KVec<IfaceConfig>,
}

/// Centralised network stack state — the single source of truth for per-interface
/// IPv4 configuration.
///
/// # Usage
///
/// ```ignore
/// // DHCP lease obtained:
/// NET_STACK.configure(dev, addr, netmask, gateway, [dns1, dns2]);
///
/// // Query our IP for a device:
/// let ip = NET_STACK.our_ip(dev);
///
/// // Check if an address belongs to any of our interfaces:
/// if NET_STACK.is_our_addr(dst_ip) { /* deliver locally */ }
/// ```
pub struct NetStack {
    inner: SpinLock<NetStackInner>,
}

// SAFETY: All mutable state behind SpinLock.

/// The global network stack instance.
pub static NET_STACK: NetStack = NetStack::new();

impl NetStack {
    /// Create an empty network stack.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(
                NetStackInner {
                    ifaces: KVec::new(),
                },
                LOCK_LEVEL_REGISTRY,
            ),
        }
    }

    /// Configure (or reconfigure) an interface with the given IPv4 parameters.
    ///
    /// Called by DHCP on lease acquisition or by static configuration.
    /// If the device already has a config, it is updated in place.
    ///
    /// This will also trigger route table updates here.
    pub fn configure(
        &self,
        dev: DevIndex,
        addr: Ipv4Addr,
        netmask: Ipv4Addr,
        gateway: Ipv4Addr,
        dns: [Ipv4Addr; 2],
    ) {
        let mut inner = self.inner.lock();

        // Update existing config or insert new one.
        if let Some(iface) = inner.ifaces.iter_mut().find(|c| c.dev_index == dev) {
            iface.ipv4_addr = addr;
            iface.netmask = netmask;
            iface.gateway = gateway;
            iface.dns = dns;
            iface.up = true;

            klog_debug!(
                "netstack: updated dev {} -> {}/{}  gw={}",
                dev,
                addr,
                iface.prefix_len(),
                gateway,
            );
        } else {
            let config = IfaceConfig {
                dev_index: dev,
                ipv4_addr: addr,
                netmask,
                gateway,
                dns,
                up: true,
            };

            klog_debug!(
                "netstack: configured dev {} -> {}/{}  gw={}",
                dev,
                addr,
                config.prefix_len(),
                gateway,
            );

            let _ = inner.ifaces.push(config);
        }

        // trigger route table update.
        // Drop the inner lock first — route_table.add() takes its own lock.
        drop(inner);

        // Compute prefix_len from netmask for the connected route.
        let prefix_len = netmask.to_u32_be().leading_ones() as u8;

        // Compute the network prefix (addr & mask).
        let prefix = Ipv4Addr::from_u32_be(addr.to_u32_be() & netmask.to_u32_be());

        // Remove old routes for this device before adding new ones.
        super::route::ROUTE_TABLE.remove_device_routes(dev);

        // Add connected-subnet route.
        super::route::ROUTE_TABLE.add(super::route::RouteEntry {
            prefix,
            prefix_len,
            gateway: Ipv4Addr::UNSPECIFIED, // directly connected
            dev,
            metric: 0,
        });

        // Add default route via gateway (if one was provided).
        if !gateway.is_unspecified() {
            super::route::ROUTE_TABLE.add(super::route::RouteEntry {
                prefix: Ipv4Addr::UNSPECIFIED, // 0.0.0.0/0
                prefix_len: 0,
                gateway,
                dev,
                metric: 100, // lower than host routes, higher metric = less preferred
            });
        }
    }
    /// Look up the interface configuration for a device.
    ///
    /// Returns `None` if the device has not been configured.
    pub fn iface_for_dev(&self, dev: DevIndex) -> Option<IfaceConfig> {
        let inner = self.inner.lock();
        inner.ifaces.iter().find(|c| c.dev_index == dev).copied()
    }

    /// Convenience: return the IPv4 address assigned to `dev`, or `None`.
    pub fn our_ip(&self, dev: DevIndex) -> Option<Ipv4Addr> {
        self.iface_for_dev(dev).map(|c| c.ipv4_addr)
    }

    /// Check if `ip` is assigned to any of our configured interfaces.
    ///
    /// Used by the RX path to decide whether a packet is addressed to us.
    /// Returns `true` for loopback (127.0.0.0/8) unconditionally
    /// adds the loopback device.
    pub fn is_our_addr(&self, ip: Ipv4Addr) -> bool {
        let inner = self.inner.lock();
        inner.ifaces.iter().any(|c| c.ipv4_addr == ip && c.up)
    }

    /// Return the first configured interface's IPv4 address.
    ///
    /// Convenience for the common single-NIC case.  Returns `None` if no
    /// interface has been configured.
    pub fn first_ipv4(&self) -> Option<Ipv4Addr> {
        let inner = self.inner.lock();
        inner
            .ifaces
            .iter()
            .find(|c| c.up && !c.ipv4_addr.is_unspecified())
            .map(|c| c.ipv4_addr)
    }

    /// Return the first configured interface's full config.
    ///
    /// Convenience for DHCP info queries.
    pub fn first_iface(&self) -> Option<IfaceConfig> {
        let inner = self.inner.lock();
        inner.ifaces.iter().find(|c| c.up).copied()
    }

    /// Number of configured interfaces (diagnostic).
    pub fn iface_count(&self) -> usize {
        self.inner.lock().ifaces.len()
    }

    /// Dump all interface configs (diagnostic).
    pub fn dump(&self) {
        let inner = self.inner.lock();
        for iface in &inner.ifaces {
            klog_debug!("  {:?}", iface);
        }
    }
}
