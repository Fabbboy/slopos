#![feature(allocator_api)]

//! Host-side tests for the `hermetic_state!` function-like macro.
//!
//! Covers:
//!
//! - The macro emits a working `unsafe impl HermeticState` body.
//! - `Foo::NAME` is the type's identifier (no manual literal needed).
//! - `snapshot()` and `restore()` round-trip a value.
//! - The optional `const DEPENDS_ON` line is honoured when present.
//! - The macro also expands `__hermetic_register!`, which emits a
//!   `#[link_section = ".hermetic_state_registry"]` static. We can't
//!   walk the linker section under `cargo test`, but the static must
//!   compile.
//!
//! These tests live in OSTD because the macro itself does; the
//! 13-site kernel-side migration in `core/src/scheduler/test_hermetic.rs`
//! is verified by the full `just test` suite, not here.

use core::sync::atomic::{AtomicU64, Ordering};

#[allow(unused_imports)]
use slopos_ostd::AllocError;
use slopos_ostd::hermetic_state;
use slopos_ostd::test_support::hermetic::HermeticState;

// Backing store the synthetic impls snapshot/restore.
static GLOBAL_VALUE: AtomicU64 = AtomicU64::new(0xABCD);

// ----------------------------------------------------------------------------
// 1. Plain block: type + Snapshot + snapshot + restore.
// ----------------------------------------------------------------------------

hermetic_state! {
    pub PlainState {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(GLOBAL_VALUE.load(Ordering::Acquire))
        }
        unsafe fn restore(snap: Self::Snapshot) {
            GLOBAL_VALUE.store(snap, Ordering::Release);
        }
    }
}

#[test]
fn plain_state_macro_emits_working_impl() {
    GLOBAL_VALUE.store(0x1234, Ordering::Release);
    let snap = <PlainState as HermeticState>::snapshot().unwrap();
    assert_eq!(snap, 0x1234);

    // Mutate, then restore from snapshot.
    GLOBAL_VALUE.store(0x9999, Ordering::Release);
    // SAFETY: synthetic test — restore is called outside a real
    // KernelTestScope drop, but the contract is "single-writer
    // panic-or-test path", and `cargo test` is single-writer per
    // test by `#[test]` semantics.
    unsafe {
        <PlainState as HermeticState>::restore(snap);
    }
    assert_eq!(GLOBAL_VALUE.load(Ordering::Acquire), 0x1234);
}

#[test]
fn plain_state_name_matches_type_ident() {
    assert_eq!(<PlainState as HermeticState>::NAME, "PlainState");
}

#[test]
fn plain_state_depends_on_defaults_to_empty() {
    assert_eq!(<PlainState as HermeticState>::DEPENDS_ON.len(), 0);
}

// ----------------------------------------------------------------------------
// 2. Block with `const DEPENDS_ON` populated.
// ----------------------------------------------------------------------------

hermetic_state! {
    pub DependentState {
        type Snapshot = u64;
        const DEPENDS_ON: &[&str] = &["PlainState"];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(0)
        }
        unsafe fn restore(_snap: Self::Snapshot) {}
    }
}

#[test]
fn depends_on_propagates_to_const_item() {
    let deps = <DependentState as HermeticState>::DEPENDS_ON;
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], "PlainState");
    assert_eq!(<DependentState as HermeticState>::NAME, "DependentState");
}

// ----------------------------------------------------------------------------
// 3. A more typical kernel-shape impl: state held in an atomic, snapshot
//    captures the value, restore re-stores it.
// ----------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(42);

hermetic_state! {
    pub CounterState {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(COUNTER.load(Ordering::Acquire))
        }
        unsafe fn restore(snap: Self::Snapshot) {
            COUNTER.store(snap, Ordering::Release);
        }
    }
}

#[test]
fn counter_state_round_trips_through_snapshot_restore() {
    COUNTER.store(100, Ordering::Release);
    let snap = <CounterState as HermeticState>::snapshot().unwrap();
    COUNTER.store(200, Ordering::Release);
    // SAFETY: see plain_state_macro_emits_working_impl.
    unsafe {
        <CounterState as HermeticState>::restore(snap);
    }
    assert_eq!(COUNTER.load(Ordering::Acquire), 100);
}

#[test]
fn associated_types_are_send_static() {
    // The trait requires `Snapshot: Send + 'static`. If the macro
    // emits a wrong shape this won't compile.
    fn assert_send_static<S: HermeticState>()
    where
        S::Snapshot: Send + 'static,
    {
    }
    assert_send_static::<PlainState>();
    assert_send_static::<DependentState>();
    assert_send_static::<CounterState>();
}
