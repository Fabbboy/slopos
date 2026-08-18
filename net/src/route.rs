//! Prefix-length-bucketed routing table for IPv4: 33 buckets, one per prefix
//! length. Lookup walks /32 down to /0, so longest-prefix match is O(32)
//! regardless of route count; within a bucket routes are sorted by metric, so
//! the first match at a length is the best-metric one.

use core::fmt;

use slopos_abi::net::{
    NET_EV_ROUTE_ADDED, NET_EV_ROUTE_REMOVED, NET_IFINDEX_NONE, NET_ROUTE_ORIGIN_KERNEL,
    NET_ROUTE_ORIGIN_STATIC, NetEvent,
};
use slopos_ostd::KVec;
use slopos_ostd::klog_debug;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::{write_array_field, write_init_field};

use super::types::{DevIndex, Ipv4Addr};
use crate::netmon::netmon_post;

const MAX_ROUTES_PER_BUCKET: usize = 16;

#[derive(Clone, Copy)]
pub struct RouteEntry {
    pub prefix: Ipv4Addr,
    /// Prefix length in bits (0–32).
    pub prefix_len: u8,
    /// [`Ipv4Addr::UNSPECIFIED`] means directly connected — no gateway hop.
    pub gateway: Ipv4Addr,
    pub dev: DevIndex,
    /// Lower is preferred; breaks ties among routes at the same prefix length.
    pub metric: u32,
}

impl RouteEntry {
    #[inline]
    pub fn matches(&self, dst: Ipv4Addr) -> bool {
        if self.prefix_len == 0 {
            return true;
        }
        let mask = prefix_len_to_mask(self.prefix_len);
        (dst.to_u32_be() & mask) == (self.prefix.to_u32_be() & mask)
    }

    /// The next-hop address for a destination matching this route: the gateway,
    /// or `dst` itself when the route is directly connected.
    #[inline]
    pub fn next_hop(&self, dst: Ipv4Addr) -> Ipv4Addr {
        if self.gateway.is_unspecified() {
            dst
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

/// Inner state of the routing table, behind [`SpinLock`].
#[derive(slopos_ostd::SlotFields)]
struct RouteTableInner {
    /// Indexed by prefix length: 0 = /0 (default routes), 32 = /32 (host
    /// routes). Each bucket is sorted by metric, lowest first.
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
#[derive(slopos_ostd::SlotFields)]
pub struct RouteTable {
    inner: SpinLock<RouteTableInner>,
}

/// The global routing table.
pub static ROUTE_TABLE: RouteTable = RouteTable::new();

/// Shared by `new` and `init_with`, which build the same logical lock.
const ROUTE_TABLE_CLASS: &slopos_ostd::sync::lock_tracking::LockClassKey =
    slopos_ostd::lock_class!("ROUTE_TABLE", LOCK_LEVEL_REGISTRY);

impl RouteTable {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(RouteTableInner::new(), ROUTE_TABLE_CLASS),
        }
    }

    /// In-place [`Init`] recipe equivalent to [`Self::new`]: the 33-element
    /// `[KVec<RouteEntry>; 33]` would otherwise materialise on the caller's
    /// stack frame.
    pub fn init() -> impl Init<Self, AllocError> {
        let inner_init = init_struct_with(
            |slot: SlotPtr<RouteTableInner>| -> Result<Initialised<RouteTableInner>, AllocError> {
                write_array_field!(slot, buckets, 33, |_| KVec::<RouteEntry>::new());
                Ok(slot.finish())
            },
        );
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    inner,
                    SpinLock::<RouteTableInner>::init_with(ROUTE_TABLE_CLASS, inner_init)
                )?;
                Ok(slot.finish())
            },
        )
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        for bucket in inner.buckets.iter_mut() {
            bucket.clear();
        }
    }

    /// Add a route, replacing any existing route with the same
    /// `(prefix, prefix_len, dev)`.
    ///
    /// Returns `true` if a new route was added, `false` if an existing route
    /// was updated.
    pub fn add(&self, entry: RouteEntry) -> bool {
        let mut inner = self.inner.lock();
        let bucket = &mut inner.buckets[entry.prefix_len as usize];

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
                // Insertion sort rather than `sort_by_key`: the latter pulls in
                // `core::slice::sort::driftsort`, whose 4 KiB aligned-storage
                // stack buffer blows the kernel frame budget.
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

        if bucket.len() >= MAX_ROUTES_PER_BUCKET {
            klog_debug!(
                "route: bucket /{} full ({} routes), dropping add",
                entry.prefix_len,
                bucket.len(),
            );
            return false;
        }

        klog_debug!("route: added {:?}", entry);

        let pos = bucket.partition_point(|r| r.metric <= entry.metric);
        let _ = bucket.insert(pos, entry);
        true
    }

    /// Remove a route matching `(prefix, prefix_len)` — the first match if
    /// several differ by device or metric.
    pub fn remove(&self, prefix: Ipv4Addr, prefix_len: u8) -> bool {
        self.remove_entry(prefix, prefix_len).is_some()
    }

    /// [`remove`](Self::remove), handing back the entry that went so a caller
    /// can describe it after this returns — the removal record needs the
    /// gateway and metric, which are gone once the route is.
    pub fn remove_entry(&self, prefix: Ipv4Addr, prefix_len: u8) -> Option<RouteEntry> {
        let mut inner = self.inner.lock();
        let bucket = &mut inner.buckets[prefix_len as usize];
        let pos = bucket.iter().position(|r| r.prefix == prefix)?;
        let removed = bucket.remove(pos);
        klog_debug!("route: removed {:?}", removed);
        Some(removed)
    }

    /// Remove every route on `dev`, as the DHCP re-lease path does before
    /// reconfiguring an interface.
    pub fn remove_device_routes(&self, dev: DevIndex) -> usize {
        let mut nothing: [RouteEntry; 0] = [];
        self.remove_device_routes_into(dev, &mut nothing).1
    }

    /// Remove all routes associated with a specific device, copying what was
    /// removed into `out`. Returns `(written, removed)`.
    ///
    /// The entries come back rather than being announced from in here because
    /// this runs under the table lock, and a wake site reached from under it
    /// would give the route table an out-edge it does not otherwise have.
    /// `written` falls short of `removed` only for a buffer smaller than one
    /// device's route count, which no caller in this tree supplies — hence
    /// reported rather than asserted.
    pub fn remove_device_routes_into(
        &self,
        dev: DevIndex,
        out: &mut [RouteEntry],
    ) -> (usize, usize) {
        let mut inner = self.inner.lock();
        let mut written = 0usize;
        let mut removed = 0usize;
        for bucket in inner.buckets.iter_mut() {
            bucket.retain(|r| {
                if r.dev != dev {
                    return true;
                }
                if written < out.len() {
                    out[written] = *r;
                    written += 1;
                }
                removed += 1;
                false
            });
        }
        if removed > 0 {
            klog_debug!("route: removed {} routes for dev {}", removed, dev);
        }
        (written, removed)
    }

    /// Longest-prefix-match lookup: the `(DevIndex, next_hop)` of the first
    /// matching route, walking /32 down to /0.
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

    pub fn route_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.buckets.iter().map(|b| b.len()).sum()
    }

    pub fn all_routes(&self) -> KVec<RouteEntry> {
        let inner = self.inner.lock();
        let mut routes = KVec::new();
        for bucket in inner.buckets.iter() {
            routes.extend(bucket.iter().copied());
        }
        routes
    }
}

// The shorthands below announce only once the table lock is gone: a post from
// inside [`RouteTable`] would be a wake site under a `LOCK_LEVEL_REGISTRY`
// lock, and the monitor is deliberately a leaf with no out-edges.

/// Routes reported from a single device removal.
///
/// One connected route per address plus a default is the most any interface in
/// this tree carries, so this is headroom rather than a limit anything reaches.
const REMOVAL_REPORT_SLOTS: usize = MAX_ROUTES_PER_BUCKET;

const UNROUTED: RouteEntry = RouteEntry {
    prefix: Ipv4Addr::UNSPECIFIED,
    prefix_len: 0,
    gateway: Ipv4Addr::UNSPECIFIED,
    dev: DevIndex(0),
    metric: 0,
};

/// The origin a route's own shape implies.
///
/// [`RouteEntry`] does not record who installed it: a route with no gateway is
/// by construction the connected route derived from an address's prefix, and
/// anything else was installed deliberately. [`add_with_origin`] is how a
/// caller that *does* know — the DHCP path — says so.
fn implied_origin(entry: &RouteEntry) -> u8 {
    if entry.gateway.is_unspecified() {
        NET_ROUTE_ORIGIN_KERNEL
    } else {
        NET_ROUTE_ORIGIN_STATIC
    }
}

fn post_route_event(kind: u16, entry: &RouteEntry, origin: u8) {
    // Takes the interface table: legal only because the route table's lock was
    // released before this was called, and neither is ever held across the
    // other.
    let ifindex = crate::iface::get_by_dev(entry.dev)
        .map(|i| i.ifindex)
        .unwrap_or(NET_IFINDEX_NONE);
    netmon_post(
        kind,
        ifindex,
        NetEvent::route_payload(
            entry.prefix.0,
            entry.gateway.0,
            entry.prefix_len,
            origin,
            entry.metric,
        ),
    );
}

/// Install a route in the kernel table and announce it.
///
/// Only a genuinely new route is announced: [`RouteTable::add`] reports `false`
/// for an in-place update and for a full bucket alike, and announcing an add
/// the table refused would mislead a renderer.
pub fn add(entry: RouteEntry) -> bool {
    add_with_origin(entry, implied_origin(&entry))
}

/// [`add`], with the origin stated by a caller that knows it.
pub fn add_with_origin(entry: RouteEntry, origin: u8) -> bool {
    let added = ROUTE_TABLE.add(entry);
    if added {
        post_route_event(NET_EV_ROUTE_ADDED, &entry, origin);
    }
    added
}

/// Withdraw one route from the kernel table and announce it.
pub fn remove(prefix: Ipv4Addr, prefix_len: u8) -> bool {
    match ROUTE_TABLE.remove_entry(prefix, prefix_len) {
        Some(entry) => {
            post_route_event(NET_EV_ROUTE_REMOVED, &entry, implied_origin(&entry));
            true
        }
        None => false,
    }
}

/// Withdraw every route belonging to `dev` and announce each one.
pub fn remove_device_routes(dev: DevIndex) -> usize {
    let mut removed = [UNROUTED; REMOVAL_REPORT_SLOTS];
    let (written, total) = ROUTE_TABLE.remove_device_routes_into(dev, &mut removed);
    for entry in &removed[..written] {
        post_route_event(NET_EV_ROUTE_REMOVED, entry, implied_origin(entry));
    }
    total
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
