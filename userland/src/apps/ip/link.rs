//! `ip link` — interfaces, their flags and their counters.
//!
//! The output follows iproute2, including two things that look like bugs and
//! are not. `lo` reports state `UNKNOWN`, because loopback has no link layer to
//! have an operational state about and RFC 2863 says so; reporting `UP` there
//! would be inventing a fact. And `NO-CARRIER` sorts first in the flag list,
//! because it is the one flag that explains why the rest of the line looks
//! wrong — buried between `MULTICAST` and `UP` it gets missed.
//!
//! Fields SlopOS has no answer for are omitted rather than filled in. iproute2
//! prints `qdisc`, `qlen`, `mode` and `group`; a `qdisc noqueue` here would be
//! a lie a reader could act on, so those columns simply are not there.

use core::fmt::Write;
use std::string::String;

use slopos_abi::net::{NET_IFINDEX_NONE, NET_IFOP_SET_ADMIN_UP, NET_Q_IFACES, UserIface};
use slopos_net_core::Mac;
use slopos_net_core::columns::{BRIEF_MAC, BRIEF_NAME, BRIEF_STATE, field};
use slopos_net_core::ip_plan::Options;
use slopos_net_core::render::{iface_kind, oper_state, write_if_flags};

use super::{Failure, Outcome, device_index, load_ifaces, warn_truncated};
use crate::net_query::{self as query, name_of};
use crate::syscall::net::net_iface_ctl;

pub fn show(dev: Option<&[u8]>, opts: Options) -> Outcome {
    let ifaces = load_ifaces("link")?;
    let filter = match dev {
        Some(name) => Some(device_index(&ifaces, name)?),
        None => None,
    };

    let q = query::fetch::<UserIface>(NET_Q_IFACES, filter.unwrap_or(NET_IFINDEX_NONE))
        .map_err(|err| Failure::from_errno("link", err))?;
    if q.truncated() {
        warn_truncated("link", q.records.len(), q.hdr.total_count);
    }

    for iface in &q.records {
        if opts.brief {
            print_brief(iface);
        } else {
            print_full(iface, opts.stats);
        }
    }
    Ok(())
}

pub fn set(dev: &[u8], up: bool) -> Outcome {
    let ifaces = load_ifaces("link")?;
    let ifindex = device_index(&ifaces, dev)?;
    let name = crate::net_query::name_or_index(&ifaces, ifindex);
    net_iface_ctl(ifindex, NET_IFOP_SET_ADMIN_UP, u64::from(up))
        .map_err(|err| Failure::from_errno(name, err))
}

/// `2: eth0: <BROADCAST,MULTICAST,UP> mtu 1500 state UP`
/// `    link/ether 52:54:00:12:34:56`
fn print_full(iface: &UserIface, stats: bool) {
    let mut flags = String::new();
    let _ = write_if_flags(&mut flags, iface.flags);

    println!(
        "{}: {}: {} mtu {} state {}",
        iface.ifindex,
        name_of(iface),
        flags,
        iface.mtu,
        oper_state(iface.oper_state)
    );
    println!("    link/{} {}", iface_kind(iface.kind), Mac(iface.mac));

    if stats {
        // Four columns because SlopOS counts four things. iproute2 prints
        // `overrun`/`mcast`/`carrier`/`collsns` too; a zero under a heading the
        // stack never increments reads as a measurement.
        println!("    RX: bytes packets errors dropped");
        println!(
            "    {} {} {} {}",
            iface.rx_bytes, iface.rx_packets, iface.rx_errors, iface.rx_dropped
        );
        println!("    TX: bytes packets errors dropped");
        println!(
            "    {} {} {} {}",
            iface.tx_bytes, iface.tx_packets, iface.tx_errors, iface.tx_dropped
        );
    }
}

/// `eth0            UP             52:54:00:12:34:56 <BROADCAST,MULTICAST,UP>`
fn print_brief(iface: &UserIface) {
    let mut line = String::new();
    let _ = field(&mut line, name_of(iface), BRIEF_NAME);
    let _ = field(&mut line, oper_state(iface.oper_state), BRIEF_STATE);

    let mut mac = String::new();
    let _ = write!(mac, "{}", Mac(iface.mac));
    let _ = field(&mut line, &mac, BRIEF_MAC);

    let _ = write_if_flags(&mut line, iface.flags);
    println!("{line}");
}
