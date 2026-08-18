//! `ip net` — the master networking switch.
//!
//! The ABI refuses the switch unless it is addressed to [`NET_IFINDEX_GLOBAL`],
//! so an interface-shaped command aimed at a device cannot reach it. Turning it
//! off downs every device by construction.

use slopos_abi::net::{
    NET_IFINDEX_GLOBAL, NET_IFINDEX_NONE, NET_IFOP_SET_ENABLED, NET_Q_GLOBAL, UserNetGlobal,
};

use super::{Failure, Outcome};
use crate::net_query as query;
use crate::syscall::net::net_iface_ctl;

pub fn show() -> Outcome {
    let q = query::fetch::<UserNetGlobal>(NET_Q_GLOBAL, NET_IFINDEX_NONE)
        .map_err(|err| Failure::from_errno("net", err))?;

    let Some(global) = q.records.first() else {
        return Err(Failure::runtime("net", "the kernel returned no state"));
    };
    println!(
        "networking: {}",
        if global.enabled != 0 { "on" } else { "off" }
    );
    Ok(())
}

/// Requires `NET_ADMIN`, which `/bin/ip` holds by path.
pub fn set(enabled: bool) -> Outcome {
    net_iface_ctl(NET_IFINDEX_GLOBAL, NET_IFOP_SET_ENABLED, u64::from(enabled))
        .map_err(|err| Failure::from_errno("net", err))
}
