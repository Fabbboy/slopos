//! `ip dhcp` — the DHCP client's lifecycle and state.

use slopos_abi::net::{
    NET_IFINDEX_NONE, NET_IFOP_DHCP_RELEASE, NET_IFOP_DHCP_RENEW, NET_IFOP_DHCP_START,
    NET_IFOP_DHCP_STOP, NET_Q_DHCP, UserDhcpStatus,
};
use slopos_net_core::Ipv4;
use slopos_net_core::render::{dhcp_reason, dhcp_state};

use super::{Failure, Outcome, device_index, load_ifaces};
use crate::net_query as query;
use crate::syscall::net::net_iface_ctl;

#[derive(Clone, Copy)]
pub enum Op {
    Start,
    Stop,
    Renew,
    Release,
}

impl Op {
    const fn code(self) -> u32 {
        match self {
            Op::Start => NET_IFOP_DHCP_START,
            Op::Stop => NET_IFOP_DHCP_STOP,
            Op::Renew => NET_IFOP_DHCP_RENEW,
            Op::Release => NET_IFOP_DHCP_RELEASE,
        }
    }
}

pub fn op(dev: &[u8], which: Op) -> Outcome {
    let ifaces = load_ifaces("dhcp")?;
    let ifindex = device_index(&ifaces, dev)?;
    let context = query::name_or_index(&ifaces, ifindex);
    net_iface_ctl(ifindex, which.code(), 0).map_err(|err| Failure::from_errno(context, err))
}

pub fn status(dev: Option<&[u8]>) -> Outcome {
    let ifaces = load_ifaces("dhcp")?;
    let filter = match dev {
        Some(name) => Some(device_index(&ifaces, name)?),
        None => None,
    };

    let q = query::fetch::<UserDhcpStatus>(NET_Q_DHCP, filter.unwrap_or(NET_IFINDEX_NONE))
        .map_err(|err| Failure::from_errno("dhcp", err))?;

    for lease in &q.records {
        println!(
            "{}: {} ({})",
            query::name_or_index(&ifaces, lease.ifindex),
            dhcp_state(lease.state),
            dhcp_reason(lease.last_reason)
        );
        if !Ipv4(lease.server_id).is_unspecified() {
            println!("    server {}", Ipv4(lease.server_id));
        }
        println!(
            "    lease {}s  t1 {}s  t2 {}s  retries {}",
            lease.lease_remaining_s, lease.t1_remaining_s, lease.t2_remaining_s, lease.retries
        );
    }
    Ok(())
}
