//! Host-side tests for `slopos_ostd::sync::lock_graph`.
//!
//! Exercises the runtime dependency-graph + cycle-detection validator.
//! The tests use synthetic lock addresses (not real `SpinLock<T>`
//! instances) so we can drive `push_lock` / `pop_lock` directly
//! without ticket-lock interaction.
//!
//! Coverage:
//! - Class registration: each unique address gets a distinct class.
//! - Ascending order across levels is accepted (no panic).
//! - Same-level distinct classes are accepted as long as the order is
//!   consistent (no cycle).
//! - AB-BA cycle detection: acquiring A→B then B→A on different chains
//!   triggers the cycle report.
//! - Chain-hash cache hit: re-acquiring a previously-validated chain
//!   prefix is fast-pathed (smoke-tested via held_lock_count + no panic).
//! - Panic-bypass: `enter_panic_bypass()` suppresses ordering checks
//!   while keeping the held-stack walk active.

use std::panic;
use std::sync::Mutex;

use slopos_ostd::sync::lock_graph::{
    LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, enable_lock_tracking,
    enter_panic_bypass, held_lock_count, poison_unlock_all_held, pop_lock, push_lock,
    reset_for_test,
};

/// Serialise every test that touches the global graph state.
/// `cargo test` parallelises `#[test]` items by default; the class
/// table / edge pool / chain cache are process-global so concurrent
/// tests would interleave registrations.
static LOCK_LOCK: Mutex<()> = Mutex::new(());

unsafe fn noop_poison(_addr: *const ()) {}

fn setup() {
    reset_for_test();
    enable_lock_tracking();
}

#[test]
fn ascending_levels_accepted() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    let a = 0x1001_usize as *const ();
    let b = 0x1002_usize as *const ();
    let c = 0x1003_usize as *const ();

    unsafe {
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(b, noop_poison, LOCK_LEVEL_REGISTRY);
        push_lock(c, noop_poison, LOCK_LEVEL_ALLOCATOR);
    }
    assert_eq!(held_lock_count(), 3);
    unsafe {
        pop_lock(c);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn same_level_distinct_classes_accepted() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    // Two distinct lock instances at the same RESOURCE level. Under the
    // old strict-level rule this would panic; under the cycle-detection
    // model it's accepted as long as the order is consistent.
    let a = 0x2001_usize as *const ();
    let b = 0x2002_usize as *const ();

    unsafe {
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(b, noop_poison, LOCK_LEVEL_RESOURCE);
    }
    assert_eq!(held_lock_count(), 2);
    unsafe {
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn ab_then_ba_detects_cycle() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    let a = 0x3001_usize as *const ();
    let b = 0x3002_usize as *const ();

    // First chain: A then B. Establishes the edge A -> B.
    unsafe {
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(b, noop_poison, LOCK_LEVEL_RESOURCE);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);

    // Second chain: B then A. Would establish B -> A, closing a cycle.
    // The cycle detector should panic when we try to push A while B is
    // held.
    let result = panic::catch_unwind(|| unsafe {
        push_lock(b, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
    });
    assert!(
        result.is_err(),
        "expected cycle detection to panic on B->A after A->B"
    );

    // After the catch, clean up the stuck B entry (push_lock may have
    // pushed B before panicking on A's check).
    unsafe {
        poison_unlock_all_held();
    }
}

#[test]
fn chain_hash_cache_repeated_chain_is_fast() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    let a = 0x4001_usize as *const ();
    let b = 0x4002_usize as *const ();

    // Acquire the same chain (A, B) 100 times. After the first pass the
    // chain key is cached; subsequent passes should hit the chain-hash
    // and skip the BFS. Smoke-tested by absence of panic + correct
    // depth tracking — actual chain-hash hit-rate measurement would
    // need instrumentation we haven't exposed.
    for _ in 0..100 {
        unsafe {
            push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
            push_lock(b, noop_poison, LOCK_LEVEL_REGISTRY);
        }
        assert_eq!(held_lock_count(), 2);
        unsafe {
            pop_lock(b);
            pop_lock(a);
        }
        assert_eq!(held_lock_count(), 0);
    }
}

#[test]
fn panic_bypass_suppresses_ordering_check() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    let a = 0x5001_usize as *const ();
    let b = 0x5002_usize as *const ();

    // Establish A -> B.
    unsafe {
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(b, noop_poison, LOCK_LEVEL_RESOURCE);
        pop_lock(b);
        pop_lock(a);
    }

    // Enter panic bypass: subsequent acquires must not panic even on
    // ordering violations (Inv. 9 relaxes lock discipline during fatal
    // abort).
    enter_panic_bypass();

    // B -> A would normally be a cycle; with bypass active it should
    // not panic.
    let result = panic::catch_unwind(|| unsafe {
        push_lock(b, noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(a, noop_poison, LOCK_LEVEL_RESOURCE);
    });
    assert!(
        result.is_ok(),
        "panic bypass should suppress cycle detection"
    );

    // Walk to clean up the held stack.
    unsafe {
        poison_unlock_all_held();
    }
}

#[test]
fn held_stack_walk_after_chain_acquisition() {
    let _g = LOCK_LOCK.lock().unwrap();
    setup();

    let addrs = [
        0x6001_usize as *const (),
        0x6002_usize as *const (),
        0x6003_usize as *const (),
    ];

    // Acquire three locks in ascending levels (no cycle possible).
    unsafe {
        push_lock(addrs[0], noop_poison, LOCK_LEVEL_RESOURCE);
        push_lock(addrs[1], noop_poison, LOCK_LEVEL_REGISTRY);
        push_lock(addrs[2], noop_poison, LOCK_LEVEL_ALLOCATOR);
    }
    assert_eq!(held_lock_count(), 3);

    // Poison-walk drains the stack.
    unsafe {
        poison_unlock_all_held();
    }
    assert_eq!(held_lock_count(), 0);
}
