//! Self-tests for the test framework itself.
//!
//! The `aaa`/`bbb`/`ccc`/`ddd` suffixes fix the lex order: the panic-isolation
//! check must run after the canary.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::klog_info;

use crate::filter::glob_match;
use crate::TestResult;

/// Incremented on entry to every bootstrap test, including the canary before it
/// panics; the isolation check reads it to confirm prior tests ran.
static BOOTSTRAP_CTR: AtomicU32 = AtomicU32::new(0);
static BOOTSTRAP_DROP_CTR: AtomicU32 = AtomicU32::new(0);

struct PanicCanaryGuard;

impl Drop for PanicCanaryGuard {
    fn drop(&mut self) {
        BOOTSTRAP_DROP_CTR.fetch_add(1, Ordering::Relaxed);
    }
}

fn bootstrap_aaa_glob_match() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    if !glob_match(b"a::*", b"a::b") {
        return TestResult::Fail;
    }
    if glob_match(b"a::*", b"b::a") {
        return TestResult::Fail;
    }
    if !glob_match(b"*sched*", b"slopos_sched::scheduler::test_basic") {
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

fn bootstrap_aab_unwind_backtrace_nonempty() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    let backtrace = slopos_ostd::unwind::capture_backtrace();
    if backtrace.as_slice().is_empty() {
        klog_info!("BOOTSTRAP unwind: _Unwind_Backtrace returned no frames");
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
    let needle = b"MARKER_X9";
    let (found, log_len) = crate::capture::with_cpu0_log(|log| {
        (log.windows(needle.len()).any(|w| w == needle), log.len())
    });
    if found {
        TestResult::Pass
    } else {
        klog_info!(
            "BOOTSTRAP: capture_roundtrip did not observe marker (log_len={})",
            log_len
        );
        TestResult::Fail
    }
}

/// Panics on purpose to exercise the harness's panic-isolation path.
fn bootstrap_ccc_panic_canary() -> TestResult {
    BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    let _guard = PanicCanaryGuard;
    panic!("intentional bootstrap canary panic");
}

/// Verifies the harness recovered cleanly from `bootstrap_ccc`'s panic.
fn bootstrap_ddd_isolation_check() -> TestResult {
    let n = BOOTSTRAP_CTR.fetch_add(1, Ordering::Relaxed);
    if n < 4 {
        klog_info!(
            "BOOTSTRAP isolation: counter={} (expected >= 4 after canary)",
            n
        );
        return TestResult::Fail;
    }
    let drops = BOOTSTRAP_DROP_CTR.load(Ordering::Relaxed);
    if drops != 1 {
        klog_info!(
            "BOOTSTRAP isolation: panic canary Drop count={} (expected 1)",
            drops
        );
        return TestResult::Fail;
    }
    {
        let _g = crate::capture::begin();
        klog_info!("MARKER_AFTER_PANIC");
    }
    let needle = b"MARKER_AFTER_PANIC";
    if crate::capture::with_cpu0_log(|log| log.windows(needle.len()).any(|w| w == needle)) {
        TestResult::Pass
    } else {
        klog_info!("BOOTSTRAP isolation: post-panic capture roundtrip failed");
        TestResult::Fail
    }
}

crate::stest!(name = bootstrap_aaa_glob_match);
crate::stest!(name = bootstrap_aab_unwind_backtrace_nonempty);
crate::stest!(name = bootstrap_bbb_capture_roundtrip);
crate::stest!(
    name = bootstrap_ccc_panic_canary,
    flags = crate::FLAG_EXPECTED_PANIC
);
crate::stest!(name = bootstrap_ddd_isolation_check);
