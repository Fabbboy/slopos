//! `ip addr` — the addresses assigned to each interface.
//!
//! Interfaces and addresses are joined on `ifindex`, never on the name: names
//! are reused across re-probes, indices are not.

use core::fmt::Write;
use std::string::String;
use std::vec::Vec;

use slopos_abi::net::{
    NET_IFINDEX_NONE, NET_IFOP_FLUSH_ADDRS, NET_LFT_FOREVER, NET_Q_ADDRS, NET_Q_IFACES, UserAddr,
    UserAddrReq, UserIface,
};
use slopos_net_core::columns::{BRIEF_NAME, BRIEF_STATE, field};
use slopos_net_core::ip_plan::Options;
use slopos_net_core::render::{addr_origin, addr_scope, iface_kind, oper_state, write_if_flags};
use slopos_net_core::{Cidr, Mac};

use super::{Failure, Outcome, device_index, load_ifaces, warn_truncated};
use crate::net_query::{self as query, name_of};
use crate::syscall::net::{net_addr_ctl, net_iface_ctl};

pub fn show(dev: Option<&[u8]>, opts: Options) -> Outcome {
    let ifaces = load_ifaces("addr")?;
    let filter = match dev {
        Some(name) => Some(device_index(&ifaces, name)?),
        None => None,
    };
    let scope = filter.unwrap_or(NET_IFINDEX_NONE);
    let links = query::fetch::<UserIface>(NET_Q_IFACES, scope)
        .map_err(|err| Failure::from_errno("addr", err))?;
    let addrs = query::fetch::<UserAddr>(NET_Q_ADDRS, scope)
        .map_err(|err| Failure::from_errno("addr", err))?;
    if addrs.truncated() {
        warn_truncated("addr", addrs.records.len(), addrs.hdr.total_count);
    }

    for iface in &links.records {
        let mine: Vec<&UserAddr> = addrs
            .records
            .iter()
            .filter(|a| a.ifindex == iface.ifindex)
            .collect();
        if opts.brief {
            print_brief(iface, &mine);
        } else {
            print_full(iface, &mine);
        }
    }
    Ok(())
}

pub fn add(cidr: Cidr, dev: &[u8], adding: bool) -> Outcome {
    let ifaces = load_ifaces("addr")?;
    let ifindex = device_index(&ifaces, dev)?;

    let mut req = UserAddrReq::default();
    req.ifindex = ifindex;
    req.addr = cidr.addr.octets();
    req.prefix_len = cidr.prefix_len;
    req.family = slopos_abi::net::AF_INET as u8;
    req.scope = slopos_abi::net::NET_ADDR_SCOPE_GLOBAL;

    let context = query::name_or_index(&ifaces, ifindex);
    net_addr_ctl(&req, adding).map_err(|err| Failure::from_errno(context, err))
}

pub fn flush(dev: Option<&[u8]>) -> Outcome {
    let ifaces = load_ifaces("addr")?;
    let targets: Vec<u32> = match dev {
        Some(name) => std::vec![device_index(&ifaces, name)?],
        None => ifaces.rows.iter().map(|row| row.ifindex).collect(),
    };

    for ifindex in targets {
        let context = query::name_or_index(&ifaces, ifindex);
        net_iface_ctl(ifindex, NET_IFOP_FLUSH_ADDRS, 0)
            .map_err(|err| Failure::from_errno(context, err))?;
    }
    Ok(())
}

fn print_full(iface: &UserIface, addrs: &[&UserAddr]) {
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

    for addr in addrs {
        println!(
            "    inet {}/{} scope {} {} {}",
            slopos_net_core::Ipv4(addr.addr),
            addr.prefix_len,
            addr_scope(addr.scope),
            addr_origin(addr.origin),
            name_of(iface)
        );
        println!(
            "       valid_lft {} preferred_lft {}",
            lifetime(addr.valid_lft_s),
            lifetime(addr.pref_lft_s)
        );
    }
}

fn print_brief(iface: &UserIface, addrs: &[&UserAddr]) {
    let mut line = String::new();
    let _ = field(&mut line, name_of(iface), BRIEF_NAME);
    let _ = field(&mut line, oper_state(iface.oper_state), BRIEF_STATE);
    for addr in addrs {
        let _ = write!(
            line,
            "{}/{} ",
            slopos_net_core::Ipv4(addr.addr),
            addr.prefix_len
        );
    }
    println!("{}", line.trim_end());
}

/// Spelled as iproute2 does: `forever`, or seconds with the unit attached.
fn lifetime(seconds: u32) -> String {
    if seconds == NET_LFT_FOREVER {
        String::from("forever")
    } else {
        std::format!("{seconds}sec")
    }
}
