#![feature(restricted_std)]

//! Name resolution must actually resolve.
//!
//! Everything else in the suite that touches the network dials a literal IP —
//! `curl_e2e_test` picks from a hardcoded list precisely so a resolver problem
//! cannot mask a TCP problem. That left the resolver itself untested, and a
//! `-netdev` option that pointed the guest at a nameserver SLIRP does not
//! answer on went unnoticed: every `ping`/`curl` by name timed out after nine
//! seconds while `ip a` and the whole GUI stack looked healthy.
//!
//! This test is about the *resolver*, not about any one name: it asserts a
//! lookup completes quickly and yields a routable address, which is false both
//! when no nameserver is reachable and when one is configured but never
//! replies.
//!
//! **Known-failing in a full-suite boot, and deliberately not excused.**
//! Running the kernel-phase AF_UNIX socket tests first leaves UDP DNS dead for
//! the rest of the run: `test_unix_socket_send_recv_basic` alone reproduces
//! it, the queries go out, and no reply ever reaches `udp::handle_rx` — so the
//! loss is below the UDP layer, not in the resolver's expectation check. It
//! reproduces identically on a tree from before this test existed, so it is
//! older than the resolver work and is tracked separately. `just test
//! '*dns_resolve*'` is green, which is what proves the resolver itself works.

use slopos_userland as _;

use slopos_userland::net::{ResolveError, resolve_host};

use std::time::Instant;

/// Well past a working SLIRP stub (single-digit milliseconds) and well under
/// the resolver's own retry budget, so a timeout fails here rather than
/// stalling the suite.
const BUDGET_MS: u128 = 4_000;

/// Several names, because one domain being unresolvable upstream is a fact
/// about the internet rather than about SlopOS. Every one failing is the
/// resolver.
const HOSTS: [&str; 3] = ["example.com", "cloudflare.com", "google.com"];

/// 0.0.0.0/8, 127/8 and the broadcast address parse but cannot be dialled;
/// accepting one would let a stub that echoes garbage pass.
fn is_routable(addr: [u8; 4]) -> bool {
    !(addr[0] == 0 || addr[0] == 127 || addr == [255, 255, 255, 255])
}

fn resolver_answers_a_name() -> bool {
    let mut last_err = None;
    for host in HOSTS {
        let started = Instant::now();
        match resolve_host(host) {
            Ok(addr) => {
                let elapsed = started.elapsed().as_millis();
                let octets = addr.0;
                if !is_routable(octets) {
                    eprintln!(
                        "dns_resolve: {host} -> {}.{}.{}.{} is not a dialable address",
                        octets[0], octets[1], octets[2], octets[3]
                    );
                    return false;
                }
                if elapsed > BUDGET_MS {
                    eprintln!("dns_resolve: {host} resolved but took {elapsed}ms");
                    return false;
                }
                eprintln!(
                    "dns_resolve: {host} -> {}.{}.{}.{} in {elapsed}ms",
                    octets[0], octets[1], octets[2], octets[3]
                );
                return true;
            }
            Err(e) => {
                eprintln!("dns_resolve: {host} failed: {e}");
                last_err = Some(e);
            }
        }
    }
    match last_err {
        Some(ResolveError::NoDnsServer) => {
            eprintln!("dns_resolve: no nameserver was configured — DHCP did not supply one");
        }
        Some(ResolveError::Transient) => {
            eprintln!(
                "dns_resolve: every query timed out — the configured nameserver is not \
                 answering. QEMU's `dns=` names the guest-visible address of its own \
                 stub, not an upstream to forward to, so setting it to a public \
                 resolver moves the stub somewhere nothing replies from."
            );
        }
        _ => {}
    }
    false
}

/// An IP literal must not reach the resolver at all. This stays true even
/// where DNS is unavailable, so it separates "the resolver is broken" from
/// "this binary cannot parse an address".
fn ip_literals_bypass_the_resolver() -> bool {
    match resolve_host("10.0.2.15") {
        Ok(addr) if addr.0 == [10, 0, 2, 15] => true,
        Ok(addr) => {
            eprintln!("dns_resolve: literal parsed as {:?}", addr.0);
            false
        }
        Err(e) => {
            eprintln!("dns_resolve: literal rejected: {e}");
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "ip_literals_bypass_the_resolver",
            ip_literals_bypass_the_resolver,
        ),
        ("resolver_answers_a_name", resolver_answers_a_name),
    ]);
}
