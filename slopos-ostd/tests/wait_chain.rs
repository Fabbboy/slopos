//! The wait-for cycle walk.
//!
//! Acquiring a `SpinLock` is not possible here — `PreemptGuard::new` does
//! an unguarded `gs:`-relative RMW — so holder attribution is pinned
//! in-kernel by `boot/src/tests/watchdog_tests.rs`. What is testable here
//! is the graph, which is pure CPU indices.
//!
//! Each test owns distinct CPU indices: the graph is a process-global
//! array and `cargo test` runs integration tests on parallel threads.

use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};
use slopos_ostd::watchdog::test_support::{clear_wait, plant_wait, reset_slot};
use slopos_ostd::watchdog::wait_chain_closes_cycle;

#[test]
fn an_untaken_lock_names_no_holder() {
    let lock: SpinLock<u32> = SpinLock::new(0, LOCK_LEVEL_UNORDERED);
    // A zeroed holder field decodes as "CPU 0, ticket 0", which is exactly
    // what a virgin lock's ticket pair also reads as. Both conjuncts of the
    // validation exist to reject this.
    assert_eq!(lock.holder_cpu_for_test(), None);
}

#[test]
fn a_two_cpu_cycle_closes() {
    const A: usize = 20;
    const B: usize = 21;
    reset_slot(A);
    reset_slot(B);

    plant_wait(A, Some(B), 0xAAAA);
    plant_wait(B, Some(A), 0xBBBB);

    assert!(wait_chain_closes_cycle(A));
    assert!(wait_chain_closes_cycle(B));

    clear_wait(A);
    clear_wait(B);
}

#[test]
fn a_self_cycle_closes() {
    const A: usize = 22;
    reset_slot(A);
    plant_wait(A, Some(A), 0xCCCC);
    assert!(wait_chain_closes_cycle(A));
    clear_wait(A);
}

#[test]
fn a_chain_that_leaves_the_graph_is_not_a_cycle() {
    const A: usize = 23;
    const B: usize = 24;
    const C: usize = 25;
    reset_slot(A);
    reset_slot(B);
    reset_slot(C);

    // C is not spinning at all — the holder is stuck somewhere the graph
    // cannot describe, which is not the same answer as "no cycle".
    plant_wait(A, Some(B), 1);
    plant_wait(B, Some(C), 2);

    assert!(!wait_chain_closes_cycle(A));

    clear_wait(A);
    clear_wait(B);
}

#[test]
fn a_link_that_left_its_wait_breaks_the_cycle() {
    const A: usize = 26;
    const B: usize = 27;
    reset_slot(A);
    reset_slot(B);

    plant_wait(A, Some(B), 1);
    plant_wait(B, Some(A), 2);
    assert!(wait_chain_closes_cycle(A));

    // B won its lock and moved on. The edge it published is stale, and a
    // walk that still believed it would print a cycle that never existed.
    clear_wait(B);
    assert!(!wait_chain_closes_cycle(A));

    clear_wait(A);
}

#[test]
fn a_long_acyclic_chain_is_bounded_and_rejected() {
    // Longer than MAX_WAIT_HOPS, so the walk stops without a verdict of
    // "cycle" rather than following it forever.
    const BASE: usize = 30;
    const LEN: usize = 12;
    for i in 0..LEN {
        reset_slot(BASE + i);
    }
    for i in 0..LEN - 1 {
        plant_wait(BASE + i, Some(BASE + i + 1), i as u64);
    }

    assert!(!wait_chain_closes_cycle(BASE));

    for i in 0..LEN {
        clear_wait(BASE + i);
    }
}
