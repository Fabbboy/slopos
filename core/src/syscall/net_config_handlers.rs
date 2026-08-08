//! Network-configuration syscall handlers.
//!
//! Synchronous; the work lives in `slopos-net`. These only marshal the caller's
//! process id and map the result to an errno.

use slopos_abi::Errno;
use slopos_abi::net::{
    NET_ADDROP_ADD, NET_ADDROP_DEL, NET_IFINDEX_GLOBAL, NET_IFOP_CONN_RECHECK, NET_IFOP_DEL_NEIGH,
    NET_IFOP_DHCP_RELEASE, NET_IFOP_DHCP_RENEW, NET_IFOP_DHCP_START, NET_IFOP_DHCP_STOP,
    NET_IFOP_FLUSH_ADDRS, NET_IFOP_FLUSH_NEIGH, NET_IFOP_SET_ADMIN_UP, NET_IFOP_SET_ENABLED,
    NET_IFOP_SET_MTU, NET_ROUTEOP_ADD, NET_ROUTEOP_DEL, UserAddrReq, UserResolverReq, UserRouteReq,
};
use slopos_net::iface::{AddrOrigin, AddrScope, IfaceAddr, IfaceError};
use slopos_net::neighbor::NEIGHBOR_CACHE;
use slopos_net::route;
use slopos_net::types::{DevIndex, Ipv4Addr};
use slopos_net::{iface, iface_ctl};

/// The errno each control-plane failure reports, as `SYSCALL_NET_IFACE_CTL`'s
/// contract specifies. Written once so no operation can spell one differently.
fn iface_errno(err: IfaceError) -> Errno {
    match err {
        IfaceError::NoSuchIface => Errno::ENODEV,
        IfaceError::Busy => Errno::EBUSY,
        IfaceError::Invalid => Errno::EINVAL,
        IfaceError::TooManyAddrs => Errno::ENOSPC,
        IfaceError::NotFound => Errno::ENOENT,
        IfaceError::NoSpace => Errno::ENOMEM,
    }
}

/// Resolve an interface index to the device behind it.
fn device_for(ifindex: u32) -> Result<DevIndex, Errno> {
    iface::get(ifindex).map(|i| i.dev).ok_or(Errno::ENODEV)
}

define_syscall!(syscall_net_monitor
    (ctx, mask: u32, flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let _ = flags; // reserved
    let fd = slopos_net::netmon_file_ops::netmon_create(process_id, mask);
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});

define_syscall!(syscall_net_iface_ctl
    (ctx, ifindex: u32, op: u32, arg: u64)
    requires(net_admin)
    -> Result<(), Errno>
{
    // The two global operations address the stack rather than an interface, so
    // they are answered before anything tries to resolve an index. Requiring
    // the sentinel rather than ignoring the index is what keeps
    // `ip link set eth0 <global op>` from reading as a per-interface command
    // that quietly moved the master switch.
    match op {
        NET_IFOP_SET_ENABLED => {
            if ifindex != NET_IFINDEX_GLOBAL {
                return Err(Errno::EINVAL);
            }
            iface_ctl::set_networking_enabled(arg != 0);
            return Ok(());
        }
        NET_IFOP_CONN_RECHECK => {
            if ifindex != NET_IFINDEX_GLOBAL {
                return Err(Errno::EINVAL);
            }
            // Synchronous on purpose: a caller asking for a re-check wants the
            // answer its next `net_query` returns to reflect this call, not the
            // next timer tick.
            slopos_net::connectivity::recheck();
            return Ok(());
        }
        NET_IFOP_DHCP_START => {
            let dev = device_for(ifindex)?;
            if !slopos_net::dhcp::start(dev) {
                return Err(Errno::ENOMEM);
            }
            return Ok(());
        }
        // Stop and release are the same operation: a client that gives up its
        // lease without telling the server leaves the address unusable to
        // everybody else until it times out, so the RELEASE is not optional.
        NET_IFOP_DHCP_STOP | NET_IFOP_DHCP_RELEASE => {
            let dev = device_for(ifindex)?;
            if !slopos_net::dhcp::is_running(dev) {
                return Err(Errno::ENOENT);
            }
            slopos_net::dhcp::stop(dev);
            return Ok(());
        }
        NET_IFOP_DHCP_RENEW => {
            let dev = device_for(ifindex)?;
            if !slopos_net::dhcp::is_running(dev) {
                return Err(Errno::ENOENT);
            }
            slopos_net::dhcp::renew_now(dev);
            return Ok(());
        }
        _ => {}
    }

    match op {
        NET_IFOP_SET_ADMIN_UP => {
            iface_ctl::set_admin_up(ifindex, arg != 0).map_err(iface_errno)?;
            Ok(())
        }
        NET_IFOP_SET_MTU => {
            let mtu = u16::try_from(arg).map_err(|_| Errno::EINVAL)?;
            iface::set_mtu(ifindex, mtu).map_err(iface_errno)
        }
        NET_IFOP_FLUSH_NEIGH => {
            let dev = device_for(ifindex)?;
            // The cache hands the queued packets back rather than dropping them
            // itself, because `PacketBuf::drop` takes the packet pool's lock and
            // freeing them inside would nest the pool under the cache. Dropping
            // the vector here is the free, with no network lock held.
            drop(NEIGHBOR_CACHE.flush_device(dev));
            Ok(())
        }
        NET_IFOP_DEL_NEIGH => {
            let dev = device_for(ifindex)?;
            let ip = Ipv4Addr::from_u32_be(arg as u32);
            let orphans = NEIGHBOR_CACHE.remove(dev, ip).ok_or(Errno::ENOENT)?;
            drop(orphans);
            Ok(())
        }
        NET_IFOP_FLUSH_ADDRS => {
            let dev = device_for(ifindex)?;
            // Addresses first, then the routes derived from them: a connected
            // route outliving its address would forward onto a prefix the
            // interface no longer answers for.
            iface::retain_addrs(ifindex, |_| false).map_err(iface_errno)?;
            route::remove_device_routes(dev);
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
});

// Add or remove one interface address.
//
// Takes exactly one struct and checks `len` against its size, which is why the
// address, route and resolver mutators are three syscalls rather than one
// multiplexed one: each has a single unambiguous shape, so no handler ever
// decides how to reinterpret user memory from an op code.
define_syscall!(syscall_net_addr_ctl
    (ctx, op: u32, ptr: u64, len: u64)
    requires(net_admin)
    -> Result<(), Errno>
{
    if len as usize != core::mem::size_of::<UserAddrReq>() {
        return Err(Errno::EINVAL);
    }
    let user = slopos_mm::user_ptr::UserPtr::<UserAddrReq>::try_new(ptr)
        .map_err(|_| Errno::EFAULT)?;
    let req = slopos_mm::user_copy::copy_from_user(user).map_err(|_| Errno::EFAULT)?;

    if req.family != slopos_abi::net::AF_INET as u8 || req.prefix_len > 32 {
        return Err(Errno::EINVAL);
    }
    let addr = Ipv4Addr(req.addr);

    match op {
        NET_ADDROP_ADD => {
            let scope = match req.scope {
                slopos_abi::net::NET_ADDR_SCOPE_GLOBAL => AddrScope::Global,
                slopos_abi::net::NET_ADDR_SCOPE_LINK => AddrScope::Link,
                slopos_abi::net::NET_ADDR_SCOPE_HOST => AddrScope::Host,
                _ => return Err(Errno::EINVAL),
            };
            // Always `Static`: an address a caller asked for outranks a lease,
            // and admin-down keeps Static addresses while dropping DHCP ones.
            // Recording the caller's own origin byte would let userland forge a
            // DHCP address that the next admin-down would silently delete.
            iface::add_addr(
                req.ifindex,
                IfaceAddr::permanent(addr, req.prefix_len, scope, AddrOrigin::Static),
            )
            .map_err(iface_errno)?;
            // A new address implies the prefix it sits on is directly reachable;
            // without this the address is configured and nothing routes to its
            // own subnet.
            let dev = device_for(req.ifindex)?;
            let prefix = addr.masked(req.prefix_len);
            route::add(route::RouteEntry {
                prefix,
                prefix_len: req.prefix_len,
                gateway: Ipv4Addr::UNSPECIFIED,
                dev,
                metric: 0,
            });
            Ok(())
        }
        NET_ADDROP_DEL => {
            iface::del_addr(req.ifindex, addr, req.prefix_len).map_err(iface_errno)?;
            // The connected route goes with it, for the reason
            // `NET_IFOP_FLUSH_ADDRS` gives: a route onto a prefix the interface
            // no longer answers for is a black hole.
            route::remove(addr.masked(req.prefix_len), req.prefix_len);
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
});

// Add or remove one route.
define_syscall!(syscall_net_route_ctl
    (ctx, op: u32, ptr: u64, len: u64)
    requires(net_admin)
    -> Result<(), Errno>
{
    if len as usize != core::mem::size_of::<UserRouteReq>() {
        return Err(Errno::EINVAL);
    }
    let user = slopos_mm::user_ptr::UserPtr::<UserRouteReq>::try_new(ptr)
        .map_err(|_| Errno::EFAULT)?;
    let req = slopos_mm::user_copy::copy_from_user(user).map_err(|_| Errno::EFAULT)?;

    if req.prefix_len > 32 {
        return Err(Errno::EINVAL);
    }
    let prefix = Ipv4Addr(req.prefix);

    match op {
        NET_ROUTEOP_ADD => {
            let dev = device_for(req.ifindex)?;
            // The prefix is normalised rather than trusted: `10.0.2.15/24` names
            // the same route as `10.0.2.0/24`, and storing the un-masked form
            // would make the delete of one fail to find the other.
            let entry = route::RouteEntry {
                prefix: prefix.masked(req.prefix_len),
                prefix_len: req.prefix_len,
                gateway: Ipv4Addr(req.gateway),
                dev,
                metric: req.metric,
            };
            if route::add(entry) { Ok(()) } else { Err(Errno::ENOSPC) }
        }
        NET_ROUTEOP_DEL => {
            if route::remove(prefix.masked(req.prefix_len), req.prefix_len) {
                Ok(())
            } else {
                Err(Errno::ENOENT)
            }
        }
        _ => Err(Errno::EINVAL),
    }
});

// Pin the resolver configuration, or hand it back to DHCP.
//
// `n_servers == 0` clears the static override rather than configuring zero
// nameservers, so `ip dns set` with no argument is the documented way back to
// lease-supplied servers.
define_syscall!(syscall_net_resolver_set
    (ctx, ptr: u64, len: u64)
    requires(net_admin)
    -> Result<(), Errno>
{
    if len as usize != core::mem::size_of::<UserResolverReq>() {
        return Err(Errno::EINVAL);
    }
    let user = slopos_mm::user_ptr::UserPtr::<UserResolverReq>::try_new(ptr)
        .map_err(|_| Errno::EFAULT)?;
    let req = slopos_mm::user_copy::copy_from_user(user).map_err(|_| Errno::EFAULT)?;

    let count = req.n_servers as usize;
    if count > req.servers.len() {
        return Err(Errno::EINVAL);
    }
    if count == 0 {
        slopos_net::resolver::RESOLVER.clear_static();
        return Ok(());
    }

    let mut servers = [Ipv4Addr::UNSPECIFIED; slopos_abi::net::NET_MAX_RESOLVERS];
    for (slot, raw) in servers.iter_mut().zip(req.servers.iter()).take(count) {
        *slot = Ipv4Addr(*raw);
    }
    // A nameserver of 0.0.0.0 is never reachable; accepting one would leave the
    // stack pointed at an address it can only time out against.
    if servers[..count].iter().any(|s| s.is_unspecified()) {
        return Err(Errno::EINVAL);
    }
    slopos_net::resolver::RESOLVER.set_static(&servers[..count], req.timeout_ms, req.attempts);
    Ok(())
});
