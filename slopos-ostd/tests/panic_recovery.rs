//! Host-side tests for `slopos_ostd::sync::panic_recovery`.
//!
//! `poison_all_held_locks() -> !` halts forever via `cli; hlt`, so we
//! can't invoke it directly under `cargo test`. Instead the tests
//! exercise the load-bearing primitive the wrapper builds on:
//! `lock_tracking::poison_unlock_all_held()`. The wrapper's halt-suffix
//! is verified at the type-system level (the signature is
//! `pub fn poison_all_held_locks() -> !`).
//!
//! Coverage: at least two locks pushed onto the per-CPU held-lock
//! stack, both observed by the poison-walk, and the stack rewound to
//! depth 0. Mirrors the acceptance floor of "≥ 2 held locks across
//! CPUs" — host-side we use one CPU (slot 0) since the per-CPU register
//! isn't installed in user-space, but the walk-and-poison invariant
//! is the same shape as the kernel-side panic recovery.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use slopos_ostd::sync::lock_tracking::{
    LOCK_LEVEL_UNORDERED, enable_lock_tracking, held_lock_count, poison_unlock_all_held, pop_lock,
    push_lock,
};

/// Serialises every test that touches the per-CPU held-lock stack.
/// `cargo test` parallelises `#[test]` items by default; the per-CPU
/// stack is process-global on host so concurrent pushes from
/// independent tests would interleave and confuse the depth checks.
static LOCK_LOCK: Mutex<()> = Mutex::new(());

// Counters incremented by the synthetic poison callbacks so the
// walk-order invariant can be checked from outside.
static POISON_A_COUNT: AtomicUsize = AtomicUsize::new(0);
static POISON_B_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAST_POISON: AtomicUsize = AtomicUsize::new(0);

const TAG_A: usize = 1;
const TAG_B: usize = 2;

unsafe fn poison_a(_addr: *const ()) {
    POISON_A_COUNT.fetch_add(1, Ordering::Relaxed);
    LAST_POISON.store(TAG_A, Ordering::Relaxed);
}

unsafe fn poison_b(_addr: *const ()) {
    POISON_B_COUNT.fetch_add(1, Ordering::Relaxed);
    LAST_POISON.store(TAG_B, Ordering::Relaxed);
}

/// Reset the lock-tracking state between tests. `enable_lock_tracking`
/// only flips an atomic; the per-CPU slot is process-global so we
/// drain it manually.
fn reset_held_stack() {
    // Drain whatever's there from prior tests by popping until empty.
    // Each pop is keyed by the synthetic addresses we registered.
    while held_lock_count() > 0 {
        unsafe {
            pop_lock(0x1 as *const ());
            pop_lock(0x2 as *const ());
            pop_lock(0x3 as *const ());
        }
        // Safety net: if the held entries weren't ours, do a
        // poison-walk to clear them.
        if held_lock_count() > 0 {
            unsafe {
                poison_unlock_all_held();
            }
            break;
        }
    }
    POISON_A_COUNT.store(0, Ordering::Relaxed);
    POISON_B_COUNT.store(0, Ordering::Relaxed);
    LAST_POISON.store(0, Ordering::Relaxed);
}

#[test]
fn poison_walk_fires_each_held_lock_callback() {
    let _g = LOCK_LOCK.lock().unwrap();
    enable_lock_tracking();
    reset_held_stack();

    let addr_a = 0xAAAA_AAAA_usize as *const ();
    let addr_b = 0xBBBB_BBBB_usize as *const ();

    unsafe {
        push_lock(addr_a, poison_a, LOCK_LEVEL_UNORDERED);
        push_lock(addr_b, poison_b, LOCK_LEVEL_UNORDERED);
    }
    assert_eq!(held_lock_count(), 2);

    unsafe {
        poison_unlock_all_held();
    }

    // Both poison fns must have fired exactly once each.
    assert_eq!(POISON_A_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(POISON_B_COUNT.load(Ordering::Relaxed), 1);
    // The walk runs in reverse (innermost lock first) so B fires
    // before A; the last-poison tag is A.
    assert_eq!(LAST_POISON.load(Ordering::Relaxed), TAG_A);
    // The stack must be empty after the walk.
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn poison_walk_empty_stack_is_noop() {
    let _g = LOCK_LOCK.lock().unwrap();
    enable_lock_tracking();
    reset_held_stack();

    assert_eq!(held_lock_count(), 0);
    unsafe {
        poison_unlock_all_held();
    }
    assert_eq!(held_lock_count(), 0);
    assert_eq!(POISON_A_COUNT.load(Ordering::Relaxed), 0);
    assert_eq!(POISON_B_COUNT.load(Ordering::Relaxed), 0);
}

#[test]
fn poison_all_held_locks_signature_is_never_returning() {
    // Type-level assertion that the safe wrapper has the documented
    // `-> !` signature. The function is exposed at the sync module
    // root via the re-export in `slopos-ostd/src/sync/mod.rs`. We
    // never invoke it under cargo test (it halts forever via `cli;
    // hlt` on x86_64 / `spin_loop` on host) — pinning the signature
    // here protects against accidental return-type drift.
    let _: fn() -> ! = slopos_ostd::sync::poison_all_held_locks;
}
