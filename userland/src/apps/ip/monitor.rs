//! `ip monitor` — the network event stream.
//!
//! A dropped event is reported as a `NET_EV_OVERFLOW` ordered before the
//! records that followed the drop, so a reader never loses its position.
//!
//! Bounded by default: with no `-c` or `-t` it stops after ten seconds, since a
//! monitor that blocks forever reads as a hang in a serial transcript. `-t 0`
//! asks for no deadline.
//!
//! Events name an interface by index, so the interface table is snapshotted at
//! start and re-read when an interface appears; an index the table does not
//! have prints as `if#N` rather than being dropped.

use std::string::String;

use slopos_abi::net::{
    NET_EV_ADDR_ADDED, NET_EV_ADDR_REMOVED, NET_EV_CONNECTIVITY, NET_EV_DHCP, NET_EV_GLOBAL_ENABLE,
    NET_EV_IFACE_ADDED, NET_EV_IFACE_CHANGED, NET_EV_IFACE_REMOVED, NET_EV_NEIGH_CHANGED,
    NET_EV_OVERFLOW, NET_EV_RESOLVER, NET_EV_ROUTE_ADDED, NET_EV_ROUTE_REMOVED, NET_EVENT_LEN,
    NET_MON_ADDR, NET_MON_CONN, NET_MON_DEFAULT, NET_MON_DHCP, NET_MON_GLOBAL, NET_MON_IFACE,
    NET_MON_NEIGH, NET_MON_RESOLV, NET_MON_ROUTE, NetEvent,
};
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_net_core::Ipv4;
use slopos_net_core::render::{
    addr_origin, addr_scope, connectivity, dhcp_reason, dhcp_state, neigh_state, oper_state,
    route_origin, write_if_flags,
};

use super::{Failure, MonitorBounds, Outcome};
use crate::net_query::{self as query, Ifaces};
use crate::syscall::fs::{poll, read_slice};
use crate::syscall::net::net_monitor;

/// How many events one `read` can drain. The kernel ring is bounded, so a
/// larger buffer only means fewer syscalls, never a deeper backlog.
const DRAIN_EVENTS: usize = 32;

pub fn run(filter: Option<&[u8]>, bounds: MonitorBounds) -> Outcome {
    let mask = match filter {
        None => NET_MON_DEFAULT,
        Some(word) => mask_for(word)?,
    };

    let fd = net_monitor(mask, 0).map_err(|err| Failure::from_errno("monitor", err))?;
    let mut ifaces = query::Ifaces::fetch().map_err(|err| Failure::from_errno("monitor", err))?;

    let started = std::time::Instant::now();
    let mut seen: u32 = 0;
    let mut buf = [0u8; NET_EVENT_LEN * DRAIN_EVENTS];

    loop {
        if bounds.count.is_some_and(|limit| seen >= limit) {
            break;
        }
        let timeout = match bounds.deadline_ms {
            None => -1,
            Some(total) => {
                let elapsed = started.elapsed().as_millis() as i64;
                let left = total - elapsed;
                if left <= 0 {
                    break;
                }
                left
            }
        };

        let mut fds = [UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        let ready = poll(&mut fds, timeout).map_err(|err| Failure::from_errno("monitor", err))?;
        if ready == 0 {
            break;
        }

        let n =
            read_slice(fd.raw(), &mut buf).map_err(|err| Failure::from_errno("monitor", err))?;
        if n == 0 {
            break;
        }

        for chunk in buf[..n].chunks_exact(NET_EVENT_LEN) {
            let mut record = [0u8; NET_EVENT_LEN];
            record.copy_from_slice(chunk);
            let event = NetEvent::from_bytes(&record);

            // A new interface is one the snapshot cannot name, so refresh
            // before rendering the line that would otherwise say `if#N`.
            if event.kind == NET_EV_IFACE_ADDED {
                if let Ok(fresh) = query::Ifaces::fetch() {
                    ifaces = fresh;
                }
            }

            print_event(&event, &ifaces);
            seen = seen.saturating_add(1);
            if bounds.count.is_some_and(|limit| seen >= limit) {
                break;
            }
        }
    }

    // `fd` closes here: the monitor registry slot is released with it.
    Ok(())
}

/// Neighbour churn is not in the default set: ARP is the only high-rate source
/// in the stack, and subscribing to it keeps a bounded ring in permanent
/// overflow, masking the interface events a subscriber opened the fd for.
fn mask_for(word: &[u8]) -> Result<u32, Failure> {
    let mask = match word {
        b"link" => NET_MON_IFACE,
        b"addr" => NET_MON_ADDR,
        b"route" => NET_MON_ROUTE,
        b"dns" => NET_MON_RESOLV,
        b"conn" => NET_MON_CONN,
        b"dhcp" => NET_MON_DHCP,
        b"global" => NET_MON_GLOBAL,
        b"neigh" => NET_MON_NEIGH,
        b"all" => NET_MON_DEFAULT | NET_MON_NEIGH,
        _ => {
            return Err(Failure::usage(
                core::str::from_utf8(word).unwrap_or("<non-utf8>"),
                "unknown filter; one of link, addr, route, dns, conn, dhcp, global, neigh, all",
            ));
        }
    };
    Ok(mask)
}

fn print_event(event: &NetEvent, ifaces: &Ifaces) {
    let dev = query::name_or_index(ifaces, event.ifindex);
    match event.kind {
        NET_EV_IFACE_ADDED | NET_EV_IFACE_CHANGED | NET_EV_IFACE_REMOVED => {
            let p = event.as_iface();
            let verb = match event.kind {
                NET_EV_IFACE_ADDED => "added",
                NET_EV_IFACE_REMOVED => "removed",
                _ => "changed",
            };
            let mut flags = String::new();
            let _ = write_if_flags(&mut flags, p.flags);
            println!(
                "[LINK] {dev}: {verb} {} -> {} carrier {} admin {} mtu {} {}",
                oper_state(p.oper_old),
                oper_state(p.oper_new),
                p.carrier,
                p.admin_up,
                p.mtu,
                flags
            );
        }
        NET_EV_ADDR_ADDED | NET_EV_ADDR_REMOVED => {
            let p = event.as_addr();
            let verb = if event.kind == NET_EV_ADDR_ADDED {
                "added"
            } else {
                "removed"
            };
            println!(
                "[ADDR] {dev}: {verb} {}/{} scope {} {}",
                Ipv4(p.addr),
                p.prefix_len,
                addr_scope(p.scope),
                addr_origin(p.origin)
            );
        }
        NET_EV_ROUTE_ADDED | NET_EV_ROUTE_REMOVED => {
            let p = event.as_route();
            let verb = if event.kind == NET_EV_ROUTE_ADDED {
                "added"
            } else {
                "removed"
            };
            let dest = if p.prefix_len == 0 && Ipv4(p.prefix).is_unspecified() {
                String::from("default")
            } else {
                std::format!("{}/{}", Ipv4(p.prefix), p.prefix_len)
            };
            let gateway = Ipv4(p.gateway);
            if gateway.is_unspecified() {
                println!(
                    "[ROUTE] {dev}: {verb} {dest} proto {} metric {}",
                    route_origin(p.origin),
                    p.metric
                );
            } else {
                println!(
                    "[ROUTE] {dev}: {verb} {dest} via {gateway} proto {} metric {}",
                    route_origin(p.origin),
                    p.metric
                );
            }
        }
        NET_EV_RESOLVER => {
            let p = event.as_resolver();
            println!(
                "[DNS] {} server(s), primary {}",
                p.n_servers,
                Ipv4(p.primary)
            );
        }
        NET_EV_CONNECTIVITY => {
            let p = event.as_connectivity();
            println!("[CONN] {} -> {}", connectivity(p.old), connectivity(p.new));
        }
        NET_EV_DHCP => {
            let p = event.as_dhcp();
            println!(
                "[DHCP] {dev}: {} ({}) lease {}s",
                dhcp_state(p.state),
                dhcp_reason(p.reason),
                p.lease_remaining_s
            );
        }
        NET_EV_GLOBAL_ENABLE => {
            let on = event.as_u32() != 0;
            println!("[GLOBAL] networking {}", if on { "on" } else { "off" });
        }
        NET_EV_NEIGH_CHANGED => {
            let p = event.as_neigh();
            println!("[NEIGH] {dev}: {} {}", Ipv4(p.addr), neigh_state(p.state));
        }
        NET_EV_OVERFLOW => {
            println!("[OVERFLOW] {} event(s) dropped", event.as_u32());
        }
        other => println!("[?] kind {other} ifindex {}", event.ifindex),
    }
}
