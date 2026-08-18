#![feature(restricted_std)]

//! A deterministic desktop-shaped resource population, so the quota gate has
//! something real to measure.
//!
//! Under `tests=on`, `/sbin/init` exits above the compositor, shell and
//! terminal spawns, so no automated boot otherwise holds a desktop descriptor
//! population. Asserts only that the population was built, never a number:
//! the gate file holds the peaks and `check_quota_headroom.sh` compares them.

use slopos_userland as _;

use std::fs::File;
use std::io::Write;

use slopos_userland::syscall::process;

/// Small enough that N of them fit under the tightest per-process ceiling,
/// large enough that the peak is not noise.
const FILES_PER_CLIENT: usize = 6;

const CLIENTS: usize = 8;

/// Descriptors must be live simultaneously to move a high-water mark; serial
/// opens would leave a peak of one however many times the loop ran.
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

fn holds_a_descriptor_population() -> bool {
    let Some(held) = open_population() else {
        return false;
    };
    let ok = held.len() == FILES_PER_CLIENT;
    drop(held);
    ok
}

/// The cross-process shape the per-principal ceilings exist for: N accounts
/// each at their own peak, summing into the root's.
fn spawns_concurrent_clients() -> bool {
    let mut children = Vec::new();
    for _ in 0..CLIENTS {
        let tid = process::spawn_path("/bin/cd_test");
        if tid <= 0 {
            // A refused spawn would leave the population smaller than the peak
            // the gate records.
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

/// Catches a child's charges being billed to the parent's account, which would
/// make the parent's own opens start failing once enough children existed.
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
