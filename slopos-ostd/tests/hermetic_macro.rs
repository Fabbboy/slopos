#![feature(allocator_api)]

//! Host-side tests for the `hermetic_state!` function-like macro: the impl it
//! emits, and that the `.hermetic_state_registry` static it also emits
//! compiles. The linker section itself is not walkable under `cargo test`.

use core::sync::atomic::{AtomicU64, Ordering};

#[allow(unused_imports)]
use slopos_ostd::AllocError;
use slopos_ostd::hermetic_state;
use slopos_ostd::test_support::hermetic::HermeticState;

static GLOBAL_VALUE: AtomicU64 = AtomicU64::new(0xABCD);

hermetic_state! {
    pub PlainState {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(GLOBAL_VALUE.load(Ordering::Acquire))
        }
        fn restore(snap: Self::Snapshot) {
            GLOBAL_VALUE.store(snap, Ordering::Release);
        }
    }
}

#[test]
fn plain_state_macro_emits_working_impl() {
    GLOBAL_VALUE.store(0x1234, Ordering::Release);
    let snap = <PlainState as HermeticState>::snapshot().unwrap();
    assert_eq!(snap, 0x1234);

    GLOBAL_VALUE.store(0x9999, Ordering::Release);
    // SAFETY: restore runs outside a real KernelTestScope drop, but its
    // single-writer contract holds — `#[test]` bodies are single-writer.
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

hermetic_state! {
    pub DependentState {
        type Snapshot = u64;
        const DEPENDS_ON: &[&str] = &["PlainState"];
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(0)
        }
        fn restore(_snap: Self::Snapshot) {}
    }
}

#[test]
fn depends_on_propagates_to_const_item() {
    let deps = <DependentState as HermeticState>::DEPENDS_ON;
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], "PlainState");
    assert_eq!(<DependentState as HermeticState>::NAME, "DependentState");
}

static COUNTER: AtomicU64 = AtomicU64::new(42);

hermetic_state! {
    pub CounterState {
        type Snapshot = u64;
        fn snapshot() -> Result<Self::Snapshot, AllocError> {
            Ok(COUNTER.load(Ordering::Acquire))
        }
        fn restore(snap: Self::Snapshot) {
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
    // Compile-time only: a macro emitting the wrong shape fails to build.
    fn assert_send_static<S: HermeticState>()
    where
        S::Snapshot: Send + 'static,
    {
    }
    assert_send_static::<PlainState>();
    assert_send_static::<DependentState>();
    assert_send_static::<CounterState>();
}
