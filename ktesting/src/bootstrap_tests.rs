//! Self-tests for the test framework itself. The registry-sort
//! comparator clusters every `bootstrap_*` entry at the front of the
//! walk so a failure aborts the run before subsystem tests waste time
//! on broken plumbing.
//!
//! Order matters within this module — names use `aaa`/`bbb`/`ccc`/`ddd`
//! suffixes so the lex sort places them in a deterministic sequence
//! (the panic-isolation check needs to run *after* the canary).

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_utils::klog_info;

use crate::filter::glob_match;
use crate::TestResult;

/// Incremented at the entry of every bootstrap test (including the
/// panicking canary, *before* its panic). The isolation check reads it
/// to confirm the harness ran prior tests in order.
static BOOTSTRAP_CTR: AtomicU32 = AtomicU32::new(0);

fn bootstrap_aaa_glob_match() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    if !glob_match(b"a::*", b"a::b") {
        return TestResult::Fail;
    }
    if glob_match(b"a::*", b"b::a") {
        return TestResult::Fail;
    }
    if !glob_match(b"*sched*", b"slopos_core::sched::test_basic") {
        return TestResult::Fail;
    }
    if !glob_match(b"slopos_mm::*", b"slopos_mm::heap::alloc") {
        return TestResult::Fail;
    }
    if glob_match(b"a", b"ab") {
        return TestResult::Fail;
    }
    if !glob_match(b"a?c", b"abc") {
        return TestResult::Fail;
    }
    TestResult::Pass
}

fn bootstrap_bbb_capture_roundtrip() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    {
        let _g = crate::capture::begin();
        klog_info!("MARKER_X9 hello");
    }
    let log = crate::capture::drain_cpu0();
    let needle = b"MARKER_X9";
    if log.windows(needle.len()).any(|w| w == needle) {
        TestResult::Pass
    } else {
        klog_info!(
            "BOOTSTRAP: capture_roundtrip did not observe marker (log_len={})",
            log.len()
        );
        TestResult::Fail
    }
}

/// Intentionally panic to exercise the harness's panic-isolation path.
/// `FLAG_EXPECTED_PANIC` tells the harness to surface this as a Pass
/// (with `EXPECTED_PANIC` suffix) so the run as a whole stays green.
fn bootstrap_ccc_panic_canary() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    panic!("intentional bootstrap canary panic");
}

/// Verifies the harness recovered cleanly from `bootstrap_ccc`'s panic:
///   1. The counter is at least 3 — proves prior tests (including the
///      canary) ran in order and incremented before the panic.
///   2. The klog backend is not stuck on the buffering capture: a fresh
///      `capture::begin → klog → drain` roundtrip works.
fn bootstrap_ddd_isolation_check() -> TestResult {
    let n = BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    if n < 3 {
        klog_info!(
            "BOOTSTRAP isolation: counter={} (expected >= 3 after canary)",
            n
        );
        return TestResult::Fail;
    }
    {
        let _g = crate::capture::begin();
        klog_info!("MARKER_AFTER_PANIC");
    }
    let log = crate::capture::drain_cpu0();
    if log
        .windows(b"MARKER_AFTER_PANIC".len())
        .any(|w| w == b"MARKER_AFTER_PANIC")
    {
        TestResult::Pass
    } else {
        klog_info!("BOOTSTRAP isolation: post-panic capture roundtrip failed");
        TestResult::Fail
    }
}

crate::stest!(name = bootstrap_aaa_glob_match);
crate::stest!(name = bootstrap_bbb_capture_roundtrip);
crate::stest!(
    name = bootstrap_ccc_panic_canary,
    flags = crate::FLAG_EXPECTED_PANIC
);
crate::stest!(name = bootstrap_ddd_isolation_check);
