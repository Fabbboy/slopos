//! Coverage for [`RcuArcSlot`] — the RCU-published `KArc` slot.
//!
//! The slot owns exactly one strong reference and hands each reader an
//! independent one. Every property worth testing is a *refcount* property, so
//! every test asserts strong counts explicitly rather than trusting a clean
//! run: `just check-miri` runs with `-Zmiri-ignore-leaks`, which would hide a
//! reference the slot forgot to release. A double release, by contrast, is a
//! double free and Miri does catch it.
//!
//! `load` and `store` are **not** reachable here: both open an RCU read-side
//! section, and `rcu_read_lock` takes a `PreemptGuard`, whose increment is a
//! gs-relative RMW that faults without a PCR. They are covered in-kernel by
//! `slopos_core::tests::ostd_arc_tests` instead. What a host test can pin down
//! is the ownership algebra around them — that an exclusive replace returns
//! exactly the displaced reference and mints none, and that dropping the slot
//! releases what it held rather than leaking it.

use slopos_ostd::mm::KArc;
use slopos_ostd::sync::RcuArcSlot;

/// A payload with the bounds the slot requires. `KArc<T>` is `Send + Sync`
/// only when `T` is, and the slot's whole point is cross-CPU publication.
#[derive(Debug, PartialEq, Eq)]
struct Payload(u32);

fn arc(v: u32) -> KArc<Payload> {
    KArc::try_new(Payload(v)).expect("payload allocation")
}

#[test]
fn empty_slot_holds_nothing() {
    let slot: RcuArcSlot<Payload> = RcuArcSlot::empty();
    assert!(slot.is_empty_racy());
}

#[test]
fn exclusive_replace_moves_the_reference_without_minting_one() {
    let mut slot = RcuArcSlot::empty();
    let first = arc(1);
    assert_eq!(KArc::strong_count(&first), 1);

    // Publishing moves the caller's reference into the slot: the count is
    // unchanged, because a reference changed owner rather than being cloned.
    assert!(slot.replace_exclusive(Some(first)).is_none());
    assert!(!slot.is_empty_racy());

    let second = arc(2);
    let displaced = slot
        .replace_exclusive(Some(second))
        .expect("the first reference comes back out");
    assert_eq!(*displaced, Payload(1));
    // Exactly the reference we put in, and nothing else holds it.
    assert_eq!(KArc::strong_count(&displaced), 1);
    drop(displaced);

    let last = slot.replace_exclusive(None).expect("second comes back out");
    assert_eq!(*last, Payload(2));
    assert_eq!(KArc::strong_count(&last), 1);
    assert!(slot.is_empty_racy());
}

/// A reader's handle outliving the slot's contents is the whole reason this
/// type exists rather than a borrow-lending [`slopos_ostd::sync::RcuCell`].
/// The in-kernel test mints the reader handle with `load`; here the same
/// property is checked against a handle taken before publication, which is the
/// half that needs no RCU section.
#[test]
fn a_reference_taken_before_publication_outlives_the_slot() {
    let mut slot = RcuArcSlot::empty();
    let published = arc(7);
    let reader = published.clone();
    let observer = KArc::downgrade(&published);
    slot.replace_exclusive(Some(published));
    assert_eq!(KArc::strong_count(&reader), 2, "slot's reference plus ours");

    drop(slot.replace_exclusive(None));
    assert_eq!(KArc::strong_count(&reader), 1);
    assert_eq!(
        *reader,
        Payload(7),
        "still readable after the slot moved on"
    );

    drop(reader);
    assert!(
        observer.upgrade().is_none(),
        "every reference was released exactly once"
    );
}

#[test]
fn dropping_the_slot_releases_what_it_held() {
    let published = arc(9);
    let observer = KArc::downgrade(&published);
    {
        let mut slot = RcuArcSlot::empty();
        slot.replace_exclusive(Some(published));
        assert!(
            observer.upgrade().is_some(),
            "the slot keeps the payload alive"
        );
    }
    // Without `Drop` on the slot, this reference would leak — invisibly, since
    // KernMiri runs with `-Zmiri-ignore-leaks`. Hence the explicit assertion.
    assert!(
        observer.upgrade().is_none(),
        "dropping the slot released its reference"
    );
}

#[test]
fn default_is_empty() {
    let slot: RcuArcSlot<Payload> = RcuArcSlot::default();
    assert!(slot.is_empty_racy());
}
