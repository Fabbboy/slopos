//! The network subsystem's diagnostic-console command.
//!
//! Registration lives here rather than in OSTD because OSTD is linked into
//! userland binaries too, and their linker script brackets no
//! `.kconsole_registry` section.
//!
//! Each table is read through its snapshot accessor, so the command holds no
//! two network locks at once and holds none across the console's own output.
//! Every loop is bounded by [`KConsole::budget_left`].

use slopos_abi::net::{
    IFF_SLOP_CARRIER_ASSUMED, IFF_SLOP_DHCP, IFF_SLOP_DISABLED, IFF_SLOP_NO_CARRIER,
    NET_MAX_ADDRS_PER_IFACE, NET_MAX_IFACES, NET_NEIGH_FAILED, NET_NEIGH_INCOMPLETE,
    NET_NEIGH_REACHABLE, NET_NEIGH_STALE,
};
use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;

use crate::connectivity;
use crate::iface::{self, Iface, IfaceAddr};
use crate::neighbor::{NEIGHBOR_CACHE, NeighborSnapshot};
use crate::netmon::NETMON_TABLE;
use crate::route::ROUTE_TABLE;
use crate::types::{DevIndex, Ipv4Addr, MacAddr};

slopos_ostd::kcommand! {
    name = net,
    key = b'n',
    help = "interfaces, addresses, routes, neighbours, connectivity, monitors",
    flags = KCMD_INFORMATIONAL,
    run = run_net,
}

/// Neighbours printed at most; dumping all 256 the cache holds would spend the
/// whole line budget.
const MAX_NEIGH_SHOWN: usize = 16;

fn run_net(kc: &mut KConsole<'_>) {
    print_connectivity(kc);
    print_ifaces(kc);
    print_routes(kc);
    print_neighbours(kc);
    print_monitors(kc);
}

/// The classifier's evidence is printed next to its verdict: "limited" on its
/// own invites a guess.
fn print_connectivity(kc: &mut KConsole<'_>) {
    let state = connectivity::state();
    let evidence = connectivity::gather_evidence();
    kline!(
        kc,
        "connectivity: {} since={}ms enabled={} (classifier {})",
        connectivity::state_name(state),
        connectivity::since_ms(),
        iface::is_enabled(),
        if connectivity::CONNECTIVITY.is_enabled() {
            "kernel"
        } else {
            "userland"
        }
    );
    kline!(
        kc,
        "  evidence: carrier={} address={} default_route={} gateway_reachable={} wan_fresh={}",
        evidence.any_carrier,
        evidence.has_address,
        evidence.has_default_route,
        evidence.gateway_reachable,
        evidence.wan_fresh
    );
    match connectivity::cached_gateway() {
        Some(gw) => kline!(kc, "  gateway: {}", gw),
        None => kline!(kc, "  gateway: none"),
    }
}

fn print_ifaces(kc: &mut KConsole<'_>) {
    let enabled = iface::is_enabled();
    let mut ifaces = [const { None::<Iface> }; NET_MAX_IFACES];
    let mut count = 0usize;
    iface::for_each(|i| {
        if count < ifaces.len() {
            ifaces[count] = Some(*i);
            count += 1;
        }
    });

    kline!(kc, "interfaces: {}", count);
    for iface_row in ifaces.iter().flatten() {
        if kc.budget_left() == 0 {
            return;
        }
        print_iface(kc, iface_row, enabled);
    }
}

fn print_iface(kc: &mut KConsole<'_>, i: &Iface, enabled: bool) {
    let flags = i.flags(enabled);
    kline!(
        kc,
        "  {} idx={} {:?} oper={:?} mtu={} mac={} flags={:#x}{}{}{}{}",
        i.name,
        i.ifindex,
        i.kind,
        i.oper_state(enabled),
        i.mtu,
        i.mac,
        flags,
        if flags & IFF_SLOP_DISABLED != 0 {
            " DISABLED"
        } else {
            ""
        },
        if flags & IFF_SLOP_NO_CARRIER != 0 {
            " NO-CARRIER"
        } else {
            ""
        },
        if flags & IFF_SLOP_CARRIER_ASSUMED != 0 {
            " CARRIER-ASSUMED"
        } else {
            ""
        },
        if flags & IFF_SLOP_DHCP != 0 {
            " DHCP"
        } else {
            ""
        }
    );

    let mut addrs = [const {
        (
            0u32,
            IfaceAddr::permanent(
                Ipv4Addr::UNSPECIFIED,
                0,
                crate::iface::AddrScope::Global,
                crate::iface::AddrOrigin::Static,
            ),
        )
    }; NET_MAX_ADDRS_PER_IFACE];
    let (written, total) = iface::snapshot_addrs(i.ifindex, &mut addrs);
    for (_, addr) in addrs.iter().take(written) {
        if kc.budget_left() == 0 {
            return;
        }
        kline!(
            kc,
            "    addr {}/{} scope={:?} origin={:?}",
            addr.addr,
            addr.prefix_len,
            addr.scope,
            addr.origin
        );
    }
    if total > written {
        kline!(kc, "    ... {} more addresses", total - written);
    }
}

fn print_routes(kc: &mut KConsole<'_>) {
    let routes = ROUTE_TABLE.all_routes();
    kline!(kc, "routes: {}", routes.len());
    for r in routes.iter() {
        if kc.budget_left() == 0 {
            return;
        }
        let ifname = iface::get_by_dev(r.dev).map(|i| i.name);
        match ifname {
            Some(name) => kline!(kc, "  {:?} dev {}", r, name),
            None => kline!(kc, "  {:?} dev {}", r, r.dev),
        }
    }
}

fn print_neighbours(kc: &mut KConsole<'_>) {
    let mut snap = [const {
        NeighborSnapshot {
            dev: DevIndex(0),
            ip: Ipv4Addr::UNSPECIFIED,
            mac: MacAddr::ZERO,
            state: NET_NEIGH_INCOMPLETE,
            confirmed_ms_ago: 0,
            queued_pkts: 0,
        }
    }; MAX_NEIGH_SHOWN];
    let (written, total) = NEIGHBOR_CACHE.snapshot(None, &mut snap);

    kline!(kc, "neighbours: {}", total);
    for entry in snap.iter().take(written) {
        if kc.budget_left() == 0 {
            return;
        }
        kline!(
            kc,
            "  {} -> {} dev {} {} queued={}",
            entry.ip,
            entry.mac,
            entry.dev,
            neigh_state_name(entry.state),
            entry.queued_pkts
        );
    }
    if total > written {
        kline!(kc, "  ... {} more", total - written);
    }
}

const fn neigh_state_name(state: u8) -> &'static str {
    match state {
        NET_NEIGH_INCOMPLETE => "incomplete",
        NET_NEIGH_REACHABLE => "reachable",
        NET_NEIGH_STALE => "stale",
        NET_NEIGH_FAILED => "failed",
        _ => "?",
    }
}

/// `dropped` is the only place a subscriber that has stopped draining becomes
/// visible.
fn print_monitors(kc: &mut KConsole<'_>) {
    let stats = NETMON_TABLE.stats();
    kline!(
        kc,
        "netmon: subscribers={} queued={} dropped={} seq={}",
        stats.live,
        stats.queued,
        stats.dropped,
        crate::netseq::net_seq()
    );
}
