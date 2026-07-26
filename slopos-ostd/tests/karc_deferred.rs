//! Coverage for the deferred strong-release split
//! (`KArc::release_deferrable` / `destroy_deferred`, surfaced for tasks as
//! `task_release_strong` / `task_destroy_parked`).
//!
//! These back the task graveyard: a task's final reference may be released in a
//! context where the allocator-heavy destructor must not run (interrupts off, a
//! lock held, or on the dying task's own stack), so releasing the reference and
//! destroying the allocation become separate steps.
//!
//! The load-bearing property is that *finality is decided by the decrement*,
//! never by reading the count beforehand. A `strong_count == 1` pre-check is
//! racy — two holders can both observe two and both then drop — so the graveyard
//! could not rely on one. Here that shows up as: across racing releasers exactly
//! one gets `Some`, and it uniquely owns the allocation.
//!
//! The racing tests use a plain `Send` payload rather than `TaskInner`, which is
//! `!Send` by design (raw-pointer fields; in the kernel it crosses CPUs only
//! through the audited placement primitives). The property under test belongs to
//! the generic refcount, not to any one payload.
//!
//! Note `just check-miri` runs with `-Zmiri-ignore-leaks`, so a *leaked*
//! allocation is invisible here — every test asserts destructor counts
//! explicitly. A double destroy is a double free, which Miri does catch.

use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::KArc;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{task_destroy_parked, task_release_strong};

/// Miri interprets, so keep the racing iteration count small there.
#[cfg(miri)]
const ROUNDS: usize = 32;
#[cfg(not(miri))]
const ROUNDS: usize = 10_000;

struct DropCounter(StdArc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

fn counted() -> (KArc<DropCounter>, StdArc<AtomicUsize>) {
    let drops = StdArc::new(AtomicUsize::new(0));
    let arc = KArc::try_new(DropCounter(drops.clone())).expect("allocation");
    (arc, drops)
}

/// A release with another handle outstanding is not final and must not destroy;
/// the final one reports itself and destroys only on `destroy_deferred`.
#[test]
fn release_is_deferred_until_destroy() {
    let (arc, drops) = counted();
    let second = arc.clone();

    assert!(
        KArc::release_deferrable_for_test(arc).is_none(),
        "a release with another handle outstanding is not final"
    );
    assert_eq!(drops.load(Ordering::Acquire), 0);

    let node = KArc::release_deferrable_for_test(second).expect("final release reports itself");
    assert_eq!(
        drops.load(Ordering::Acquire),
        0,
        "release must not run the destructor"
    );

    // SAFETY: `node` is the result of exactly one `Some` release above.
    unsafe { KArc::destroy_deferred_for_test(node) };
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

/// The split must be observationally identical to dropping the `KArc`.
#[test]
fn split_release_matches_plain_drop() {
    let (arc, via_drop) = counted();
    drop(arc);

    let (arc, via_split) = counted();
    let node = KArc::release_deferrable_for_test(arc).expect("sole handle is final");
    // SAFETY: `node` is the result of exactly one `Some` release above.
    unsafe { KArc::destroy_deferred_for_test(node) };

    assert_eq!(via_drop.load(Ordering::Acquire), 1);
    assert_eq!(
        via_split.load(Ordering::Acquire),
        via_drop.load(Ordering::Acquire)
    );
}

/// The parked pointer is the identity `KArc::as_ptr` hands out, so it
/// interchanges with the placement primitives' node pointers.
#[test]
fn parked_pointer_matches_as_ptr() {
    let (arc, _drops) = counted();
    let expected = KArc::as_ptr(&arc);
    let node = KArc::release_deferrable_for_test(arc).expect("sole handle is final");
    assert_eq!(node.as_ptr().cast_const(), expected);
    // SAFETY: `node` is the result of exactly one `Some` release above.
    unsafe { KArc::destroy_deferred_for_test(node) };
}

/// Two threads racing the last two references: exactly one is told it won, and
/// the destructor runs exactly once. This is the property the graveyard's
/// single-pusher assumption rests on.
#[test]
fn exactly_one_racing_releaser_wins() {
    for _ in 0..ROUNDS {
        let (arc, drops) = counted();
        let wins = StdArc::new(AtomicUsize::new(0));
        let other = arc.clone();

        let handles: Vec<_> = [arc, other]
            .into_iter()
            .map(|handle| {
                let wins = wins.clone();
                std::thread::spawn(move || {
                    if let Some(node) = KArc::release_deferrable_for_test(handle) {
                        wins.fetch_add(1, Ordering::AcqRel);
                        // SAFETY: this thread won the sole one-to-zero
                        // transition, so it uniquely owns the allocation.
                        unsafe { KArc::destroy_deferred_for_test(node) };
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("releaser thread");
        }

        assert_eq!(wins.load(Ordering::Acquire), 1, "exactly one final release");
        assert_eq!(drops.load(Ordering::Acquire), 1, "destroyed exactly once");
    }
}

/// A weak upgrade racing the final release must never resurrect a value whose
/// strong count already hit zero, and a winning upgrade must keep the release
/// non-final. Either way the value is destroyed exactly once and never leaks.
#[test]
fn upgrade_never_races_past_the_final_release() {
    for _ in 0..ROUNDS {
        let (arc, drops) = counted();
        let weak = KArc::downgrade(&arc);

        // `NonNull` is `!Send`, so the winning releaser destroys in-thread and
        // reports only whether it won.
        let releaser = std::thread::spawn(move || match KArc::release_deferrable_for_test(arc) {
            Some(node) => {
                // SAFETY: this thread won the sole one-to-zero transition.
                unsafe { KArc::destroy_deferred_for_test(node) };
                true
            }
            None => false,
        });
        let upgraded = weak.upgrade();

        match (releaser.join().expect("releaser thread"), upgraded) {
            // The upgrade lost: the releaser owned and destroyed it.
            (true, None) => {}
            // The upgrade won: it now holds the only reference.
            (false, Some(rescued)) => {
                let node =
                    KArc::release_deferrable_for_test(rescued).expect("rescued handle is final");
                // SAFETY: sole `Some` release for this allocation.
                unsafe { KArc::destroy_deferred_for_test(node) };
            }
            (true, Some(rescued)) => {
                // Already destroyed by the releaser: dropping this would be a
                // second free and would mask the real failure.
                std::mem::forget(rescued);
                panic!("upgrade resurrected a value past its final release");
            }
            (false, None) => panic!("leaked: neither the releaser nor the upgrade owns it"),
        }
        assert_eq!(drops.load(Ordering::Acquire), 1, "destroyed exactly once");
    }
}

/// A weak handle keeps the allocation mapped after the strong side is gone, so
/// the identity address stays readable for comparison and never upgrades. This
/// is what lets the task registry answer "is this pointer one of mine?" without
/// upgrading — an upgraded handle dropped under the registry lock could be the
/// final reference and would run the destructor there.
#[test]
fn weak_as_ptr_is_a_stable_comparison_token() {
    let (arc, drops) = counted();
    let weak = KArc::downgrade(&arc);
    let before = weak.as_ptr();
    assert_eq!(before, KArc::as_ptr(&arc));

    let node = KArc::release_deferrable_for_test(arc).expect("sole handle is final");
    // SAFETY: `node` is the result of exactly one `Some` release above.
    unsafe { KArc::destroy_deferred_for_test(node) };
    assert_eq!(drops.load(Ordering::Acquire), 1);

    assert_eq!(
        weak.as_ptr(),
        before,
        "the identity address survives destruction while a weak handle holds \
         the allocation"
    );
    assert!(weak.upgrade().is_none(), "a dead weak never resurrects");
}

/// An empty weak handle has no referent and must report null rather than the
/// sentinel address it stores internally.
#[test]
fn empty_weak_as_ptr_is_null() {
    assert!(slopos_ostd::KWeak::<DropCounter>::new().as_ptr().is_null());
}

/// The task-typed wrappers are the surface `#![forbid(unsafe_code)]` crates
/// drive; check they delegate faithfully for a real `TaskInner`.
#[test]
fn task_wrappers_release_and_destroy_a_task() {
    let arc = KArc::try_new(TaskInner::<(), ()>::invalid()).expect("task allocation");
    let weak = KArc::downgrade(&arc);
    let second = arc.clone();

    assert!(task_release_strong(arc).is_none());
    assert!(
        weak.upgrade().is_some(),
        "task live after a non-final release"
    );

    let node = task_release_strong(second).expect("final release reports itself");
    assert!(
        weak.upgrade().is_none(),
        "the strong side is gone once the final release lands"
    );
    task_destroy_parked(node);
}
