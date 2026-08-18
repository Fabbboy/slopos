//! Host-side tests for `slopos_ostd::sync::panic_recovery`.
//!
//! `poison_all_held_locks() -> !` halts forever, so these exercise the
//! primitive it builds on, `lock_tracking::poison_unlock_all_held()`, and
//! pin the wrapper's signature at the type level.

use core::sync::atomic::{AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use std::sync::Mutex;

use slopos_ostd::sync::lock_tracking::{
    LOCK_LEVEL_UNORDERED, enable_lock_tracking, held_lock_count, poison_unlock_all_held, pop_lock,
    push_lock,
};

/// Serialises every test that touches the per-CPU held-lock stack: it is
/// process-global on host, and `cargo test` runs tests in parallel.
static LOCK_LOCK: Mutex<()> = Mutex::new(());

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

/// Drain the process-global per-CPU slot between tests;
/// `enable_lock_tracking` only flips an atomic.
fn reset_held_stack() {
    while held_lock_count() > 0 {
        unsafe {
            pop_lock(core::ptr::without_provenance::<()>(0x1));
            pop_lock(core::ptr::without_provenance::<()>(0x2));
            pop_lock(core::ptr::without_provenance::<()>(0x3));
        }
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

    let addr_a = core::ptr::without_provenance::<()>(0xAAAA_AAAA);
    let addr_b = core::ptr::without_provenance::<()>(0xBBBB_BBBB);

    unsafe {
        push_lock(addr_a, poison_a, lock_class!("pr.a", LOCK_LEVEL_UNORDERED));
        push_lock(addr_b, poison_b, lock_class!("pr.b", LOCK_LEVEL_UNORDERED));
    }
    assert_eq!(held_lock_count(), 2);

    unsafe {
        poison_unlock_all_held();
    }

    assert_eq!(POISON_A_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(POISON_B_COUNT.load(Ordering::Relaxed), 1);
    // The walk runs innermost-first, so B fires before A.
    assert_eq!(LAST_POISON.load(Ordering::Relaxed), TAG_A);
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
    // Never invoked: it halts forever. Pinning the type catches
    // return-type drift.
    let _: fn() -> ! = slopos_ostd::sync::poison_all_held_locks;
}
