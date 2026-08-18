//! `nmap` — local-segment host discovery.
//!
//! Sends one throwaway UDP datagram per candidate — anything addressed to the
//! local prefix forces neighbour resolution — then reads the answers out of
//! `NET_Q_NEIGH`. Unprivileged, and the waiting happens in this process rather
//! than inside a syscall.

use slopos_abi::net::{
    NET_ADDR_SCOPE_GLOBAL, NET_IFINDEX_NONE, NET_NEIGH_FAILED, NET_NEIGH_INCOMPLETE, NET_Q_ADDRS,
    NET_Q_NEIGH, UserAddr, UserNeigh,
};
use slopos_net_core::{Ipv4, Mac};

use crate::net_query as query;
use crate::syscall::net;

/// Discard port: the datagram is never meant to be answered, sending it is only
/// what pushes the address through neighbour resolution.
const PROBE_PORT: u16 = 9;

/// How long to let replies land, and how often to re-read the cache. ARP on a
/// local segment answers in single-digit milliseconds.
const SETTLE_ROUNDS: u32 = 10;
const SETTLE_MS: u32 = 100;

/// Cap on probes per scan, so a short prefix cannot turn one into a broadcast
/// storm. A /24 fits exactly.
const MAX_TARGETS: usize = 254;

/// Every host address in `local/prefix_len`, excluding the network and
/// broadcast addresses and our own.
fn candidates(local: [u8; 4], prefix_len: u8, out: &mut [[u8; 4]; MAX_TARGETS]) -> usize {
    if prefix_len < 24 || prefix_len > 30 {
        // Longer than /30 has no host addresses to probe; shorter than /24 is
        // more hosts than this is meant to be pointed at.
        return 0;
    }
    let host_bits = 32 - prefix_len as u32;
    let count = (1u32 << host_bits) - 2; // network and broadcast are not hosts
    let base = u32::from_be_bytes(local) & !((1u32 << host_bits) - 1);

    let mut n = 0usize;
    for i in 1..=count {
        if n >= out.len() {
            break;
        }
        let addr = (base | i).to_be_bytes();
        if addr == local {
            continue;
        }
        out[n] = addr;
        n += 1;
    }
    n
}

/// The first non-loopback global address the stack holds, with its prefix.
fn local_address() -> Option<(u32, [u8; 4], u8)> {
    let addrs = query::fetch::<UserAddr>(NET_Q_ADDRS, NET_IFINDEX_NONE).ok()?;
    addrs
        .records
        .iter()
        .find(|a| a.scope == NET_ADDR_SCOPE_GLOBAL as u8 && a.addr != [0; 4])
        .map(|a| (a.ifindex, a.addr, a.prefix_len))
}

fn probe(targets: &[[u8; 4]]) {
    let Ok(sock) = net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_DGRAM, 0) else {
        return;
    };
    for target in targets {
        let addr = slopos_abi::net::SockAddrIn {
            family: slopos_abi::net::AF_INET,
            port: PROBE_PORT.to_be(),
            addr: *target,
            _pad: [0; 8],
        };
        // Failures are expected: an unresolvable neighbour is itself an answer.
        let _ = net::sendto(sock.raw(), &[0u8; 1], 0, &addr);
    }
}

/// Neighbours that resolved to a MAC, deduplicated by address.
fn resolved() -> std::vec::Vec<UserNeigh> {
    let Ok(q) = query::fetch::<UserNeigh>(NET_Q_NEIGH, NET_IFINDEX_NONE) else {
        return std::vec::Vec::new();
    };
    q.records
        .into_iter()
        .filter(|n| n.state != NET_NEIGH_INCOMPLETE && n.state != NET_NEIGH_FAILED)
        .collect()
}

pub fn nmap_main() {
    let Some((ifindex, local, prefix_len)) = local_address() else {
        eprintln!("nmap: no usable address — is the link up and DHCP done?");
        std::process::exit(1);
    };

    let ifaces = query::Ifaces::fetch().ok();
    let name = match &ifaces {
        Some(ifaces) => query::name_or_index(ifaces, ifindex),
        None => std::format!("if{ifindex}"),
    };
    println!("nmap: interface {name} ip {}/{}", Ipv4(local), prefix_len);

    let mut targets = [[0u8; 4]; MAX_TARGETS];
    let n = candidates(local, prefix_len, &mut targets);
    if n == 0 {
        eprintln!("nmap: /{prefix_len} has no host range worth scanning");
        std::process::exit(1);
    }
    println!("nmap: probing {n} address(es)...");

    probe(&targets[..n]);

    // Re-read rather than sleeping once for the worst case: a segment that
    // answers immediately must not cost a full timeout.
    let mut hosts = std::vec::Vec::new();
    for _ in 0..SETTLE_ROUNDS {
        std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS as u64));
        hosts = resolved();
        if hosts.len() >= n {
            break;
        }
    }

    if hosts.is_empty() {
        eprintln!("nmap: no hosts answered");
        std::process::exit(1);
    }

    println!("nmap: discovered hosts");
    for host in &hosts {
        println!("host {}  mac {}", Ipv4(host.addr), Mac(host.mac));
    }
    std::process::exit(0);
}
