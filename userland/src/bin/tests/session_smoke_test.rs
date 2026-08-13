#![feature(restricted_std)]

//! A deterministic desktop-shaped resource population, so the quota gate has
//! something real to measure.
//!
//! Under `tests=on`, `/sbin/init` calls `run_userland_tests()` and then
//! `exit_with_code(0)`; the compositor, shell and terminal spawns are all
//! *below* that exit. So no automated boot has a compositor listen socket,
//! client AF_UNIX pairs, or a desktop descriptor population — which are
//! precisely the tight resources the ceilings have to be derived from. Without
//! this binary the gate's numbers describe a kernel running its own unit
//! tests, and claiming they describe a desktop session would be false.
//!
//! What it does *not* do is assert a number. Peaks are a measurement, and this
//! is the thing being measured; the gate file holds the values and
//! `check_quota_headroom.sh` compares against them. What this asserts is that
//! the population was actually built — a smoke test that silently created
//! nothing would let the gate record a peak of zero and call it a pass.

use slopos_userland as _;

use std::fs::File;
use std::io::Write;

use slopos_userland::syscall::process;

/// Descriptors one client of a desktop session plausibly holds open at once:
/// a few files, a socket, a pipe pair. Small enough that N of them fit under
/// the tightest per-process ceiling, large enough that the peak is not noise.
const FILES_PER_CLIENT: usize = 6;

/// Concurrent clients. The compositor's real peak is what the gate wants, and
/// it scales with this.
const CLIENTS: usize = 8;

/// Hold a descriptor population open, all at once rather than serially.
///
/// Serial opens would leave a peak of one however many times the loop ran:
/// the peak is a high-water mark, so the descriptors have to be live
/// simultaneously to move it.
fn open_population() -> Option<Vec<File>> {
    let mut held = Vec::new();
    for i in 0..FILES_PER_CLIENT {
        let path = format!("/tmp/session_smoke_{i}");
        let mut f = File::create(&path).ok()?;
        f.write_all(b"session smoke").ok()?;
        held.push(f);
    }
    Some(held)
}

/// A descriptor population is built and held simultaneously.
fn holds_a_descriptor_population() -> bool {
    let Some(held) = open_population() else {
        return false;
    };
    let ok = held.len() == FILES_PER_CLIENT;
    drop(held);
    ok
}

/// Several processes hold their own populations at once.
///
/// This is the cross-process shape the per-principal ceilings exist for: N
/// accounts each at their own peak, summing into the root's. A single process
/// opening N times measures one row and says nothing about the tree.
fn spawns_concurrent_clients() -> bool {
    let mut children = Vec::new();
    for _ in 0..CLIENTS {
        let tid = process::spawn_path("/bin/cd_test");
        if tid <= 0 {
            // A refused spawn is a real failure here: the population this
            // exists to create would be smaller than the gate recorded.
            for tid in children {
                process::waitpid(tid as u32);
            }
            return false;
        }
        children.push(tid);
    }
    // Reaped only after every child exists, so their accounts are live at the
    // same moment and the root's peak sees the sum.
    let mut reaped = 0usize;
    for tid in children {
        process::waitpid(tid as u32);
        reaped += 1;
    }
    reaped == CLIENTS
}

/// The session's own descriptors survive the concurrent spawns.
///
/// Catches the failure the quota could plausibly introduce: a child's charges
/// billed to the parent's account would make the parent's own opens start
/// failing once enough children existed.
fn population_survives_concurrent_children() -> bool {
    let Some(held) = open_population() else {
        return false;
    };
    let spawned = spawns_concurrent_clients();
    let still_open = File::create("/tmp/session_smoke_after").is_ok();
    drop(held);
    spawned && still_open
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "holds_a_descriptor_population",
            holds_a_descriptor_population,
        ),
        ("spawns_concurrent_clients", spawns_concurrent_clients),
        (
            "population_survives_concurrent_children",
            population_survives_concurrent_children,
        ),
    ]);
}
