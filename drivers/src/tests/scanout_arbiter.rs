//! Singleton-resource arbiter regression tests.
//!
//! Exercises the generic [`SingletonResource`] claim/commit protocol with a
//! dummy provider type (no hardware): priority decides the winner, equal
//! priority loses the tie, a live reservation blocks later claims before any
//! commit, `abort_claim` restores the prior owner, and committing a higher
//! provider evicts the displaced one exactly once.
//!
//! Each test owns a distinct function-local `static` arbiter (the methods take
//! `&'static self`); the harness runs each test once per boot, so the statics
//! start in their default (unowned) state.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::syscall_services::scanout::{ClaimOutcome, SingletonResource};
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

/// A higher-priority claimant wins and takes ownership over a lower-priority
/// committed owner.
pub fn test_higher_priority_wins() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new("test-prio");

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

/// A lower-priority claim loses; an equal-priority claim loses the tie. Neither
/// disturbs the committed owner.
pub fn test_lower_and_equal_priority_lose() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new("test-tie");

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

/// A live reservation (a winner that has not yet committed) blocks later claims,
/// proving reserve-before-reset: a loser learns to stay passive before any
/// hardware is touched.
pub fn test_reservation_blocks_before_commit() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new("test-reserve");

    if A.claim(30) != ClaimOutcome::Won {
        return fail!("first claim should win the empty arbiter");
    }
    // No commit yet — the reservation alone must repel competitors.
    if A.claim(30) != ClaimOutcome::LostTie {
        return fail!("equal claim must lose to a live reservation");
    }
    if A.claim(10) != ClaimOutcome::Lost {
        return fail!("lower claim must lose to a live reservation");
    }
    pass!()
}

/// `abort_claim` (a winner whose bring-up failed) clears the reservation and
/// leaves the prior owner intact, and a fresh claim can win afterwards.
pub fn test_abort_claim_restores_prior_owner() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new("test-abort");

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
    // The reservation must be cleared, so a fresh claim can win again.
    if A.claim(30) != ClaimOutcome::Won {
        return fail!("reservation must be cleared after abort");
    }
    pass!()
}

/// Committing a higher-priority provider evicts the displaced one exactly once;
/// the first commit (no prior owner) evicts nothing.
pub fn test_eviction_calls_displaced_evict_once() -> TestResult {
    static A: SingletonResource<DummyProvider> = SingletonResource::new("test-evict");

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
