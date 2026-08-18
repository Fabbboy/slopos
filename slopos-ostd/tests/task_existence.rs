//! Coverage for the task existence reference — the one strong reference a task
//! holds to itself between registration and reap.
//!
//! In the states no container covers — a blocked kernel thread, an unqueued
//! placement reservation, a task registered before it is published — it is the
//! only thing keeping the allocation alive.
//!
//! `just check-miri` runs with `-Zmiri-ignore-leaks`, so a *leaked* reference is
//! invisible to Miri here — every test asserts strong counts and the parked
//! tally explicitly. A double release is a double free, which Miri does catch.

use core::ptr::NonNull;
use std::cell::Cell;
use std::sync::{Mutex, MutexGuard};

use slopos_ostd::KArc;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{
    task_existence_is_parked, task_existence_park, task_existence_parked_count,
    task_existence_release, task_placement_strong_count,
};

type HostTask = TaskInner<(), ()>;

/// Serialises every test in this binary, so the parked tally moves only by the
/// operations of the thread reading it.
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    /// Whether this thread holds [`SERIAL`] — the tripwire [`fresh_task`] reads.
    static HOLDS_SERIAL: Cell<bool> = const { Cell::new(false) };
}

struct SerialGate {
    _guard: MutexGuard<'static, ()>,
}

impl Drop for SerialGate {
    fn drop(&mut self) {
        HOLDS_SERIAL.set(false);
    }
}

/// Poison is taken over rather than propagated: propagating it would replace
/// every later test's real result with a panic naming the wrong test, and a
/// poisoning test either leaves the tally balanced or has already reported the
/// imbalance itself.
fn serial() -> SerialGate {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    HOLDS_SERIAL.set(true);
    SerialGate { _guard: guard }
}

fn fresh_task() -> (KArc<HostTask>, NonNull<HostTask>) {
    assert!(
        HOLDS_SERIAL.get(),
        "open the test with `let _gate = serial();`: parking a task off the gate \
         moves the process-global tally under whichever test is reading it"
    );
    let arc = KArc::try_new(HostTask::invalid()).expect("task allocation");
    let node = NonNull::new(KArc::as_ptr(&arc).cast_mut()).expect("KArc base is non-null");
    (arc, node)
}

#[test]
fn park_then_release_round_trips_exactly_once() {
    let _gate = serial();
    let (arc, node) = fresh_task();
    let before = task_placement_strong_count(node);

    assert!(task_existence_park(node), "a fresh task parks");
    assert!(task_existence_is_parked(node));
    assert_eq!(
        task_placement_strong_count(node),
        before + 1,
        "parking mints exactly one reference"
    );

    let taken = task_existence_release(node).expect("the first release wins");
    assert!(!task_existence_is_parked(node));
    assert!(
        task_existence_release(node).is_none(),
        "a second release must not hand out a reference twice"
    );

    drop(taken);
    assert_eq!(
        task_placement_strong_count(node),
        before,
        "release gives the reference back"
    );
    drop(arc);
}

#[test]
fn parking_twice_mints_one_reference() {
    let _gate = serial();
    let (arc, node) = fresh_task();
    let before = task_placement_strong_count(node);

    assert!(task_existence_park(node));
    assert!(
        !task_existence_park(node),
        "the second park reports that it did not take the flag"
    );
    assert_eq!(
        task_placement_strong_count(node),
        before + 1,
        "the losing park undid its retain"
    );

    let taken = task_existence_release(node).expect("release wins");
    drop(taken);
    assert_eq!(task_placement_strong_count(node), before);
    drop(arc);
}

/// What lets the reap path run without first proving the task was published.
#[test]
fn release_without_park_yields_nothing() {
    let _gate = serial();
    let (arc, node) = fresh_task();
    let before = task_placement_strong_count(node);

    assert!(!task_existence_is_parked(node));
    assert!(task_existence_release(node).is_none());
    assert_eq!(
        task_placement_strong_count(node),
        before,
        "a refused release touches no count"
    );
    drop(arc);
}

/// The parked tally is the leak tripwire the kernel asserts against its
/// registry occupancy.
#[test]
fn parked_count_tracks_park_and_release() {
    let _gate = serial();
    let baseline = task_existence_parked_count();
    let (arc, node) = fresh_task();

    assert!(task_existence_park(node));
    assert_eq!(
        task_existence_parked_count(),
        baseline + 1,
        "a park that mints a reference counts once"
    );

    assert!(!task_existence_park(node));
    assert_eq!(
        task_existence_parked_count(),
        baseline + 1,
        "a park that minted nothing must not be counted"
    );

    let taken = task_existence_release(node).expect("release wins");
    assert_eq!(
        task_existence_parked_count(),
        baseline,
        "the release that took the reference back returns the tally"
    );

    assert!(task_existence_release(node).is_none());
    assert_eq!(
        task_existence_parked_count(),
        baseline,
        "a release that took nothing back must not move the tally"
    );

    drop(taken);
    drop(arc);
}

/// The base address it round-trips through is the pointer the placement links
/// are keyed on.
#[test]
fn release_returns_a_handle_to_the_same_allocation() {
    let _gate = serial();
    let (arc, node) = fresh_task();
    assert!(task_existence_park(node));

    let taken = task_existence_release(node).expect("release wins");
    assert!(
        KArc::ptr_eq(&taken, &arc),
        "the reclaimed handle names the task it was parked on"
    );
    drop(taken);
    drop(arc);
}

/// `clone_from_raw` copies the parent bytewise, flag included. A child that
/// kept that `true` would be reaped by taking back a reference it was never
/// given, dropping the count below what its real owners hold.
#[test]
fn a_cloned_task_does_not_inherit_the_parked_flag() {
    let _gate = serial();
    let (parent, parent_node) = fresh_task();
    assert!(task_existence_park(parent_node));
    assert!(task_existence_is_parked(parent_node));

    let mut child = KArc::try_new(HostTask::invalid()).expect("child allocation");
    {
        let slot = KArc::get_mut(&mut child).expect("fresh child is unique");
        // SAFETY: `slot` and `parent` are distinct live allocations, and `slot`
        // is uniquely owned here, which is exactly `clone_from_raw`'s contract.
        unsafe { slot.clone_from_raw(&parent) };
    }
    // Derived only after the exclusive borrow above has ended: `get_mut`'s retag
    // invalidates any pointer taken from the allocation beforehand.
    let child_node = NonNull::new(KArc::as_ptr(&child).cast_mut()).expect("non-null");

    assert!(
        !task_existence_is_parked(child_node),
        "the child inherited its parent's existence reference"
    );
    assert!(
        task_existence_release(child_node).is_none(),
        "the child must not be able to release a reference it never held"
    );

    let taken = task_existence_release(parent_node).expect("the parent still holds its own");
    drop(taken);
    drop(child);
    drop(parent);
}
