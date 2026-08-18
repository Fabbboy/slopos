//! Kernel-side tests for the OSTD `KArc` / `KWeak` reference-counting
//! primitives.

use slopos_ostd::sync::RcuArcSlot;
use slopos_ostd::{KArc, KWeak};
use slopos_testing::TestResult;
use slopos_testing::assert_test;

pub fn test_kweak_upgrade_after_last_strong_drop_is_none() -> TestResult {
    let strong = match KArc::try_new(0xA5A5_u32) {
        Ok(a) => a,
        Err(_) => return TestResult::Fail,
    };
    let weak = KArc::downgrade(&strong);

    assert_test!(
        weak.upgrade().is_some(),
        "upgrade should succeed while strong-alive"
    );

    drop(strong);
    assert_test!(
        weak.upgrade().is_none(),
        "upgrade must be None after last strong drop"
    );
    assert_test!(
        weak.strong_count() == 0,
        "strong_count must be 0 after drop"
    );
    TestResult::Pass
}

pub fn test_kweak_downgrade_upgrade_round_trip() -> TestResult {
    let strong = match KArc::try_new(1234_u64) {
        Ok(a) => a,
        Err(_) => return TestResult::Fail,
    };
    let weak = KArc::downgrade(&strong);

    let Some(upgraded) = weak.upgrade() else {
        return TestResult::Fail;
    };
    assert_test!(*upgraded == 1234, "round-tripped value mismatch");
    assert_test!(
        KArc::strong_count(&strong) == 2,
        "strong_count should be 2 with one upgrade outstanding"
    );
    drop(upgraded);
    assert_test!(
        KArc::strong_count(&strong) == 1,
        "strong_count should return to 1 after the upgrade drops"
    );
    TestResult::Pass
}

/// A node holding a `KWeak` back at itself, established by `try_new_cyclic`.
struct CyclicNode {
    payload: u32,
    self_link: KWeak<CyclicNode>,
}

pub fn test_karc_try_new_cyclic_wires_weak_self_link() -> TestResult {
    let node = match KArc::try_new_cyclic(|weak| CyclicNode {
        payload: 0xBEEF,
        self_link: weak.clone(),
    }) {
        Ok(node) => node,
        Err(_) => return TestResult::Fail,
    };

    let Some(via_weak) = node.self_link.upgrade() else {
        return TestResult::Fail;
    };
    assert_test!(via_weak.payload == 0xBEEF, "cyclic payload mismatch");
    assert_test!(
        KArc::strong_count(&node) == 2,
        "upgrading the self-link yields a second strong ref"
    );
    assert_test!(
        KArc::weak_count(&node) >= 1,
        "weak_count must reflect the stored self-link"
    );
    drop(via_weak);

    let observer = KArc::downgrade(&node);
    drop(node);
    assert_test!(
        observer.upgrade().is_none(),
        "cyclic node must not leak: weak self-link does not keep it alive"
    );
    TestResult::Pass
}

pub fn test_karc_weak_count_accuracy() -> TestResult {
    let strong = match KArc::try_new(7_u8) {
        Ok(a) => a,
        Err(_) => return TestResult::Fail,
    };
    assert_test!(
        KArc::weak_count(&strong) == 0,
        "fresh KArc should have weak_count 0"
    );

    let w1 = KArc::downgrade(&strong);
    assert_test!(
        KArc::weak_count(&strong) == 1,
        "one downgrade -> weak_count 1"
    );
    let w2 = w1.clone();
    assert_test!(
        KArc::weak_count(&strong) == 2,
        "cloning a KWeak bumps weak_count to 2"
    );
    drop(w2);
    assert_test!(
        KArc::weak_count(&strong) == 1,
        "dropping a weak clone -> weak_count 1"
    );
    drop(w1);
    assert_test!(
        KArc::weak_count(&strong) == 0,
        "dropping the last weak -> weak_count 0"
    );
    TestResult::Pass
}

pub fn test_karc_strong_count_saturates() -> TestResult {
    let strong = match KArc::try_new(0x5A_u8) {
        Ok(arc) => arc,
        Err(_) => return TestResult::Fail,
    };
    KArc::prepare_strong_saturation_for_test(&strong);
    let clone = strong.clone();
    assert_test!(
        KArc::strong_count(&strong) == KArc::<u8>::max_refcount_for_test(),
        "clone must saturate the strong count"
    );
    drop(clone);
    assert_test!(
        KArc::strong_count(&strong) == KArc::<u8>::max_refcount_for_test(),
        "drop must not undo strong-count saturation"
    );
    // A saturated allocation is immortal by design; releasing the last handle
    // confirms Drop takes the saturation path.
    drop(strong);
    TestResult::Pass
}

/// `RcuArcSlot::load` mints exactly one reference per call, and the handles it
/// mints stay valid after the slot has been published over.
///
/// Kernel-side because both halves open an RCU read-side section, and
/// `rcu_read_lock`'s gs-relative increment faults without a PCR, so the host
/// suite can only reach the exclusive paths.
pub fn test_rcu_arc_slot_load_mints_one_reference() -> TestResult {
    let slot: RcuArcSlot<u64> = RcuArcSlot::empty();
    assert_test!(slot.load().is_none(), "an empty slot loads nothing");

    let Ok(published) = KArc::try_new(0xDEAD_BEEF_u64) else {
        return TestResult::Fail;
    };
    let observer = KArc::downgrade(&published);
    slot.store(Some(published));

    let Some(first) = slot.load() else {
        return TestResult::Fail;
    };
    assert_test!(*first == 0xDEAD_BEEF, "loaded value mismatch");
    assert_test!(
        KArc::strong_count(&first) == 2,
        "load mints one reference on top of the slot's own"
    );

    let Some(second) = slot.load() else {
        return TestResult::Fail;
    };
    assert_test!(
        KArc::strong_count(&second) == 3,
        "a second load mints a second reference"
    );
    assert_test!(
        KArc::ptr_eq(&first, &second),
        "both loads name the same allocation"
    );

    // Publish over the slot: the displaced reference is released only after a
    // grace period, and the already-minted handles are independent of it.
    let Ok(replacement) = KArc::try_new(0x1234_u64) else {
        return TestResult::Fail;
    };
    slot.store(Some(replacement));
    assert_test!(
        *first == 0xDEAD_BEEF,
        "an outstanding reader handle survives republication"
    );

    drop(first);
    drop(second);
    slot.store(None);
    // `call_rcu` only queues, and an idle CPU may drain concurrently, so poll
    // for the effect rather than assuming one manual drain observes it.
    assert_test!(
        crate::tests::rcu_cb_tests::drain_until(|| observer.upgrade().is_none()),
        "every reference the slot handed out was released exactly once"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_kweak_upgrade_after_last_strong_drop_is_none,
    suite = ostd_arc
);
slopos_testing::stest!(
    name = test_rcu_arc_slot_load_mints_one_reference,
    suite = ostd_arc
);
slopos_testing::stest!(
    name = test_kweak_downgrade_upgrade_round_trip,
    suite = ostd_arc
);
slopos_testing::stest!(
    name = test_karc_try_new_cyclic_wires_weak_self_link,
    suite = ostd_arc
);
slopos_testing::stest!(name = test_karc_weak_count_accuracy, suite = ostd_arc);
slopos_testing::stest!(name = test_karc_strong_count_saturates, suite = ostd_arc);
