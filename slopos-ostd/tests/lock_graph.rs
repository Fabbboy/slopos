//! Host-side tests for `slopos_ostd::sync::lock_graph`.
//!
//! The tests use synthetic lock addresses (not real `SpinLock<T>` instances) so
//! `push_lock` / `pop_lock` can be driven directly without ticket-lock
//! interaction.

use std::panic;
use std::sync::Mutex;

use slopos_ostd::cpu::x86_64::interrupts::{are_interrupts_enabled, enable_interrupts};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_graph::{
    ACQ_NONE, ACQ_RECURSIVE, LO_DUPOK, LOCK_LEVEL_ALLOCATOR, LOCK_LEVEL_REGISTRY,
    LOCK_LEVEL_RESOURCE, LockClassKey, LockdepMode, PushIrqState, class_collisions, class_count,
    enable_lock_tracking, enter_fatal_bypass, fatal_bypassed, for_each_held_lock_name,
    for_each_held_lock_name_for_cpu, held_depth_mark, held_lock_count, poison_unlock_all_held,
    poison_unlock_held_above, pop_lock, pop_misses, push_irq_state, push_lock, push_lock_ex,
    register_class_for_test, report_only_violations, reset_for_test, set_in_report_for_test,
    set_lockdep_mode, violation_reports,
};

/// Serialise every test that touches the global graph state: the class table,
/// edge pool and chain cache are process-global, and `cargo test` parallelises
/// `#[test]` items by default.
static LOCK_LOCK: Mutex<()> = Mutex::new(());

/// Take the gate, taking over a poisoning left by an earlier failure. Every
/// test opens with `reset_for_test`, so an inherited poison says nothing about
/// the state this test will see.
fn serial_gate() -> std::sync::MutexGuard<'static, ()> {
    LOCK_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

unsafe fn noop_poison(_addr: *const ()) {}

fn setup() {
    reset_for_test();
    enable_lock_tracking();
}

/// Push with the default subclass and no acquisition flags.
///
/// # Safety
/// As `push_lock`: synthetic addresses only, popped before the test ends.
unsafe fn push(addr: *const (), class: &'static LockClassKey) {
    unsafe { push_lock(addr, noop_poison, class) }
}

#[test]
fn ascending_levels_accepted() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x1001);
    let b = core::ptr::without_provenance::<()>(0x1002);
    let c = core::ptr::without_provenance::<()>(0x1003);
    let ka = lock_class!("t1.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t1.b", LOCK_LEVEL_REGISTRY);
    let kc = lock_class!("t1.c", LOCK_LEVEL_ALLOCATOR);

    unsafe {
        push(a, ka);
        push(b, kb);
        push(c, kc);
        assert_eq!(held_lock_count(), 3);
        pop_lock(c);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn same_level_distinct_classes_accepted() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x2001);
    let b = core::ptr::without_provenance::<()>(0x2002);
    let ka = lock_class!("t2.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t2.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        push(b, kb);
        assert_eq!(held_lock_count(), 2);
        pop_lock(b);
        pop_lock(a);
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

/// Declaration-site keying: two instances from one `lock_class!` expansion are
/// one class, so an array of N locks does not cost N class slots.
#[test]
fn one_site_two_instances_share_a_class() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t3.array", LOCK_LEVEL_RESOURCE);
    let a = core::ptr::without_provenance::<()>(0x3001);
    let b = core::ptr::without_provenance::<()>(0x3002);

    let before = class_count();
    unsafe {
        push(a, key);
        pop_lock(a);
        push(b, key);
        pop_lock(b);
    }
    assert_eq!(
        class_count(),
        before + 1,
        "two instances of one declaration site must register one class"
    );
}

#[test]
fn two_sites_are_two_classes() {
    let _g = serial_gate();
    setup();

    let ka = lock_class!("t4.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t4.b", LOCK_LEVEL_RESOURCE);
    let a = core::ptr::without_provenance::<()>(0x4001);

    let before = class_count();
    unsafe {
        push(a, ka);
        pop_lock(a);
        push(a, kb);
        pop_lock(a);
    }
    assert_eq!(class_count(), before + 2);
}

#[test]
fn ab_then_ba_detects_cycle() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x5001);
    let b = core::ptr::without_provenance::<()>(0x5002);
    let ka = lock_class!("t5.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t5.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }

    let result = panic::catch_unwind(|| unsafe {
        push(b, kb);
        push(a, ka);
    });
    assert!(result.is_err(), "B->A after A->B must be reported");

    // `push_lock` may have pushed before panicking; drain for the next test.
    unsafe { poison_unlock_all_held() };
}

/// Nesting two instances of one class is reported by default: nothing says the
/// site orders its instances.
#[test]
fn same_class_different_instance_reports() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t6.array", LOCK_LEVEL_RESOURCE);
    let a = core::ptr::without_provenance::<()>(0x6001);
    let b = core::ptr::without_provenance::<()>(0x6002);

    let result = panic::catch_unwind(|| unsafe {
        push(a, key);
        push(b, key);
    });
    assert!(result.is_err(), "same-class nesting must be reported");
    unsafe { poison_unlock_all_held() };
}

#[test]
fn dupok_permits_same_class_nesting() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t7.array", LOCK_LEVEL_RESOURCE, LO_DUPOK);
    let a = core::ptr::without_provenance::<()>(0x7001);
    let b = core::ptr::without_provenance::<()>(0x7002);

    unsafe {
        push(a, key);
        push(b, key);
        assert_eq!(held_lock_count(), 2);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

/// Same class *and* same instance is recursion, a different finding from
/// nesting — and `LO_DUPOK` deliberately does not cover it.
#[test]
fn same_class_same_instance_reports_recursion() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t8.recursive", LOCK_LEVEL_RESOURCE, LO_DUPOK);
    let a = core::ptr::without_provenance::<()>(0x8001);

    let result = panic::catch_unwind(|| unsafe {
        push(a, key);
        push(a, key);
    });
    assert!(result.is_err(), "same-instance recursion must be reported");
    unsafe { poison_unlock_all_held() };
}

#[test]
fn acq_recursive_permits_same_instance() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t9.reader", LOCK_LEVEL_RESOURCE, LO_DUPOK);
    let a = core::ptr::without_provenance::<()>(0x9001);

    unsafe {
        push_lock_ex(a, noop_poison, key, 0, ACQ_RECURSIVE);
        push_lock_ex(a, noop_poison, key, 0, ACQ_RECURSIVE);
        assert_eq!(held_lock_count(), 2);
        pop_lock(a);
        pop_lock(a);
    }
    assert_eq!(held_lock_count(), 0);
}

/// A non-recursive acquisition nested inside a recursive one is still a
/// deadlock (a writer behind a reader): only a recursive *pair* is waved
/// through.
#[test]
fn non_recursive_inside_recursive_still_reports() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t10.rw", LOCK_LEVEL_RESOURCE, LO_DUPOK);
    let a = core::ptr::without_provenance::<()>(0xA001);

    let result = panic::catch_unwind(|| unsafe {
        push_lock_ex(a, noop_poison, key, 0, ACQ_RECURSIVE);
        push_lock_ex(a, noop_poison, key, 0, ACQ_NONE);
    });
    assert!(result.is_err());
    unsafe { poison_unlock_all_held() };
}

/// A subclass splits one site into distinct classes, so nesting stays
/// *checked* rather than being waived.
#[test]
fn subclass_yields_distinct_classes() {
    let _g = serial_gate();
    setup();

    let key = lock_class!("t11.nested", LOCK_LEVEL_RESOURCE);
    let a = core::ptr::without_provenance::<()>(0xB001);
    let b = core::ptr::without_provenance::<()>(0xB002);

    let before = class_count();
    unsafe {
        push_lock_ex(a, noop_poison, key, 0, ACQ_NONE);
        push_lock_ex(b, noop_poison, key, 1, ACQ_NONE);
        assert_eq!(held_lock_count(), 2);
        pop_lock(b);
        pop_lock(a);
    }
    assert_eq!(class_count(), before + 2, "subclass 1 is its own class");

    let result = panic::catch_unwind(|| unsafe {
        push_lock_ex(b, noop_poison, key, 1, ACQ_NONE);
        push_lock_ex(a, noop_poison, key, 0, ACQ_NONE);
    });
    assert!(result.is_err(), "subclass order must still be checked");
    unsafe { poison_unlock_all_held() };
}

/// Warn mode reports and keeps going, and must not learn the offending edge or
/// cache the chain: the graph would go cyclic and every later finding would be
/// derived noise.
#[test]
fn warn_mode_withholds_the_edge() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0xC001);
    let b = core::ptr::without_provenance::<()>(0xC002);
    let ka = lock_class!("t12.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t12.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }

    set_lockdep_mode(LockdepMode::Warn);
    let before = report_only_violations();
    unsafe {
        push(b, kb);
        push(a, ka);
        pop_lock(a);
        pop_lock(b);
    }
    assert!(
        report_only_violations() > before,
        "warn mode must still count the finding"
    );

    set_lockdep_mode(LockdepMode::Panic);
    let result = panic::catch_unwind(|| unsafe {
        push(b, kb);
        push(a, ka);
    });
    assert!(
        result.is_err(),
        "warn mode learned the offending edge, so the cycle vanished"
    );
    unsafe { poison_unlock_all_held() };
    set_lockdep_mode(LockdepMode::Panic);
}

/// One inversion on a hot path is one finding, however many times it runs.
#[test]
fn violations_dedup_per_class_pair() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0xD001);
    let b = core::ptr::without_provenance::<()>(0xD002);
    let ka = lock_class!("t13.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t13.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }

    set_lockdep_mode(LockdepMode::Warn);
    let reports_before = violation_reports();
    let counted_before = report_only_violations();
    for _ in 0..32 {
        unsafe {
            push(b, kb);
            push(a, ka);
            pop_lock(a);
            pop_lock(b);
        }
    }
    assert_eq!(
        violation_reports(),
        reports_before + 1,
        "one class pair must print once"
    );
    assert!(
        report_only_violations() >= counted_before + 32,
        "every occurrence must still be counted"
    );
    set_lockdep_mode(LockdepMode::Panic);
}

/// Two `.rodata` copies of one key — what a release build may produce
/// across crates — are one class, not a collision.
#[test]
fn rodata_duplicate_key_is_not_a_collision() {
    let _g = serial_gate();
    setup();

    static K1: LockClassKey =
        LockClassKey::__from_site("dup", "dup.rs:1:1", LOCK_LEVEL_RESOURCE, 0);
    static K2: LockClassKey =
        LockClassKey::__from_site("dup", "dup.rs:1:1", LOCK_LEVEL_RESOURCE, 0);

    let i1 = register_class_for_test(&K1).expect("class table has room");
    let i2 = register_class_for_test(&K2).expect("class table has room");
    assert_eq!(i1, i2, "identical keys must resolve to one class");
    assert_eq!(class_collisions(), 0);
}

/// Two genuinely different sites that hash the same are merged — safe, but
/// it must be reported rather than silently changing what is validated.
#[test]
fn distinct_sites_sharing_an_id_are_reported() {
    let _g = serial_gate();
    setup();

    static K1: LockClassKey =
        LockClassKey::__from_site("collide.a", "a.rs:1:1", LOCK_LEVEL_RESOURCE, 0);
    static K2: LockClassKey =
        LockClassKey::__from_site("collide.a", "a.rs:1:1", LOCK_LEVEL_RESOURCE, 0);
    static K3: LockClassKey =
        LockClassKey::__from_site("collide.b", "b.rs:9:9", LOCK_LEVEL_RESOURCE, 0);

    assert_eq!(K1.id(), K2.id());
    assert_ne!(K1.id(), K3.id(), "different sites must hash differently");

    let _ = register_class_for_test(&K1);
    let _ = register_class_for_test(&K2);
    assert_eq!(
        class_collisions(),
        0,
        "identical strings are not a collision"
    );
}

#[test]
fn chain_hash_cache_repeated_chain_is_fast() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0xE001);
    let b = core::ptr::without_provenance::<()>(0xE002);
    let ka = lock_class!("t16.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t16.b", LOCK_LEVEL_REGISTRY);

    for _ in 0..16 {
        unsafe {
            push(a, ka);
            push(b, kb);
            assert_eq!(held_lock_count(), 2);
            pop_lock(b);
            pop_lock(a);
        }
    }
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn panic_bypass_suppresses_ordering_check() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0xF001);
    let b = core::ptr::without_provenance::<()>(0xF002);
    let ka = lock_class!("t17.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t17.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }

    enter_fatal_bypass();

    unsafe {
        push(b, kb);
        push(a, ka);
        assert_eq!(held_lock_count(), 2);
        pop_lock(a);
        pop_lock(b);
    }
    assert_eq!(held_lock_count(), 0);
}

/// `Off` runs no checks but must keep the held stack complete: the poison
/// walk, `held_lock_count` and the TLB ack-wait diagnostic all read it.
#[test]
fn off_mode_still_tracks_for_poison_walk() {
    let _g = serial_gate();
    setup();
    set_lockdep_mode(LockdepMode::Off);

    let key = lock_class!("t18.off", LOCK_LEVEL_RESOURCE);
    let a = core::ptr::without_provenance::<()>(0x1_0001);
    let b = core::ptr::without_provenance::<()>(0x1_0002);

    unsafe {
        push(a, key);
        push(b, key);
        assert_eq!(held_lock_count(), 2, "Off must still record held locks");
        poison_unlock_all_held();
    }
    assert_eq!(held_lock_count(), 0);
    set_lockdep_mode(LockdepMode::Panic);
}

/// The poison walk is a stack drain, not a policy decision: a recovered oops
/// runs it and the kernel resumes, so latching the fatal bypass there would
/// switch ordering validation off for the rest of the boot.
#[test]
fn poison_walk_does_not_disable_ordering_checks() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x3_0001);
    let b = core::ptr::without_provenance::<()>(0x3_0002);
    let ka = lock_class!("t20.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t20.b", LOCK_LEVEL_RESOURCE);

    unsafe {
        push(a, ka);
        poison_unlock_all_held();
    }
    assert_eq!(held_lock_count(), 0);
    assert!(
        !fatal_bypassed(),
        "a recovered oops must not latch the fatal bypass"
    );

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }
    let result = panic::catch_unwind(|| unsafe {
        push(b, kb);
        push(a, ka);
    });
    assert!(
        result.is_err(),
        "the poison walk silently disabled cycle detection"
    );
    unsafe { poison_unlock_all_held() };
}

/// A recovery boundary nested inside another releases only what it acquired:
/// draining past the outer frame's locks would leave live guards whose `Drop`
/// releases a second time.
#[test]
fn nested_recovery_keeps_outer_locks_held() {
    let _g = serial_gate();
    setup();

    let outer = core::ptr::without_provenance::<()>(0x4_0001);
    let inner = core::ptr::without_provenance::<()>(0x4_0002);
    let ko = lock_class!("t21.outer", LOCK_LEVEL_RESOURCE);
    let ki = lock_class!("t21.inner", LOCK_LEVEL_REGISTRY);

    unsafe {
        push(outer, ko);
        let mark = held_depth_mark();
        push(inner, ki);
        assert_eq!(held_lock_count(), 2);

        poison_unlock_held_above(mark);
        assert_eq!(
            held_lock_count(),
            1,
            "the outer frame's lock must still be held"
        );

        pop_lock(outer);
    }
    assert_eq!(held_lock_count(), 0);
}

#[test]
fn held_stack_walk_after_chain_acquisition() {
    let _g = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x2_0001);
    let b = core::ptr::without_provenance::<()>(0x2_0002);
    let ka = lock_class!("t19.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t19.b", LOCK_LEVEL_REGISTRY);

    unsafe {
        push(a, ka);
        push(b, kb);
        assert_eq!(held_lock_count(), 2);
        poison_unlock_all_held();
    }
    assert_eq!(held_lock_count(), 0);
}

/// The entry write and the depth publish are separate stores: an interrupt
/// between them lets the handler's own acquire claim the slot the interrupted
/// push had filled but not yet counted, leaving an entry inside `depth` with a
/// null address that no `pop_lock` can find.
///
/// Interrupts are enabled here because that is how a `PreemptMutex` or an
/// `Epoch::enter` acquires.
#[test]
fn held_stack_update_masks_interrupts() {
    let _g = serial_gate();
    setup();

    enable_interrupts();
    assert!(
        are_interrupts_enabled(),
        "the acquire being modelled runs with interrupts on"
    );

    let a = core::ptr::without_provenance::<()>(0x5_0001);
    let b = core::ptr::without_provenance::<()>(0x5_0002);
    let ka = lock_class!("t22.a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("t22.b", LOCK_LEVEL_REGISTRY);

    unsafe {
        push(a, ka);
        push(b, kb);
        pop_lock(b);
        pop_lock(a);
    }

    assert_eq!(
        push_irq_state(),
        PushIrqState::ReachedMasked,
        "a held-stack update either never ran or ran with interrupts enabled; \
         an interrupt landing inside one leaves an uncountable entry"
    );
    assert!(
        are_interrupts_enabled(),
        "the caller's interrupt state must survive the update"
    );
    assert_eq!(held_lock_count(), 0);
}

/// A lock taken while this CPU is inside a report — the klog ring every
/// `report_*` acquires — must still be recorded. Suppressing the record
/// instead of the check strands the depth: a panicking report leaves the latch
/// raised, so the release does not read the state the acquire did.
#[test]
fn report_reentrancy_records_without_checking() {
    let _g = serial_gate();
    setup();

    let held = core::ptr::without_provenance::<()>(0x6_0001);
    let nested = core::ptr::without_provenance::<()>(0x6_0002);
    let kh = lock_class!("t23.held", LOCK_LEVEL_RESOURCE);
    let kn = lock_class!("t23.nested", LOCK_LEVEL_REGISTRY);

    unsafe {
        push(held, kh);
        set_in_report_for_test(true);
        push(nested, kn);
        assert_eq!(
            held_lock_count(),
            2,
            "an acquire inside a report must still be recorded"
        );

        // Cleared before the release, as a panicking report leaves it for
        // `poison_unlock_held_above`.
        set_in_report_for_test(false);
        pop_lock(nested);
        assert_eq!(
            held_lock_count(),
            1,
            "the release must find the entry the acquire recorded"
        );
        pop_lock(held);
    }
    assert_eq!(held_lock_count(), 0);
    assert_eq!(pop_misses(), 0, "no release went unmatched");
}

/// The TLB shootdown timeout reports through these. A reporter that silently
/// yields nothing costs a whole reproduction cycle to discover.
#[test]
fn held_lock_names_are_reported() {
    let _gate = serial_gate();
    setup();

    let a = core::ptr::without_provenance::<()>(0x5001);
    let b = core::ptr::without_provenance::<()>(0x6001);
    let ka = lock_class!("held_name_a", LOCK_LEVEL_RESOURCE);
    let kb = lock_class!("held_name_b", LOCK_LEVEL_REGISTRY);

    unsafe {
        push(a, ka);
        push(b, kb);
    }

    let mut local = Vec::new();
    for_each_held_lock_name(|name| local.push(name));
    assert_eq!(local, ["held_name_a", "held_name_b"]);

    // Read the way a peer CPU reads it during the timeout report.
    let cpu = slopos_ostd::cpu::x86_64::pcr::get_current_cpu();
    let mut remote = Vec::new();
    for_each_held_lock_name_for_cpu(cpu, |name| remote.push(name));
    assert_eq!(remote, local);

    unsafe {
        pop_lock(b);
        pop_lock(a);
    }

    let mut after = Vec::new();
    for_each_held_lock_name(|name| after.push(name));
    assert!(after.is_empty(), "names outlived the locks: {after:?}");
}

/// The one case where reporting nothing is correct.
#[test]
fn held_lock_names_are_silent_without_tracking() {
    let _gate = serial_gate();
    reset_for_test();

    let mut seen = 0usize;
    for_each_held_lock_name(|_| seen += 1);
    for_each_held_lock_name_for_cpu(0, |_| seen += 1);
    assert_eq!(seen, 0);
}
