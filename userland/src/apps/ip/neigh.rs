//! `ip neigh` — the neighbour cache.
//!
//! Output follows iproute2: `<addr> dev <name> lladdr <mac> <STATE>`, with the
//! `lladdr` clause omitted entirely when there is no MAC rather than printed as
//! all-zeroes. An `INCOMPLETE` entry additionally reports how many packets are
//! queued behind it, which is the one thing that distinguishes "ARP is in
//! flight" from "ARP is in flight and traffic is piling up".

use slopos_abi::net::{NET_IFINDEX_NONE, NET_IFOP_DEL_NEIGH, NET_IFOP_FLUSH_NEIGH, NET_Q_NEIGH};
use slopos_abi::net::{NET_NEIGH_FAILED, NET_NEIGH_INCOMPLETE, UserNeigh};
use slopos_net_core::{Ipv4, Mac};

use super::{Failure, Outcome, device_index, load_ifaces, warn_truncated};
use crate::net_query as query;
use crate::syscall::net::net_iface_ctl;

pub fn show(dev: Option<&[u8]>) -> Outcome {
    let ifaces = load_ifaces("neigh")?;
    let filter = match dev {
        Some(name) => Some(device_index(&ifaces, name)?),
        None => None,
    };

    let q = query::fetch::<UserNeigh>(NET_Q_NEIGH, filter.unwrap_or(NET_IFINDEX_NONE))
        .map_err(|err| Failure::from_errno("neigh", err))?;
    if q.truncated() {
        warn_truncated("neigh", q.records.len(), q.hdr.total_count);
    }

    for entry in &q.records {
        let mut line = std::format!(
            "{} dev {}",
            Ipv4(entry.addr),
            query::name_or_index(&ifaces, entry.ifindex)
        );
        // No MAC is the truth for INCOMPLETE and FAILED; `lladdr 00:00:00:00:00:00`
        // would read as a resolved neighbour with a broken address.
        if entry.state != NET_NEIGH_INCOMPLETE && entry.state != NET_NEIGH_FAILED {
            line.push_str(&std::format!(" lladdr {}", Mac(entry.mac)));
        }
        line.push(' ');
        line.push_str(slopos_net_core::render::neigh_state(entry.state));
        if entry.queued_pkts > 0 {
            line.push_str(&std::format!(" ({} queued)", entry.queued_pkts));
        }
        println!("{}", line);
    }
    Ok(())
}

pub fn del(addr: Ipv4, dev: &[u8]) -> Outcome {
    let ifaces = load_ifaces("neigh")?;
    let ifindex = device_index(&ifaces, dev)?;
    let context = query::name_or_index(&ifaces, ifindex);

    // The address is the operation's scalar operand: `net_iface_ctl` takes no
    // user memory at all.
    let key = u64::from(u32::from_be_bytes(addr.octets()));
    net_iface_ctl(ifindex, NET_IFOP_DEL_NEIGH, key).map_err(|err| Failure::from_errno(context, err))
}

pub fn flush(dev: Option<&[u8]>) -> Outcome {
    let ifaces = load_ifaces("neigh")?;
    let targets: std::vec::Vec<u32> = match dev {
        Some(name) => std::vec![device_index(&ifaces, name)?],
        None => ifaces.rows.iter().map(|row| row.ifindex).collect(),
    };

    for ifindex in targets {
        let context = query::name_or_index(&ifaces, ifindex);
        net_iface_ctl(ifindex, NET_IFOP_FLUSH_NEIGH, 0)
            .map_err(|err| Failure::from_errno(context, err))?;
    }
    Ok(())
}
