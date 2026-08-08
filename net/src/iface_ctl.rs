//! Interface control plane: operations that span more than one table.
//!
//! [`iface`](crate::iface) owns interface rows and nothing else.
//! [`ROUTE_TABLE`](crate::route::ROUTE_TABLE) owns forwarding.
//! [`NEIGHBOR_CACHE`](crate::neighbor::NEIGHBOR_CACHE) owns resolution. Any
//! operation that has to touch two of them lives here, because that is where
//! the ordering rule has to be enforced:
//!
//! > **N-1: a control-plane operation holds at most one network lock at a
//! > time.** Every multi-structure change is a pipeline of independently
//! > locked steps over a snapshot, never a transaction.
//!
//! That is not a stylistic preference. The interface table, the route table and
//! the neighbour cache all sit at `LOCK_LEVEL_REGISTRY`, so nesting any two of
//! them is a same-level lock-order violation the validator will report. Writing
//! each step as "take, mutate, release, then take the next" means the edges the
//! validator could record never exist in the first place.

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

/// Metric for a route derived from an interface's own prefix.
const METRIC_CONNECTED: u32 = 0;
/// Metric for a default route. Higher than any connected route, so a
/// directly reachable destination never goes via the gateway.
const METRIC_DEFAULT: u32 = 100;

/// Assign an IPv4 configuration to an interface: address, connected route, and
/// optionally a default route through `gateway`.
///
/// Used by the DHCP client on a lease and by static configuration. Replaces any
/// previous routes this device owned, so a reconfiguration does not leave the
/// old subnet behind.
///
/// # Locking
///
/// Three separate critical sections, in this order and never nested: the
/// interface table (via [`iface::add_addr`], which releases before returning),
/// then the route table for the withdrawal, then the route table for the two
/// additions. Compare [`iface::source_ip_for`], which takes the route table and
/// *then* the interface table — the two orders coexist safely only because
/// neither ever holds one across the other.
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

    // Withdraw whatever this device had before adding what it has now, so a
    // re-lease onto a different subnet cannot leave the old prefix routed.
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
        // The default route's origin is the address's: this is the one route in
        // the tree whose installer knows whether a lease put it there, and the
        // table itself has nowhere to record that.
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

    // An address and a default route are two of the rungs the classifier reads,
    // so re-evaluate now rather than waiting for the next tick — this is the
    // moment a lease turns a `Local` machine into a connected one.
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
///
/// The route table is the single authority for forwarding, so this reads it
/// rather than caching a copy on the interface: a cached gateway can outlive
/// the route it names.
pub fn default_gateway_for(dev: DevIndex) -> Option<Ipv4Addr> {
    ROUTE_TABLE
        .all_routes()
        .iter()
        .find(|r| r.prefix_len == 0 && r.dev == dev && !r.gateway.is_unspecified())
        .map(|r| r.gateway)
}

/// Apply an interface's administrative intent to the world.
///
/// # The step sequence, and why it is a sequence
///
/// Bringing an interface down touches four tables. Doing it as one transaction
/// would mean holding several `LOCK_LEVEL_REGISTRY` locks at once, which is a
/// same-level ordering violation the validator reports — and worse, it would
/// record edges in both directions, because [`iface::source_ip_for`] already
/// takes the route table and then the interface table. So each step takes one
/// lock, finishes, and releases before the next begins:
///
/// 1. claim the per-interface guard and flip intent — **interface table**
/// 2. resolve the device — **device registry**, released before step 3
/// 3. `set_down()` — the driver's own lock, no network lock held
/// 4. withdraw the device's routes — **route table**
/// 5. flush its neighbours — **neighbour cache**, which returns the queued
///    packets rather than dropping them
/// 6. drop those packets — the **packet pool**, with no network lock held
/// 7. drop DHCP-origin addresses, keep static ones — **interface table**
///
/// Step 6 is not a formality: `PacketBuf::drop` returns the buffer to the pool
/// and takes the pool's lock, so dropping inside step 5 would nest the pool
/// under the neighbour cache for no reason.
///
/// Step 7 keeps a static address because it is the operator's configuration,
/// not the lease's to discard; the lease's own address goes because the lease
/// is what an interface being down invalidates.
///
/// Returns the operational state before and after.
pub fn set_admin_up(ifindex: u32, up: bool) -> Result<(OperState, OperState), IfaceError> {
    let was_up = iface::get(ifindex).map(|i| i.admin_up);
    iface::try_begin_admin(ifindex)?;
    let result = apply_admin(ifindex, up);
    iface::end_admin(ifindex);

    // Announce after the guard is released, so the event means "the sequence
    // finished" rather than "it started" — a subscriber that reacted by issuing
    // another administrative call would otherwise be refused by the guard it
    // was told about. A request that changed nothing is not announced:
    // re-stating the state an operator already had is not an event.
    if let Ok((before, after)) = result {
        if was_up != Some(up) {
            if let Some(info) = iface::get(ifindex) {
                iface::post_iface_changed(&info, before, after);
            }
        }
    }
    // An interface going up or down is a topology change, so what the machine
    // can reach may have changed with it. Last, with every lock released.
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
/// The two callers genuinely differ, and conflating them makes the master
/// switch a one-way door:
///
/// * An **administrative down** is a statement about this interface. The
///   lease was granted to a host reachable on it, so taking the interface
///   down invalidates it and the address goes.
/// * The **master switch** is a statement about the machine. The lease is
///   still time-valid and the server still holds the binding, so keeping the
///   address is what makes `disable; enable` restore the connection instead
///   of stranding the machine — which is also what NetworkManager's
///   `NetworkingEnabled` does.
///
/// Without this distinction, disabling networking on a DHCP-configured system
/// drops the address and nothing ever puts it back, because re-binding is the
/// DHCP client's job and enable does not (and should not) forge a lease.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeasePolicy {
    /// Drop DHCP-origin addresses; keep static ones.
    Invalidate,
    /// Keep every address.
    Keep,
}

/// Make the world match one interface's realisation state.
///
/// Shared by the per-interface path and the master switch, because "this
/// interface should now be carrying traffic" is the same work whichever of the
/// two decided it.
fn realise(info: &iface::Iface, up: bool, lease: LeasePolicy) {
    let dev = info.dev;

    if up {
        if let Some(device) = DEVICE_REGISTRY.device_at(dev) {
            // The registry has already released its lock; `device_at` resolves
            // an owning reference precisely so the driver is never called with
            // it held.
            device.set_up();
        }
        // Re-install the connected route for every address that survived the
        // down. The default route is not restored here: it belongs to whoever
        // learned it, and DHCP re-installs it when it re-binds.
        for addr in info.addrs() {
            route::add(RouteEntry {
                prefix: addr.network(),
                prefix_len: addr.prefix_len,
                gateway: Ipv4Addr::UNSPECIFIED,
                dev,
                metric: METRIC_CONNECTED,
            });
        }
        // An interface that gets its address from a lease needs a client again:
        // the down released its lease, and nothing else will ask for a new one.
        // Without this, `down` then `up` leaves the interface with no address
        // and no way to acquire one.
        if info.dhcp_managed {
            crate::dhcp::start(dev);
        }
        return;
    }

    // Give the lease back **before** the interface goes down. A RELEASE is
    // identified by its source address and carries the address in `ciaddr`;
    // sent after the unbind it names an address this client no longer holds,
    // and a server that cannot match it to a binding keeps the address
    // reserved for the rest of the lease — which is the whole thing RELEASE
    // exists to prevent.
    //
    // Only on an administrative down. The master switch passes
    // `LeasePolicy::Keep` precisely because the lease is still valid and the
    // server still holds the binding.
    //
    // This runs with no driver lock held, and that is load-bearing rather than
    // incidental: the RELEASE is a transmit, so reaching this from under the
    // driver's own lock would re-enter it. `device.set_down()` below is the
    // first thing that takes it.
    if lease == LeasePolicy::Invalidate {
        crate::dhcp::stop_with(dev, true);
    }

    if let Some(device) = DEVICE_REGISTRY.device_at(dev) {
        device.set_down();
    }
    route::remove_device_routes(dev);
    // The packets come back so they are freed here, with the neighbour cache
    // lock already gone. Dropping the vector is what returns them to the pool.
    drop(NEIGHBOR_CACHE.flush_device(dev));
    if lease == LeasePolicy::Invalidate {
        let _ = iface::retain_addrs(info.ifindex, |a| a.origin != AddrOrigin::Dhcp);
    }
}

/// Move the master networking switch.
///
/// Returns `true` if it changed. Realises or unrealises every non-loopback
/// interface to match, **without touching any interface's `admin_up`** — that
/// field is the memory of what the operator asked for, so a disable that wrote
/// it would destroy the very thing the next enable needs to read.
///
/// Loopback is skipped entirely: taking `127.0.0.1` away would break AF_INET
/// localhost IPC that has nothing to do with networking being switched off.
pub fn set_networking_enabled(on: bool) -> bool {
    if !iface::set_enabled_flag(on) {
        return false;
    }

    // Snapshot first: `realise` takes three other locks, so it must not run
    // under the interface table's.
    let mut targets: KVec<iface::Iface> = match KVec::with_capacity(NET_MAX_IFACES) {
        Ok(v) => v,
        // Out of memory here means the gate has moved but nothing was
        // realised. Report the change so the caller emits the event; the next
        // administrative action reconciles.
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
    // Last, so a subscriber that re-queries on this event reads a stack that
    // has already finished moving. The per-interface records the realisation
    // produced arrive first and describe how it moved.
    post_global_enable(on);
    connectivity::recheck();
    true
}

/// Announce the master switch. Addresses the stack rather than an interface,
/// so it carries [`NET_IFINDEX_GLOBAL`].
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
