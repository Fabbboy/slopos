//! `ip status` — the whole stack in one screen.
//!
//! Not an iproute2 command: it renders `NET_Q_GLOBAL` through the same renderer
//! the compositor's status bar uses, so both show the same numbers.

use slopos_abi::net::{NET_IFINDEX_NONE, NET_Q_GLOBAL, UserNetGlobal};
use slopos_net_core::Ipv4;
use slopos_net_core::render::connectivity_label;

use super::{Failure, Outcome, load_ifaces};
use crate::net_query as query;

pub fn show() -> Outcome {
    let ifaces = load_ifaces("status")?;
    let q = query::fetch::<UserNetGlobal>(NET_Q_GLOBAL, NET_IFINDEX_NONE)
        .map_err(|err| Failure::from_errno("status", err))?;

    let Some(global) = q.records.first() else {
        return Err(Failure::runtime("status", "the kernel returned no state"));
    };

    let enabled = global.enabled != 0;
    println!("networking:   {}", if enabled { "on" } else { "off" });
    println!(
        "connectivity: {}",
        connectivity_label(enabled, global.connectivity)
    );
    println!(
        "interfaces:   {} ({} running)",
        global.n_ifaces, global.n_ifaces_running
    );

    if global.default_ifindex == NET_IFINDEX_NONE {
        println!("default:      none");
    } else {
        let via = Ipv4(global.default_gateway);
        let dev = query::name_or_index(&ifaces, global.default_ifindex);
        if via.is_unspecified() {
            println!("default:      dev {dev}");
        } else {
            println!("default:      via {via} dev {dev}");
        }
    }

    println!("routes:       {}", global.n_routes);
    println!("neighbours:   {}", global.n_neigh);
    Ok(())
}
