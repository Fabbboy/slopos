//! Coverage for the task existence reference — the one strong reference a task
//! holds to itself between registration and reap.
//!
//! It exists because containers do not cover every live state: a blocked kernel
//! thread sits in no queue and has no parent, a placement reservation has not
//! reached its queue, and a freshly created or forked task is registered before
//! it is published. In each of those the existence reference is the only thing
//! keeping the allocation alive, so the two properties tested here are what the
//! whole weak-only registry rests on:
//!
//! - the reference is handed out once and taken back once, even under repeated
//!   or racing attempts, so a reap is idempotent and cannot double-release;
//! - a task that never held one yields nothing, which is what makes the reap
//!   safe to attempt on any task.
//!
//! Note `just check-miri` runs with `-Zmiri-ignore-leaks`, so a *leaked*
//! reference is invisible to Miri here — every test asserts strong counts and
//! the parked tally explicitly. A double release is a double free, which Miri
//! does catch.

use core::ptr::NonNull;

use slopos_ostd::KArc;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{
    task_existence_is_parked, task_existence_park, task_existence_parked_count,
    task_existence_release, task_placement_strong_count,
};

type HostTask = TaskInner<(), ()>;

fn fresh_task() -> (KArc<HostTask>, NonNull<HostTask>) {
    let arc = KArc::try_new(HostTask::invalid()).expect("task allocation");
    let node = NonNull::new(KArc::as_ptr(&arc).cast_mut()).expect("KArc base is non-null");
    (arc, node)
}

/// The reference is minted once, reported as held, and taken back once. The
/// second attempt is the idempotent reap: it must not fabricate a reference the
/// task no longer owns.
#[test]
fn park_then_release_round_trips_exactly_once() {
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

/// Parking twice must not inflate the count. The flag is claimed with a
/// compare-exchange after the retain, so the loser undoes its own mint.
#[test]
fn parking_twice_mints_one_reference() {
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

/// A task that was never registered holds nothing, so a reap attempt on it is a
/// no-op rather than a fabricated reference. This is what lets the reap path run
/// without first proving the task was ever published.
#[test]
fn release_without_park_yields_nothing() {
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

/// The parked tally is the leak tripwire the kernel asserts against its registry
/// occupancy, so it has to return to its baseline across a park/release pair.
#[test]
fn parked_count_tracks_park_and_release() {
    let baseline = task_existence_parked_count();
    let (arc, node) = fresh_task();

    assert!(task_existence_park(node));
    assert_eq!(task_existence_parked_count(), baseline + 1);

    // A refused park must not be counted.
    assert!(!task_existence_park(node));
    assert_eq!(task_existence_parked_count(), baseline + 1);

    let taken = task_existence_release(node).expect("release wins");
    assert_eq!(task_existence_parked_count(), baseline);

    // Nor must a refused release.
    assert!(task_existence_release(node).is_none());
    assert_eq!(task_existence_parked_count(), baseline);

    drop(taken);
    drop(arc);
}

/// The reference round-trips through the task's own base address, which is the
/// same pointer the placement links are keyed on.
#[test]
fn release_returns_a_handle_to_the_same_allocation() {
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

/// A forked child must not inherit its parent's parked flag.
///
/// `clone_from_raw` copies the parent bytewise, flag included. If the child kept
/// that `true` it would be reaped by taking back a reference it was never given,
/// dropping the count one below what its real owners hold — a use-after-free
/// with no bad pointer anywhere in sight. This is the single most dangerous way
/// the existence reference can be got wrong.
#[test]
fn a_cloned_task_does_not_inherit_the_parked_flag() {
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
