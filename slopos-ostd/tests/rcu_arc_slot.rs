//! Coverage for [`RcuArcSlot`] — the RCU-published `KArc` slot.
//!
//! Every test asserts strong counts explicitly: `just check-miri` runs with
//! `-Zmiri-ignore-leaks`, so a reference the slot forgot to release is
//! otherwise invisible.
//!
//! `load` and `store` are unreachable here — both open an RCU read-side
//! section, and `rcu_read_lock` takes a `PreemptGuard` whose gs-relative RMW
//! faults without a PCR. They are covered in-kernel by
//! `slopos_core::tests::ostd_arc_tests`.

use slopos_ostd::mm::KArc;
use slopos_ostd::sync::RcuArcSlot;

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

    assert!(slot.replace_exclusive(Some(first)).is_none());
    assert!(!slot.is_empty_racy());

    let second = arc(2);
    let displaced = slot
        .replace_exclusive(Some(second))
        .expect("the first reference comes back out");
    assert_eq!(*displaced, Payload(1));
    assert_eq!(KArc::strong_count(&displaced), 1);
    drop(displaced);

    let last = slot.replace_exclusive(None).expect("second comes back out");
    assert_eq!(*last, Payload(2));
    assert_eq!(KArc::strong_count(&last), 1);
    assert!(slot.is_empty_racy());
}

/// A reader's handle outliving the slot's contents is why this type exists
/// rather than a borrow-lending [`slopos_ostd::sync::RcuCell`].
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
