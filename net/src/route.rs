//! Prefix-length-bucketed routing table for IPv4.
//!
//! # Architecture
//!
//! Routes are stored in a fixed-size array of 33 buckets (one per possible
//! prefix length, /0 through /32).  Lookup iterates from the longest prefix
//! (/32) to the shortest (/0), checking only routes at each prefix length.
//! This gives O(32) worst-case lookup regardless of the total number of routes.
//!
//! Within each bucket, routes are sorted by metric (lowest first) so that the
//! first match at any prefix length is automatically the best-metric route.
//!
//! # Concurrency
//!
//! All mutable state is behind an [`SpinLock`].  The lock is held briefly for
//! lookups and modifications.  For a hobby OS with a single-digit route count
//! this is more than sufficient.
//!
//! # Integration
//!
//! - **DHCP**: calls [`RouteTable::add`] via [`super::netstack::NetStack::configure`]
//!   when a lease is obtained, adding both the connected-subnet route and the
//!   default gateway route.
//! - **IPv4 egress**: calls [`RouteTable::lookup`] to determine the outgoing
//!   device and next-hop address for each packet.
//! - **Loopback**: the `127.0.0.0/8` connected route is added at kernel init.

use core::fmt;

use slopos_ostd::KVec;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_utils::klog_debug;

use super::types::{DevIndex, Ipv4Addr};

// =============================================================================
// 3B.1 — RouteEntry
// =============================================================================

/// Maximum number of routes per prefix-length bucket.
const MAX_ROUTES_PER_BUCKET: usize = 16;

/// A single entry in the routing table.
///
/// Routes are compared by `(prefix, prefix_len)` for equality and sorted by
/// `metric` within a bucket.
#[derive(Clone, Copy)]
pub struct RouteEntry {
    /// Network prefix (e.g. `192.168.1.0` for a /24 route).
    pub prefix: Ipv4Addr,
    /// Prefix length in bits (0–32).
    pub prefix_len: u8,
    /// Gateway address.  [`Ipv4Addr::UNSPECIFIED`] means directly connected —
    /// the destination is on the local subnet and no gateway hop is needed.
    pub gateway: Ipv4Addr,
    /// Outgoing device index.
    pub dev: DevIndex,
    /// Route metric (lower = preferred).  Used to break ties when multiple
    /// routes match at the same prefix length.
    pub metric: u32,
}

impl RouteEntry {
    /// Returns `true` if `dst` falls within this route's prefix/prefix_len.
    ///
    /// The check masks both `dst` and `self.prefix` with a mask derived from
    /// `self.prefix_len` and compares.  A prefix_len of 0 matches everything
    /// (default route).
    #[inline]
    pub fn matches(&self, dst: Ipv4Addr) -> bool {
        if self.prefix_len == 0 {
            return true; // default route matches everything
        }
        let mask = prefix_len_to_mask(self.prefix_len);
        (dst.to_u32_be() & mask) == (self.prefix.to_u32_be() & mask)
    }

    /// Returns the next-hop address for a destination matching this route.
    ///
    /// - If `gateway` is non-zero (not `UNSPECIFIED`): return the gateway
    ///   (the packet must be forwarded through the gateway).
    /// - Otherwise: return `dst` itself (directly connected subnet — the
    ///   destination is the next hop).
    #[inline]
    pub fn next_hop(&self, dst: Ipv4Addr) -> Ipv4Addr {
        if self.gateway.is_unspecified() {
            dst // directly connected
        } else {
            self.gateway
        }
    }
}

impl fmt::Debug for RouteEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.gateway.is_unspecified() {
            write!(
                f,
                "{}/{} dev {} metric {} (connected)",
                self.prefix, self.prefix_len, self.dev, self.metric
            )
        } else {
            write!(
                f,
                "{}/{} via {} dev {} metric {}",
                self.prefix, self.prefix_len, self.gateway, self.dev, self.metric
            )
        }
    }
}

impl fmt::Display for RouteEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// =============================================================================
// 3B.2 — RouteTable
// =============================================================================

/// Inner state of the routing table, behind [`SpinLock`].
struct RouteTableInner {
    /// Routes bucketed by prefix length.  Index 0 = /0 (default routes),
    /// index 32 = /32 (host routes).  Within each bucket, routes are sorted
    /// by metric (lowest first).
    buckets: [KVec<RouteEntry>; 33],
}

impl RouteTableInner {
    const fn new() -> Self {
        Self {
            buckets: [const { KVec::new() }; 33],
        }
    }
}

/// Prefix-length-bucketed IPv4 routing table with longest-prefix-match lookup.
///
/// See [module documentation](self) for architecture details.
pub struct RouteTable {
    inner: SpinLock<RouteTableInner>,
}

// SAFETY: All mutable state is behind SpinLock.

/// The global routing table.
pub static ROUTE_TABLE: RouteTable = RouteTable::new();

impl RouteTable {
    /// Create an empty routing table.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(RouteTableInner::new(), LOCK_LEVEL_REGISTRY),
        }
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        for bucket in inner.buckets.iter_mut() {
            bucket.clear();
        }
    }

    /// Add a route to the table.
    ///
    /// The route is inserted into `buckets[prefix_len]`, sorted by metric
    /// (lowest first).  If a route with the same `(prefix, prefix_len, dev)`
    /// already exists, it is replaced.
    ///
    /// Returns `true` if a new route was added, `false` if an existing route
    /// was updated.
    pub fn add(&self, entry: RouteEntry) -> bool {
        let mut inner = self.inner.lock();
        let bucket = &mut inner.buckets[entry.prefix_len as usize];

        // Check for existing route with same prefix+dev — update in place.
        for existing in bucket.iter_mut() {
            if existing.prefix == entry.prefix && existing.dev == entry.dev {
                klog_debug!(
                    "route: updated {:?} (metric {} -> {})",
                    entry,
                    existing.metric,
                    entry.metric,
                );
                existing.gateway = entry.gateway;
                existing.metric = entry.metric;
                // Re-sort by metric after update. Buckets are capped at
                // MAX_ROUTES_PER_BUCKET (16), so an insertion sort is
                // both correct and cheaper than `sort_by_key` — the
                // latter pulls in core::slice::sort::driftsort, which
                // carries a 4 KiB aligned-storage stack buffer that
                // blows the kernel frame budget.
                for i in 1..bucket.len() {
                    let mut j = i;
                    while j > 0 && bucket[j - 1].metric > bucket[j].metric {
                        bucket.swap(j - 1, j);
                        j -= 1;
                    }
                }
                return false;
            }
        }

        // Enforce per-bucket limit.
        if bucket.len() >= MAX_ROUTES_PER_BUCKET {
            klog_debug!(
                "route: bucket /{} full ({} routes), dropping add",
                entry.prefix_len,
                bucket.len(),
            );
            return false;
        }

        klog_debug!("route: added {:?}", entry);

        // Insert sorted by metric.
        let pos = bucket.partition_point(|r| r.metric <= entry.metric);
        let _ = bucket.insert(pos, entry);
        true
    }

    /// Remove a route matching `(prefix, prefix_len)`.
    ///
    /// If multiple routes match (different devices/metrics), removes the first
    /// match.  Returns `true` if a route was removed.
    pub fn remove(&self, prefix: Ipv4Addr, prefix_len: u8) -> bool {
        let mut inner = self.inner.lock();
        let bucket = &mut inner.buckets[prefix_len as usize];
        if let Some(pos) = bucket.iter().position(|r| r.prefix == prefix) {
            let removed = bucket.remove(pos);
            klog_debug!("route: removed {:?}", removed);
            true
        } else {
            false
        }
    }

    /// Remove all routes associated with a specific device.
    ///
    /// Called before reconfiguring an interface (e.g., DHCP re-lease).
    pub fn remove_device_routes(&self, dev: DevIndex) {
        let mut inner = self.inner.lock();
        let mut count = 0usize;
        for bucket in inner.buckets.iter_mut() {
            let before = bucket.len();
            bucket.retain(|r| r.dev != dev);
            count += before - bucket.len();
        }
        if count > 0 {
            klog_debug!("route: removed {} routes for dev {}", count, dev);
        }
    }

    /// Longest-prefix-match lookup.
    ///
    /// Iterates from /32 (host routes) down to /0 (default routes).  Returns
    /// the `(DevIndex, next_hop)` for the first matching route.
    ///
    /// O(32) worst case — one bucket check per possible prefix length,
    /// regardless of the total number of routes in the table.
    pub fn lookup(&self, dst: Ipv4Addr) -> Option<(DevIndex, Ipv4Addr)> {
        let inner = self.inner.lock();
        for prefix_len in (0..=32u8).rev() {
            for route in &inner.buckets[prefix_len as usize] {
                if route.matches(dst) {
                    return Some((route.dev, route.next_hop(dst)));
                }
            }
        }
        None
    }

    /// Number of routes in the table (diagnostic).
    pub fn route_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.buckets.iter().map(|b| b.len()).sum()
    }

    /// Dump all routes for debugging.
    pub fn dump(&self) {
        let inner = self.inner.lock();
        for (prefix_len, bucket) in inner.buckets.iter().enumerate() {
            for route in bucket {
                klog_debug!("  /{}: {:?}", prefix_len, route);
            }
        }
    }

    /// Collect all routes into a Vec (for ifconfig/diagnostic display).
    pub fn all_routes(&self) -> KVec<RouteEntry> {
        let inner = self.inner.lock();
        let mut routes = KVec::new();
        for bucket in inner.buckets.iter() {
            routes.extend(bucket.iter().copied());
        }
        routes
    }
}

// =============================================================================
// Helper: prefix length → mask
// =============================================================================

/// Convert a prefix length (0–32) to a u32 network mask in host byte order.
///
/// E.g. `prefix_len_to_mask(24)` → `0xFFFFFF00`.
#[inline]
fn prefix_len_to_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else if prefix_len >= 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix_len)
    }
}
