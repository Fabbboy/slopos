//! `net_query` — enumerate one class of network state.
//!
//! Every `what` answers with a [`UserNetQueryHdr`] followed by an array of one
//! fixed-size record. `copy_to_user` can fault and the allocator must not be
//! reached under a cli-disabling lock, so each query allocates its staging
//! vector first, fills it under the lock, then copies out after dropping it.

use slopos_abi::Errno;
use slopos_abi::net::{
    NET_IFINDEX_NONE, NET_Q_ADDRS, NET_Q_DHCP, NET_Q_GLOBAL, NET_Q_IFACES, NET_Q_NEIGH,
    NET_Q_RESOLVER, NET_Q_ROUTES, NET_Q_SOCKETS, UserAddr, UserDhcpStatus, UserIface, UserNeigh,
    UserNetGlobal, UserNetQueryHdr, UserResolver, UserRoute, UserSockInfo,
};
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_fs::fileio::FdTable;
use slopos_mm::user_copy::copy_to_user;
use slopos_net::iface::{self, Iface, IfaceAddr};
use slopos_net::neighbor::NEIGHBOR_CACHE;
use slopos_net::resolver::RESOLVER;
use slopos_net::route::ROUTE_TABLE;
use slopos_ostd::KVec;

fn write_record<T: Copy>(base: u64, index: usize, value: &T) -> Result<(), Errno> {
    let offset = (index * core::mem::size_of::<T>()) as u64;
    let addr = base.checked_add(offset).ok_or(Errno::EFAULT)?;
    let ptr = slopos_mm::user_ptr::UserPtr::<T>::try_new(addr).map_err(|_| Errno::EFAULT)?;
    copy_to_user(ptr, value).map_err(|_| Errno::EFAULT)
}

/// Write the header and return the offset the records start at.
fn write_header(
    buf: u64,
    len: usize,
    what: u32,
    record_size: usize,
    record_count: usize,
    total_count: usize,
) -> Result<u64, Errno> {
    if len < core::mem::size_of::<UserNetQueryHdr>() {
        return Err(Errno::EINVAL);
    }
    let mut hdr = UserNetQueryHdr::default();
    hdr.seq = slopos_net::netseq::net_seq();
    hdr.record_size = record_size as u32;
    hdr.record_count = record_count as u32;
    hdr.total_count = total_count as u32;
    hdr.what = what;

    let ptr =
        slopos_mm::user_ptr::UserPtr::<UserNetQueryHdr>::try_new(buf).map_err(|_| Errno::EFAULT)?;
    copy_to_user(ptr, &hdr).map_err(|_| Errno::EFAULT)?;
    Ok(buf + core::mem::size_of::<UserNetQueryHdr>() as u64)
}

/// How many records of `size` fit in what is left after the header.
fn capacity_for(len: usize, size: usize) -> usize {
    len.saturating_sub(core::mem::size_of::<UserNetQueryHdr>()) / size
}

fn render_iface(src: &Iface, enabled: bool) -> UserIface {
    let stats = slopos_net::DEVICE_REGISTRY
        .stats_by_index(src.dev)
        .unwrap_or_default();

    // Field by field from a zeroed value, not a struct literal: the ABI has no
    // implicit padding, so no uninitialised byte can reach user space.
    let mut out = UserIface::default();
    out.ifindex = src.ifindex;
    out.flags = src.flags(enabled);
    out.mtu = src.mtu as u32;
    out.kind = src.kind.to_abi();
    out.oper_state = src.oper_state(enabled).to_abi();
    out.carrier = u8::from(src.carrier);
    out.admin_up = u8::from(src.admin_up);
    out.name = src.name.raw();
    out.mac = src.mac.0;
    out.rx_packets = stats.rx_packets;
    out.tx_packets = stats.tx_packets;
    out.rx_bytes = stats.rx_bytes;
    out.tx_bytes = stats.tx_bytes;
    out.rx_errors = stats.rx_errors;
    out.tx_errors = stats.tx_errors;
    out.rx_dropped = stats.rx_dropped;
    out.tx_dropped = stats.tx_dropped;
    out
}

fn render_addr(ifindex: u32, src: &IfaceAddr) -> UserAddr {
    let mut out = UserAddr::default();
    out.ifindex = ifindex;
    out.addr = src.addr.0;
    out.prefix_len = src.prefix_len;
    out.family = slopos_abi::net::AF_INET as u8;
    out.scope = src.scope as u8;
    out.origin = src.origin as u8;
    // Absolute deadlines internally, remaining seconds on the wire.
    out.valid_lft_s = remaining_secs(src.valid_until_ms);
    out.pref_lft_s = remaining_secs(src.pref_until_ms);
    out
}

fn remaining_secs(until_ms: u64) -> u32 {
    if until_ms == u64::MAX {
        return slopos_abi::net::NET_LFT_FOREVER;
    }
    let now = slopos_net::clock::now_ms();
    ((until_ms.saturating_sub(now)) / 1000).min(u32::MAX as u64 - 1) as u32
}

fn render_route(src: &slopos_net::RouteEntry) -> UserRoute {
    let mut out = UserRoute::default();
    out.prefix = src.prefix.0;
    out.gateway = src.gateway.0;
    out.prefix_len = src.prefix_len;
    // The table records no origin; a connected route is the one with no gateway.
    out.origin = if src.gateway.is_unspecified() {
        slopos_abi::net::NET_ROUTE_ORIGIN_KERNEL
    } else {
        slopos_abi::net::NET_ROUTE_ORIGIN_DHCP
    };
    out.ifindex = iface::get_by_dev(src.dev)
        .map(|i| i.ifindex)
        .unwrap_or(NET_IFINDEX_NONE);
    out.metric = src.metric;
    out
}

/// Collect every interface, reserving up front so the fill under the table lock never allocates.
fn collect_ifaces() -> Result<KVec<Iface>, Errno> {
    let mut staging: KVec<Iface> =
        KVec::with_capacity(slopos_abi::net::NET_MAX_IFACES).map_err(|_| Errno::ENOMEM)?;
    iface::for_each(|i| {
        let _ = staging.push(*i);
    });
    Ok(staging)
}

fn query_ifaces(buf: u64, len: usize, ifindex: u32) -> Result<u64, Errno> {
    let enabled = iface::is_enabled();
    let mut staging = collect_ifaces()?;
    if ifindex != NET_IFINDEX_NONE {
        staging.retain(|i| i.ifindex == ifindex);
    }
    let total = staging.len();

    let size = core::mem::size_of::<UserIface>();
    let written = staging.len().min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_IFACES, size, written, total)?;
    for (i, src) in staging.iter().take(written).enumerate() {
        write_record(base, i, &render_iface(src, enabled))?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

fn query_addrs(buf: u64, len: usize, ifindex: u32) -> Result<u64, Errno> {
    const MAX_ADDRS: usize =
        slopos_abi::net::NET_MAX_IFACES * slopos_abi::net::NET_MAX_ADDRS_PER_IFACE;

    let mut staging: KVec<(u32, IfaceAddr)> =
        KVec::with_capacity(MAX_ADDRS).map_err(|_| Errno::ENOMEM)?;
    iface::for_each(|i| {
        if ifindex != NET_IFINDEX_NONE && i.ifindex != ifindex {
            return;
        }
        for addr in i.addrs() {
            let _ = staging.push((i.ifindex, *addr));
        }
    });
    let total = staging.len();

    let size = core::mem::size_of::<UserAddr>();
    let written = total.min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_ADDRS, size, written, total)?;
    for (i, (idx, addr)) in staging.iter().take(written).enumerate() {
        write_record(base, i, &render_addr(*idx, addr))?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

fn query_routes(buf: u64, len: usize, ifindex: u32) -> Result<u64, Errno> {
    // `all_routes` allocates and returns with the table lock already released.
    let mut routes = ROUTE_TABLE.all_routes();
    if ifindex != NET_IFINDEX_NONE {
        let dev = iface::get(ifindex).map(|i| i.dev);
        match dev {
            Some(dev) => routes.retain(|r| r.dev == dev),
            // An unknown interface has no routes — not every route.
            None => routes.clear(),
        }
    }
    let size = core::mem::size_of::<UserRoute>();
    let total = routes.len();
    let written = total.min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_ROUTES, size, written, total)?;
    for (i, src) in routes.iter().take(written).enumerate() {
        write_record(base, i, &render_route(src))?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

/// Enumerate every socket. Rows are never withheld, but the owner is disclosed
/// only for the caller's own address space, or to a holder of `NET_ADMIN`.
fn query_sockets(
    buf: u64,
    len: usize,
    caller_table: FdTable,
    net_admin: bool,
) -> Result<u64, Errno> {
    // Sized from the table's live capacity: the slab grows past `MAX_SOCKETS`.
    let capacity = slopos_net::socket::socket_table_capacity();
    let mut staging: KVec<slopos_net::socket::SocketRow> =
        KVec::with_capacity(capacity).map_err(|_| Errno::ENOMEM)?;
    slopos_net::socket::collect_sockets(&mut staging);

    let total = staging.len();
    let size = core::mem::size_of::<UserSockInfo>();
    let written = total.min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_SOCKETS, size, written, total)?;
    for (i, row) in staging.iter().take(written).enumerate() {
        write_record(base, i, &render_sock(row, caller_table, net_admin))?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

fn render_sock(
    src: &slopos_net::socket::SocketRow,
    caller_table: FdTable,
    net_admin: bool,
) -> UserSockInfo {
    let mut out = UserSockInfo::default();
    out.local_addr = src.local_ip;
    out.remote_addr = src.remote_ip;
    // Host byte order, unlike `SockAddrIn`: these are formatted, never sent.
    out.local_port = src.local_port;
    out.remote_port = src.remote_port;
    out.family = slopos_abi::net::AF_INET as u8;
    out.sock_type = src.sock_type;
    out.protocol = src.protocol;
    out.state = src.state;
    // An undisclosed owner gets the same sentinel an unowned socket carries, so
    // the refusal is not itself a disclosure.
    out.owner_pid = if net_admin || src.owner.process == Some(caller_table) {
        src.owner.task_id
    } else {
        INVALID_PROCESS_ID
    };
    out.rx_queue = src.rx_queue;
    out.tx_queue = src.tx_queue;
    out.sock_idx = src.sock_idx;
    out
}

fn query_global(buf: u64, len: usize) -> Result<u64, Errno> {
    let enabled = iface::is_enabled();

    let ifaces = collect_ifaces()?;
    let n_ifaces = ifaces.len();
    let running = ifaces
        .iter()
        .filter(|i| i.oper_state(enabled) == slopos_net::iface::OperState::Up)
        .count();

    let routes = ROUTE_TABLE.all_routes();
    let default = slopos_net::iface_ctl::default_route();

    let mut out = UserNetGlobal::default();
    out.seq = slopos_net::netseq::net_seq();
    out.enabled = u8::from(enabled);
    out.connectivity = slopos_net::connectivity::state();
    out.conn_since_ms = slopos_net::connectivity::since_ms();
    out.n_ifaces = n_ifaces.min(u8::MAX as usize) as u8;
    out.n_ifaces_running = running.min(u8::MAX as usize) as u8;
    out.n_routes = routes.len().min(u16::MAX as usize) as u16;
    out.n_neigh = slopos_net::neighbor::NEIGHBOR_CACHE
        .entry_count()
        .min(u16::MAX as usize) as u16;
    if let Some((ifindex, gateway)) = default {
        out.default_ifindex = ifindex;
        out.default_gateway = gateway.0;
    }

    let size = core::mem::size_of::<UserNetGlobal>();
    let written = if capacity_for(len, size) >= 1 { 1 } else { 0 };
    let base = write_header(buf, len, NET_Q_GLOBAL, size, written, 1)?;
    if written == 1 {
        write_record(base, 0, &out)?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

/// DHCP client state, one record per interface running a client. An interface
/// with no client is omitted; `NET_DHCP_DISABLED` means a client in that state.
fn query_dhcp(buf: u64, len: usize, ifindex: u32) -> Result<u64, Errno> {
    let mut staging: KVec<UserDhcpStatus> =
        KVec::with_capacity(slopos_abi::net::NET_MAX_IFACES).map_err(|_| Errno::ENOMEM)?;

    // Interfaces first: the interface table's lock is released before the DHCP
    // client's is taken, never nested.
    let ifaces = collect_ifaces()?;
    for row in ifaces.iter() {
        if ifindex != NET_IFINDEX_NONE && row.ifindex != ifindex {
            continue;
        }
        let Some(state) = slopos_net::dhcp::state_of(row.dev) else {
            continue;
        };
        let mut out = UserDhcpStatus::default();
        out.ifindex = row.ifindex;
        out.state = state;
        if let Some((lease, t1, t2, server)) = slopos_net::dhcp::lease_of(row.dev) {
            out.server_id = server;
            out.lease_remaining_s = lease;
            out.t1_remaining_s = t1;
            out.t2_remaining_s = t2;
        }
        let _ = staging.push(out);
    }

    let total = staging.len();
    let size = core::mem::size_of::<UserDhcpStatus>();
    let written = total.min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_DHCP, size, written, total)?;
    for (i, record) in staging.iter().take(written).enumerate() {
        write_record(base, i, record)?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

/// The neighbour cache, filtered to one interface when asked; an unknown
/// interface yields nothing rather than everything.
fn query_neigh(buf: u64, len: usize, ifindex: u32) -> Result<u64, Errno> {
    let dev = if ifindex == NET_IFINDEX_NONE {
        None
    } else {
        match iface::get(ifindex) {
            Some(i) => Some(i.dev),
            None => {
                let size = core::mem::size_of::<UserNeigh>();
                write_header(buf, len, NET_Q_NEIGH, size, 0, 0)?;
                return Ok(core::mem::size_of::<UserNetQueryHdr>() as u64);
            }
        }
    };

    let (staging, total) = NEIGHBOR_CACHE.snapshot_owned(dev);

    let size = core::mem::size_of::<UserNeigh>();
    let written = staging.len().min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_NEIGH, size, written, total)?;
    for (i, snap) in staging.iter().take(written).enumerate() {
        let mut record = UserNeigh::default();
        record.ifindex = iface::get_by_dev(snap.dev).map_or(NET_IFINDEX_NONE, |i| i.ifindex);
        record.addr = snap.ip.0;
        record.mac = snap.mac.0;
        record.state = snap.state;
        record.confirmed_ms_ago = snap.confirmed_ms_ago;
        record.queued_pkts = snap.queued_pkts;
        write_record(base, i, &record)?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

/// The resolver configuration — always exactly one record.
fn query_resolver(buf: u64, len: usize) -> Result<u64, Errno> {
    let mut servers = [slopos_net::Ipv4Addr::UNSPECIFIED; slopos_abi::net::NET_MAX_RESOLVERS];
    let n = RESOLVER.servers(&mut servers);

    let mut record = UserResolver::default();
    for (slot, server) in record.servers.iter_mut().zip(servers.iter()).take(n) {
        *slot = server.0;
    }
    record.n_servers = n as u8;
    record.source = RESOLVER.source();
    record.source_ifindex = RESOLVER.source_ifindex();
    record.timeout_ms = RESOLVER.timeout_ms();
    record.attempts = RESOLVER.attempts();

    let size = core::mem::size_of::<UserResolver>();
    let written = 1usize.min(capacity_for(len, size));
    let base = write_header(buf, len, NET_Q_RESOLVER, size, written, 1)?;
    if written == 1 {
        write_record(base, 0, &record)?;
    }
    Ok(core::mem::size_of::<UserNetQueryHdr>() as u64 + (written * size) as u64)
}

define_syscall!(syscall_net_query
    (ctx, what: u64, index: u64, buf: u64, len: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let len = len as usize;
    let ifindex = index as u32;

    match what as u32 {
        NET_Q_IFACES => query_ifaces(buf, len, ifindex),
        NET_Q_ADDRS => query_addrs(buf, len, ifindex),
        NET_Q_ROUTES => query_routes(buf, len, ifindex),
        NET_Q_GLOBAL => query_global(buf, len),
        NET_Q_DHCP => query_dhcp(buf, len, ifindex),
        NET_Q_SOCKETS => query_sockets(buf, len, process_id, ctx.is_net_admin()),
        NET_Q_NEIGH => query_neigh(buf, len, ifindex),
        NET_Q_RESOLVER => query_resolver(buf, len),
        _ => Err(Errno::EINVAL),
    }
});
