//! Interface control plane: operations spanning the interface table, the route
//! table and the neighbour cache.
//!
//! > **N-1: a control-plane operation holds at most one network lock at a
//! > time.** Every multi-structure change is a pipeline of independently
//! > locked steps over a snapshot, never a transaction.
//!
//! All three sit at `LOCK_LEVEL_REGISTRY`, so nesting any two is a same-level
//! lock-order violation.

use slopos_abi::net::{
    NET_EV_GLOBAL_ENABLE, NET_IFINDEX_GLOBAL, NET_MAX_IFACES, NET_ROUTE_ORIGIN_DHCP,
    NET_ROUTE_ORIGIN_STATIC, NetEvent,
};
use slopos_ostd::KVec;

use crate::connectivity;
use crate::iface::{
    self, AddrOrigin, AddrScope, IfaceAddr, IfaceError, IfaceKind, OperState, prefix_to_mask,
};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::DEVICE_REGISTRY;
use crate::netmon::netmon_post;
use crate::route::{self, ROUTE_TABLE, RouteEntry};
use crate::types::{DevIndex, Ipv4Addr};

const METRIC_CONNECTED: u32 = 0;
/// Higher than any connected route, so a directly reachable destination never
/// goes via the gateway.
const METRIC_DEFAULT: u32 = 100;

/// Assign an IPv4 configuration to an interface: address, connected route, and
/// optionally a default route through `gateway`.
///
/// Replaces any previous routes this device owned, so a reconfiguration does
/// not leave the old subnet behind.
///
/// # Locking
///
/// Three separate critical sections, never nested: the interface table, then
/// the route table for the withdrawal, then for the additions.
pub fn configure_ipv4(
    dev: DevIndex,
    addr: Ipv4Addr,
    prefix_len: u8,
    gateway: Ipv4Addr,
    origin: AddrOrigin,
) -> Result<u32, IfaceError> {
    let ifindex = iface::get_by_dev(dev)
        .map(|i| i.ifindex)
        .ok_or(IfaceError::NoSuchIface)?;

    iface::add_addr(
        ifindex,
        IfaceAddr::permanent(addr, prefix_len, AddrScope::Global, origin),
    )?;

    // A re-lease onto a different subnet must not leave the old prefix routed.
    route::remove_device_routes(dev);

    let mask = prefix_to_mask(prefix_len);
    route::add(RouteEntry {
        prefix: Ipv4Addr::from_u32_be(addr.to_u32_be() & mask),
        prefix_len,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: METRIC_CONNECTED,
    });

    if !gateway.is_unspecified() {
        // Only this installer knows whether a lease put the default route there.
        route::add_with_origin(
            RouteEntry {
                prefix: Ipv4Addr::UNSPECIFIED,
                prefix_len: 0,
                gateway,
                dev,
                metric: METRIC_DEFAULT,
            },
            route_origin_of(origin),
        );
    }

    connectivity::recheck();

    Ok(ifindex)
}

/// The `NET_ROUTE_ORIGIN_*` that matches an address's origin.
fn route_origin_of(origin: AddrOrigin) -> u8 {
    match origin {
        AddrOrigin::Dhcp => NET_ROUTE_ORIGIN_DHCP,
        AddrOrigin::Static | AddrOrigin::LinkLocal => NET_ROUTE_ORIGIN_STATIC,
    }
}

/// The gateway of the default route currently installed for `dev`, if any.
pub fn default_gateway_for(dev: DevIndex) -> Option<Ipv4Addr> {
    ROUTE_TABLE
        .all_routes()
        .iter()
        .find(|r| r.prefix_len == 0 && r.dev == dev && !r.gateway.is_unspecified())
        .map(|r| r.gateway)
}

/// Apply an interface's administrative intent to the world.
///
/// Returns the operational state before and after.
pub fn set_admin_up(ifindex: u32, up: bool) -> Result<(OperState, OperState), IfaceError> {
    let was_up = iface::get(ifindex).map(|i| i.admin_up);
    iface::try_begin_admin(ifindex)?;
    let result = apply_admin(ifindex, up);
    iface::end_admin(ifindex);

    // After the guard is released, so a subscriber reacting with another
    // administrative call is not refused by the guard it was told about. A
    // request that changed nothing is not announced.
    if let Ok((before, after)) = result {
        if was_up != Some(up) {
            if let Some(info) = iface::get(ifindex) {
                iface::post_iface_changed(&info, before, after);
            }
        }
    }
    connectivity::recheck();
    result
}

fn apply_admin(ifindex: u32, up: bool) -> Result<(OperState, OperState), IfaceError> {
    let (before, after) = iface::set_admin_intent(ifindex, up)?;
    let Some(info) = iface::get(ifindex) else {
        return Err(IfaceError::NoSuchIface);
    };
    realise(&info, up, LeasePolicy::Invalidate);
    Ok((before, after))
}

/// What an unrealisation does to a DHCP-assigned address.
///
/// An administrative down invalidates the lease. The master switch does not:
/// the binding is still valid and only the DHCP client can re-request one, so
/// `disable; enable` would otherwise strand the machine with no address.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeasePolicy {
    /// Drop DHCP-origin addresses; keep static ones.
    Invalidate,
    Keep,
}

/// Make the world match one interface's realisation state.
fn realise(info: &iface::Iface, up: bool, lease: LeasePolicy) {
    let dev = info.dev;

    if up {
        if let Some(device) = DEVICE_REGISTRY.device_at(dev) {
            device.set_up();
        }
        // The default route is not restored here: it belongs to whoever learned
        // it, and DHCP re-installs it when it re-binds.
        for addr in info.addrs() {
            route::add(RouteEntry {
                prefix: addr.network(),
                prefix_len: addr.prefix_len,
                gateway: Ipv4Addr::UNSPECIFIED,
                dev,
                metric: METRIC_CONNECTED,
            });
        }
        // The down released the lease and nothing else will ask for a new one.
        if info.dhcp_managed {
            crate::dhcp::start(dev);
        }
        return;
    }

    // Before the interface goes down: a RELEASE is matched by source address
    // and `ciaddr`, so one sent after the unbind names an address this client
    // no longer holds and the server keeps it reserved for the rest of the
    // lease. It is also a transmit, so it must stay above the `set_down()`
    // below, which is the first thing to take the driver's lock.
    if lease == LeasePolicy::Invalidate {
        crate::dhcp::stop_with(dev, true);
    }

    if let Some(device) = DEVICE_REGISTRY.device_at(dev) {
        device.set_down();
    }
    route::remove_device_routes(dev);
    // The queued packets are freed here rather than inside the flush: their
    // drop takes the packet pool's lock, which must not nest under the cache's.
    drop(NEIGHBOR_CACHE.flush_device(dev));
    if lease == LeasePolicy::Invalidate {
        let _ = iface::retain_addrs(info.ifindex, |a| a.origin != AddrOrigin::Dhcp);
    }
}

/// Move the master networking switch. Returns `true` if it changed.
///
/// Realises every non-loopback interface to match **without touching any
/// interface's `admin_up`**: that field is the memory of what the operator
/// asked for, which the next enable has to read. Loopback is skipped so AF_INET
/// localhost IPC survives networking being switched off.
pub fn set_networking_enabled(on: bool) -> bool {
    if !iface::set_enabled_flag(on) {
        return false;
    }

    // Snapshot first: `realise` takes other locks, so it must not run under the
    // interface table's.
    let mut targets: KVec<iface::Iface> = match KVec::with_capacity(NET_MAX_IFACES) {
        Ok(v) => v,
        // The gate has moved but nothing was realised; the next administrative
        // action reconciles.
        Err(_) => {
            post_global_enable(on);
            return true;
        }
    };
    iface::for_each(|i| {
        if !matches!(i.kind, IfaceKind::Loopback) && i.admin_up {
            let _ = targets.push(*i);
        }
    });

    for info in targets.iter() {
        realise(info, on, LeasePolicy::Keep);
    }
    // Last, so a subscriber that re-queries on this event reads a settled stack.
    post_global_enable(on);
    connectivity::recheck();
    true
}

fn post_global_enable(on: bool) {
    netmon_post(
        NET_EV_GLOBAL_ENABLE,
        NET_IFINDEX_GLOBAL,
        NetEvent::u32_payload(on as u32),
    );
}

/// The system default route as `(ifindex, gateway)`, if one is installed.
pub fn default_route() -> Option<(u32, Ipv4Addr)> {
    let routes = ROUTE_TABLE.all_routes();
    let best = routes
        .iter()
        .filter(|r| r.prefix_len == 0)
        .min_by_key(|r| r.metric)?;
    let ifindex = iface::get_by_dev(best.dev).map(|i| i.ifindex)?;
    Some((ifindex, best.gateway))
}
