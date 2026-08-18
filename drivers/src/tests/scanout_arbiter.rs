//! Singleton-resource arbiter regression tests.
//!
//! Each test owns a distinct function-local `static` arbiter (the methods take
//! `&'static self`); the harness runs each test once per boot, so the statics
//! start in their default (unowned) state.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::syscall_services::scanout::{ClaimOutcome, SingletonResource};
use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_testing::{TestResult, fail, pass};

#[derive(Clone, Copy)]
struct DummyProvider {
    id: u32,
    evict: fn(),
}

static EVICT_COUNT: AtomicU32 = AtomicU32::new(0);

fn counting_evict() {
    EVICT_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn noop_evict() {}

pub fn test_higher_priority_wins() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new(
        "test-prio",
        lock_class!("test.arbiter_prio.state", LOCK_LEVEL_RESOURCE),
    );

    if A.claim(0) != ClaimOutcome::Won {
        return fail!("base claim should win the empty arbiter");
    }
    A.commit_install(
        DummyProvider {
            id: 1,
            evict: noop_evict,
        },
        0,
        |_| {},
    );

    if A.claim(30) != ClaimOutcome::Won {
        return fail!("higher-priority claim should win");
    }
    A.commit_install(
        DummyProvider {
            id: 2,
            evict: noop_evict,
        },
        30,
        |_| {},
    );

    match A.current() {
        Some(p) if p.id == 2 => pass!(),
        _ => fail!("owner should be the higher-priority provider"),
    }
}

pub fn test_lower_and_equal_priority_lose() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new(
        "test-tie",
        lock_class!("test.arbiter_tie.state", LOCK_LEVEL_RESOURCE),
    );

    A.claim(50);
    A.commit_install(
        DummyProvider {
            id: 1,
            evict: noop_evict,
        },
        50,
        |_| {},
    );

    if A.claim(30) != ClaimOutcome::Lost {
        return fail!("lower-priority claim must lose");
    }
    if A.claim(50) != ClaimOutcome::LostTie {
        return fail!("equal-priority claim must lose the tie");
    }

    match A.current() {
        Some(p) if p.id == 1 => pass!(),
        _ => fail!("owner must be unchanged after losing claims"),
    }
}

/// Reserve-before-reset: a loser learns to stay passive before any hardware is
/// touched.
pub fn test_reservation_blocks_before_commit() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new(
        "test-reserve",
        lock_class!("test.arbiter_reserve.state", LOCK_LEVEL_RESOURCE),
    );

    if A.claim(30) != ClaimOutcome::Won {
        return fail!("first claim should win the empty arbiter");
    }
    if A.claim(30) != ClaimOutcome::LostTie {
        return fail!("equal claim must lose to a live reservation");
    }
    if A.claim(10) != ClaimOutcome::Lost {
        return fail!("lower claim must lose to a live reservation");
    }
    pass!()
}

pub fn test_abort_claim_restores_prior_owner() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new(
        "test-abort",
        lock_class!("test.arbiter_abort.state", LOCK_LEVEL_RESOURCE),
    );

    A.claim(0);
    A.commit_install(
        DummyProvider {
            id: 1,
            evict: noop_evict,
        },
        0,
        |_| {},
    );

    if A.claim(30) != ClaimOutcome::Won {
        return fail!("winner should reserve");
    }
    A.abort_claim();

    if A.current().map(|p| p.id) != Some(1) {
        return fail!("prior owner must survive an aborted claim");
    }
    if A.claim(30) != ClaimOutcome::Won {
        return fail!("reservation must be cleared after abort");
    }
    pass!()
}

pub fn test_eviction_calls_displaced_evict_once() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new(
        "test-evict",
        lock_class!("test.arbiter_evict.state", LOCK_LEVEL_RESOURCE),
    );

    EVICT_COUNT.store(0, Ordering::Relaxed);

    A.claim(0);
    A.commit_install(
        DummyProvider {
            id: 1,
            evict: counting_evict,
        },
        0,
        |displaced| {
            if let Some(p) = displaced {
                (p.evict)();
            }
        },
    );
    if EVICT_COUNT.load(Ordering::Relaxed) != 0 {
        return fail!("first commit must not evict (no prior owner)");
    }

    A.claim(30);
    A.commit_install(
        DummyProvider {
            id: 2,
            evict: noop_evict,
        },
        30,
        |displaced| {
            if let Some(p) = displaced {
                (p.evict)();
            }
        },
    );

    match EVICT_COUNT.load(Ordering::Relaxed) {
        1 => pass!(),
        n => fail!("displaced provider's evict ran {} times, expected 1", n),
    }
}

slopos_testing::stest!(name = test_higher_priority_wins, suite = scanout_arbiter);
slopos_testing::stest!(
    name = test_lower_and_equal_priority_lose,
    suite = scanout_arbiter
);
slopos_testing::stest!(
    name = test_reservation_blocks_before_commit,
    suite = scanout_arbiter
);
slopos_testing::stest!(
    name = test_abort_claim_restores_prior_owner,
    suite = scanout_arbiter
);
slopos_testing::stest!(
    name = test_eviction_calls_displaced_evict_once,
    suite = scanout_arbiter
);
