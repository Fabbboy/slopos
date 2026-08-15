#![feature(restricted_std)]

//! `prlimit64` reports the ceilings the kernel actually enforces.
//!
//! The failure this exists to prevent is the one Redox and Asterinas ship: a
//! limit reported to userland that nothing consults. A caller that cannot
//! query a real bound cannot back off gracefully, and one told a resource is
//! unlimited when it is not finds out by failing an allocation it had every
//! reason to believe would succeed.
//!
//! So these assert the *relationship* between what is reported and what is
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

/// Every published resource answers with a finite, enforced bound.
fn every_published_limit_is_finite() -> bool {
    for resource in RLIMIT_ALL {
        let mut lim = zeroed();
        if getrlimit(resource, &mut lim) != 0 {
            return false;
        }
        // A published limit that reads as infinity is the failure mode: it
        // tells a caller to stop worrying about a resource the kernel is
        // still counting and still refusing on.
        if lim.rlim_cur == RLIM_INFINITY || lim.rlim_max == RLIM_INFINITY {
            return false;
        }
        if lim.rlim_cur == 0 || lim.rlim_cur > lim.rlim_max {
            return false;
        }
    }
    true
}

/// An unknown resource is refused rather than answered with a fiction.
fn unknown_resources_are_refused() -> bool {
    let mut lim = zeroed();
    // 4 and 42 are outside the set this kernel maps.
    getrlimit(4, &mut lim) != 0 && getrlimit(42, &mut lim) != 0
}

/// `RLIMIT_AS` is byte-denominated, so it must be far larger than a page.
///
/// Catches the scale bug directly: the arena counts pages, and publishing a
/// page count under a byte-named limit would understate the bound by 4096 and
/// make a caller sizing an allocation against it back off far too early.
fn byte_limits_are_reported_in_bytes() -> bool {
    let mut lim = zeroed();
    if getrlimit(RLIMIT_AS, &mut lim) != 0 {
        return false;
    }
    lim.rlim_cur >= 4096 * 1024
}

/// Lowering the soft limit succeeds and is visible on read-back.
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

    // Put it back: a lowered descriptor ceiling would follow this process into
    // whatever the harness runs next.
    setrlimit(RLIMIT_NOFILE, &original);
    ok
}

/// Raising the hard limit is refused.
///
/// Raising is the privileged operation, and granting it unconditionally would
/// make every ceiling advisory — a process refused for want of headroom could
/// simply ask for more.
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

/// A soft limit above the hard limit is `EINVAL`.
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

/// Another process's limits are not ours to read or write.
///
/// There is no privilege principal in this kernel, so permitting it would be
/// permitting it unconditionally.
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
        ("foreign_pids_are_refused", foreign_pids_are_refused),
    ]);
}
