#![feature(restricted_std)]

//! `prlimit64` reports the ceilings the kernel actually enforces.
//!
//! These assert the *relationship* between what is reported and what is
//! enforced, never a specific number — the numbers live in `abi/src/quota.rs`
//! and in the gate file, and a test that pinned them here would be a third
//! place to update and the first one to drift.

use slopos_userland as _;

use slopos_slibc::process::{RLIM_INFINITY, RLIMIT_ALL, RLimit, getrlimit, prlimit, setrlimit};

use slopos_abi::quota::{RLIMIT_AS, RLIMIT_NOFILE};

fn zeroed() -> RLimit {
    RLimit {
        rlim_cur: 0,
        rlim_max: 0,
    }
}

fn every_published_limit_is_finite() -> bool {
    for resource in RLIMIT_ALL {
        let mut lim = zeroed();
        if getrlimit(resource, &mut lim) != 0 {
            return false;
        }
        // Infinity tells a caller to stop worrying about a resource the kernel
        // is still counting and still refusing on.
        if lim.rlim_cur == RLIM_INFINITY || lim.rlim_max == RLIM_INFINITY {
            return false;
        }
        if lim.rlim_cur == 0 || lim.rlim_cur > lim.rlim_max {
            return false;
        }
    }
    true
}

fn unknown_resources_are_refused() -> bool {
    let mut lim = zeroed();
    // 4 and 42 are outside the set this kernel maps.
    getrlimit(4, &mut lim) != 0 && getrlimit(42, &mut lim) != 0
}

/// `RLIMIT_AS` is byte-denominated: the arena counts pages, and publishing a
/// page count under a byte-named limit would understate the bound by 4096.
fn byte_limits_are_reported_in_bytes() -> bool {
    let mut lim = zeroed();
    if getrlimit(RLIMIT_AS, &mut lim) != 0 {
        return false;
    }
    lim.rlim_cur >= 4096 * 1024
}

fn lowering_a_limit_takes_effect() -> bool {
    let mut original = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut original) != 0 {
        return false;
    }

    let lowered = RLimit {
        rlim_cur: original.rlim_cur / 2,
        rlim_max: original.rlim_max,
    };
    if setrlimit(RLIMIT_NOFILE, &lowered) != 0 {
        return false;
    }

    let mut read_back = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut read_back) != 0 {
        return false;
    }
    let ok = read_back.rlim_cur == lowered.rlim_cur;

    // A lowered descriptor ceiling would follow this process into whatever the
    // harness runs next.
    setrlimit(RLIMIT_NOFILE, &original);
    ok
}

/// Raising is the privileged operation; granting it unconditionally would make
/// every ceiling advisory.
fn raising_the_hard_limit_is_refused() -> bool {
    let mut original = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut original) != 0 {
        return false;
    }
    let raised = RLimit {
        rlim_cur: original.rlim_cur,
        rlim_max: original.rlim_max.saturating_mul(4),
    };
    setrlimit(RLIMIT_NOFILE, &raised) != 0
}

/// The arena counts in `u32` and the ABI in `u64`, so a request too large to
/// convert has to land somewhere; mapping it to the no-limit sentinel would turn
/// the widest possible `setrlimit` into an unprivileged way to switch
/// enforcement off. Unreachable while the hard-limit check bounds every
/// published resource below `u32::MAX`, and pinned for the day one is not.
fn an_oversized_request_cannot_lift_the_ceiling() -> bool {
    let mut original = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut original) != 0 {
        return false;
    }
    // Both halves large: `rlim_cur > rlim_max` is rejected before the
    // conversion this is about.
    let huge = RLimit {
        rlim_cur: u64::MAX - 1,
        rlim_max: u64::MAX - 1,
    };
    setrlimit(RLIMIT_NOFILE, &huge);

    let mut after = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut after) != 0 {
        return false;
    }
    let ok = after.rlim_cur <= original.rlim_cur && after.rlim_cur != RLIM_INFINITY;
    setrlimit(RLIMIT_NOFILE, &original);
    ok
}

fn soft_above_hard_is_rejected() -> bool {
    let mut original = zeroed();
    if getrlimit(RLIMIT_NOFILE, &mut original) != 0 {
        return false;
    }
    let inverted = RLimit {
        rlim_cur: original.rlim_max.saturating_add(1),
        rlim_max: original.rlim_max,
    };
    setrlimit(RLIMIT_NOFILE, &inverted) != 0
}

/// There is no privilege principal in this kernel, so permitting a foreign pid
/// would be permitting it unconditionally.
fn foreign_pids_are_refused() -> bool {
    let mut lim = zeroed();
    // A pid this process is very unlikely to own.
    prlimit(31337, RLIMIT_NOFILE, None, Some(&mut lim)) != 0
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "every_published_limit_is_finite",
            every_published_limit_is_finite,
        ),
        (
            "unknown_resources_are_refused",
            unknown_resources_are_refused,
        ),
        (
            "byte_limits_are_reported_in_bytes",
            byte_limits_are_reported_in_bytes,
        ),
        (
            "lowering_a_limit_takes_effect",
            lowering_a_limit_takes_effect,
        ),
        (
            "raising_the_hard_limit_is_refused",
            raising_the_hard_limit_is_refused,
        ),
        ("soft_above_hard_is_rejected", soft_above_hard_is_rejected),
        (
            "an_oversized_request_cannot_lift_the_ceiling",
            an_oversized_request_cannot_lift_the_ceiling,
        ),
        ("foreign_pids_are_refused", foreign_pids_are_refused),
    ]);
}
