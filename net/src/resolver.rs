//! Resolver configuration: the single authority on which DNS servers to use.
//!
//! The resolver address is a property of the system, not of one NIC's
//! bootstrap: any interface's lease may contribute one, an operator may
//! override one, and [`ResolverConfig::source`] reports where the current one
//! came from.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use slopos_abi::net::{NET_MAX_RESOLVERS, NET_RESOLVER_SRC_DHCP, NET_RESOLVER_SRC_STATIC};

use crate::types::Ipv4Addr;

pub const DEFAULT_TIMEOUT_MS: u32 = 5_000;
pub const DEFAULT_ATTEMPTS: u32 = 2;

/// The servers are `AtomicU32` rather than a locked array so the DNS path can
/// read one without a lock: a query racing a change goes to the previous
/// server, the same as when a server goes away mid-query.
pub struct ResolverConfig {
    servers: [AtomicU32; NET_MAX_RESOLVERS],
    n_servers: AtomicU8,
    source: AtomicU8,
    /// The interface a learned configuration came from.
    source_ifindex: AtomicU32,
    timeout_ms: AtomicU32,
    attempts: AtomicU32,
    /// Set while the configuration is an operator's; a lease may not overwrite
    /// it.
    pinned: AtomicBool,
}

pub static RESOLVER: ResolverConfig = ResolverConfig::new();

impl ResolverConfig {
    pub const fn new() -> Self {
        Self {
            servers: [const { AtomicU32::new(0) }; NET_MAX_RESOLVERS],
            n_servers: AtomicU8::new(0),
            source: AtomicU8::new(NET_RESOLVER_SRC_STATIC),
            source_ifindex: AtomicU32::new(slopos_abi::net::NET_IFINDEX_NONE),
            timeout_ms: AtomicU32::new(DEFAULT_TIMEOUT_MS),
            attempts: AtomicU32::new(DEFAULT_ATTEMPTS),
            pinned: AtomicBool::new(false),
        }
    }

    pub fn primary(&self) -> Option<Ipv4Addr> {
        if self.n_servers.load(Ordering::Acquire) == 0 {
            return None;
        }
        let raw = self.servers[0].load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            Some(Ipv4Addr::from_u32_be(raw))
        }
    }

    /// Copy the configured servers into `out`, returning how many were written.
    pub fn servers(&self, out: &mut [Ipv4Addr]) -> usize {
        let n = (self.n_servers.load(Ordering::Acquire) as usize).min(NET_MAX_RESOLVERS);
        let n = n.min(out.len());
        for (slot, server) in out.iter_mut().take(n).zip(self.servers.iter()) {
            *slot = Ipv4Addr::from_u32_be(server.load(Ordering::Acquire));
        }
        n
    }

    pub fn count(&self) -> usize {
        self.n_servers.load(Ordering::Acquire) as usize
    }

    /// `NET_RESOLVER_SRC_*`.
    pub fn source(&self) -> u8 {
        self.source.load(Ordering::Acquire)
    }

    pub fn source_ifindex(&self) -> u32 {
        self.source_ifindex.load(Ordering::Acquire)
    }

    pub fn timeout_ms(&self) -> u32 {
        self.timeout_ms.load(Ordering::Acquire)
    }

    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Acquire)
    }

    /// Whether an operator's configuration is in force.
    pub fn is_pinned(&self) -> bool {
        self.pinned.load(Ordering::Acquire)
    }

    fn write_servers(&self, servers: &[Ipv4Addr]) {
        let n = servers.len().min(NET_MAX_RESOLVERS);
        for (slot, server) in self.servers.iter().zip(servers.iter().take(n)) {
            slot.store(server.to_u32_be(), Ordering::Release);
        }
        for slot in self.servers.iter().skip(n) {
            slot.store(0, Ordering::Release);
        }
        self.n_servers.store(n as u8, Ordering::Release);
    }

    /// Install an operator's configuration. Outranks anything a lease carries
    /// until [`clear_static`](Self::clear_static).
    pub fn set_static(&self, servers: &[Ipv4Addr], timeout_ms: u32, attempts: u32) {
        self.write_servers(servers);
        self.source
            .store(NET_RESOLVER_SRC_STATIC, Ordering::Release);
        self.source_ifindex
            .store(slopos_abi::net::NET_IFINDEX_NONE, Ordering::Release);
        self.timeout_ms.store(
            if timeout_ms == 0 {
                DEFAULT_TIMEOUT_MS
            } else {
                timeout_ms
            },
            Ordering::Release,
        );
        self.attempts.store(
            if attempts == 0 {
                DEFAULT_ATTEMPTS
            } else {
                attempts
            },
            Ordering::Release,
        );
        self.pinned.store(true, Ordering::Release);
    }

    /// Offer a configuration learned from a lease. Refused, and reported as
    /// refused, while an operator's configuration is pinned.
    pub fn set_from_lease(&self, ifindex: u32, servers: &[Ipv4Addr]) -> bool {
        if self.is_pinned() {
            return false;
        }
        self.write_servers(servers);
        self.source.store(NET_RESOLVER_SRC_DHCP, Ordering::Release);
        self.source_ifindex.store(ifindex, Ordering::Release);
        true
    }

    /// Withdraw a configuration a given interface's lease contributed.
    ///
    /// Keyed on the interface so a second interface's lease is not dropped when
    /// the first one's goes away, and a no-op against a pinned configuration
    /// for the same reason `set_from_lease` is.
    pub fn clear_from_lease(&self, ifindex: u32) {
        if self.is_pinned() || self.source() != NET_RESOLVER_SRC_DHCP {
            return;
        }
        if self.source_ifindex() != ifindex {
            return;
        }
        self.write_servers(&[]);
        self.source_ifindex
            .store(slopos_abi::net::NET_IFINDEX_NONE, Ordering::Release);
    }

    /// Drop the operator's configuration, letting leases apply again.
    pub fn clear_static(&self) {
        self.pinned.store(false, Ordering::Release);
        self.write_servers(&[]);
        self.source
            .store(NET_RESOLVER_SRC_STATIC, Ordering::Release);
    }

    #[cfg(feature = "test-hooks")]
    pub fn reset(&self) {
        self.pinned.store(false, Ordering::Release);
        self.write_servers(&[]);
        self.source
            .store(NET_RESOLVER_SRC_STATIC, Ordering::Release);
        self.source_ifindex
            .store(slopos_abi::net::NET_IFINDEX_NONE, Ordering::Release);
        self.timeout_ms.store(DEFAULT_TIMEOUT_MS, Ordering::Release);
        self.attempts.store(DEFAULT_ATTEMPTS, Ordering::Release);
    }
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub fn primary() -> Option<Ipv4Addr> {
    RESOLVER.primary()
}

/// Deliberately **not** inlined: its caller `dns_resolve` sits 16 bytes under
/// the build's measured frame cap, and folding this lookup's temporaries into
/// that frame exceeds it.
#[inline(never)]
pub fn primary_octets() -> Option<[u8; 4]> {
    RESOLVER.primary().map(|ip| ip.0)
}
