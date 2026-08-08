//! `ip dns` — resolver configuration.
//!
//! Neither verb is short-circuited: `show` issues `NET_Q_RESOLVER` and `set`
//! issues `net_resolver_set`, and both surface whatever error comes back rather
//! than printing an empty resolver list, which would read as "no nameservers
//! configured".

use slopos_abi::net::{NET_IFINDEX_NONE, NET_Q_RESOLVER, UserResolver, UserResolverReq};
use slopos_net_core::Ipv4;

use super::{Failure, Outcome};
use crate::net_query as query;
use crate::syscall::net::net_resolver_set;

pub fn show() -> Outcome {
    let q = query::fetch::<UserResolver>(NET_Q_RESOLVER, NET_IFINDEX_NONE)
        .map_err(|err| Failure::from_errno("dns", err))?;

    for cfg in &q.records {
        let count = (cfg.n_servers as usize).min(cfg.servers.len());
        for server in &cfg.servers[..count] {
            println!("nameserver {}", Ipv4(*server));
        }
        println!("timeout {}ms  attempts {}", cfg.timeout_ms, cfg.attempts);
    }
    Ok(())
}

pub fn set(servers: &[Ipv4]) -> Outcome {
    let mut req = UserResolverReq::default();
    let count = servers.len().min(req.servers.len());
    for (slot, server) in req.servers[..count].iter_mut().zip(servers) {
        *slot = server.octets();
    }
    req.n_servers = count as u8;
    // A caller naming servers explicitly means them to outrank whatever a lease
    // later offers, which is what the STATIC source records.
    req.source = slopos_abi::net::NET_RESOLVER_SRC_STATIC;

    net_resolver_set(&req).map_err(|err| Failure::from_errno("dns", err))
}
