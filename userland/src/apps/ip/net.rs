//! `ip net` — the master networking switch.
//!
//! The one operation that addresses the stack rather than an interface, which
//! is why it is a separate object: `ip link set DEV down` is a statement about
//! one device, and this is a statement about the machine. The ABI keeps them
//! apart too — the switch is refused unless it is addressed to
//! [`NET_IFINDEX_GLOBAL`], so a caller cannot reach it by aiming an
//! interface-shaped command at a device and having the index quietly ignored.
//!
//! Turning it off downs every device by construction, so nothing automated
//! touches it: it is the one verb in this binary whose blast radius is the
//! whole network stack.

use slopos_abi::net::{
    NET_IFINDEX_GLOBAL, NET_IFINDEX_NONE, NET_IFOP_SET_ENABLED, NET_Q_GLOBAL, UserNetGlobal,
};

use super::{Failure, Outcome};
use crate::net_query as query;
use crate::syscall::net::net_iface_ctl;

/// Report the switch, and nothing else.
///
/// One fact on one line, because that is the question `ip net` asks;
/// connectivity, interface counts and the default route are `ip status`.
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

/// Move the switch. Requires `NET_ADMIN`, which `/bin/ip` holds by path.
pub fn set(enabled: bool) -> Outcome {
    net_iface_ctl(NET_IFINDEX_GLOBAL, NET_IFOP_SET_ENABLED, u64::from(enabled))
        .map_err(|err| Failure::from_errno("net", err))
}
