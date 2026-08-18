//! `ip route` — the routing table.
//!
//! Follows iproute2: `0.0.0.0/0` prints as `default`, which the grammar also
//! accepts back, and a zero metric is omitted. `scope` and `src` are absent
//! because `UserRoute` carries no such fields.

use slopos_abi::net::{NET_IFINDEX_NONE, NET_Q_ROUTES, UserRoute, UserRouteReq};
use slopos_net_core::Ipv4;
use slopos_net_core::ip_plan::RouteDest;
use slopos_net_core::render::route_origin;

use super::{Failure, Outcome, device_index, load_ifaces, warn_truncated};
use crate::net_query as query;
use crate::syscall::net::net_route_ctl;

pub fn show(dev: Option<&[u8]>) -> Outcome {
    let ifaces = load_ifaces("route")?;
    let filter = match dev {
        Some(name) => Some(device_index(&ifaces, name)?),
        None => None,
    };

    let q = query::fetch::<UserRoute>(NET_Q_ROUTES, filter.unwrap_or(NET_IFINDEX_NONE))
        .map_err(|err| Failure::from_errno("route", err))?;
    if q.truncated() {
        warn_truncated("route", q.records.len(), q.hdr.total_count);
    }

    for route in &q.records {
        print_route(route, &ifaces);
    }
    Ok(())
}

pub fn change(dest: RouteDest, via: Option<Ipv4>, dev: Option<&[u8]>, adding: bool) -> Outcome {
    let ifaces = load_ifaces("route")?;

    let mut req = UserRouteReq::default();
    match dest {
        RouteDest::Default => {
            req.prefix = [0, 0, 0, 0];
            req.prefix_len = 0;
        }
        RouteDest::Prefix(prefix) => {
            req.prefix = prefix.addr.octets();
            req.prefix_len = prefix.prefix_len;
        }
    }
    if let Some(gateway) = via {
        req.gateway = gateway.octets();
    }
    if let Some(name) = dev {
        req.ifindex = device_index(&ifaces, name)?;
    }

    net_route_ctl(&req, adding).map_err(|err| Failure::from_errno("route", err))
}

/// `default via 10.0.2.2 dev eth0 proto dhcp metric 100`
/// `10.0.2.0/24 dev eth0 proto kernel`
fn print_route(route: &UserRoute, ifaces: &query::Ifaces) {
    let mut line = std::string::String::new();
    let gateway = Ipv4(route.gateway);

    if route.prefix_len == 0 && Ipv4(route.prefix).is_unspecified() {
        line.push_str("default");
    } else {
        line.push_str(&std::format!("{}/{}", Ipv4(route.prefix), route.prefix_len));
    }
    if !gateway.is_unspecified() {
        line.push_str(&std::format!(" via {gateway}"));
    }
    line.push_str(&std::format!(
        " dev {}",
        query::name_or_index(ifaces, route.ifindex)
    ));
    line.push_str(&std::format!(" proto {}", route_origin(route.origin)));
    if route.metric != 0 {
        line.push_str(&std::format!(" metric {}", route.metric));
    }
    println!("{line}");
}
