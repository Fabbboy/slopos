//! Per-test harness: walks the `.test_registry` linker section, runs each
//! `TestDesc` under `catch_panic!` with klog capture installed, and emits
//! KTAP per-test lines plus a final `TESTS SUMMARY:` log line.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::StateFlag;
use slopos_utils::klog_info;

use crate::config::TestConfig;

#[cfg(feature = "tests")]
use crate::config::Verbosity;
#[cfg(feature = "tests")]
use crate::registry::{registry_sorted, TestDesc, TestKind};
#[cfg(feature = "tests")]
use crate::result::TestResult;

/// Default cycles per millisecond estimate (3 GHz).
const DEFAULT_CYCLES_PER_MS: u64 = 3_000_000;

/// Aggregated counters returned to the boot caller.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TestRunSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub over_time: u32,
    pub panics: u32,
    pub elapsed_ms: u32,
}

impl TestRunSummary {
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.panics == 0
    }
}

// =============================================================================
// Time measurement utilities
// =============================================================================

static CACHED_CYCLES_PER_MS: SyncUnsafeCell<u64> = SyncUnsafeCell::new(0);

fn cached_cycles_per_ms_mut() -> *mut u64 {
    CACHED_CYCLES_PER_MS.get()
}

/// Estimate CPU cycles per millisecond using CPUID if available.
pub fn estimate_cycles_per_ms() -> u64 {
    unsafe {
        if *cached_cycles_per_ms_mut() != 0 {
            return *cached_cycles_per_ms_mut();
        }
    }

    let (max_leaf, _, _, _) = slopos_arch::cpu::cpuid(0);
    let mut cycles_per_ms = DEFAULT_CYCLES_PER_MS;
    if max_leaf >= 0x16 {
        let (freq_mhz, _, _, _) = slopos_arch::cpu::cpuid(0x16);
        if freq_mhz != 0 {
            cycles_per_ms = freq_mhz as u64 * 1_000;
        }
    }

    unsafe {
        *cached_cycles_per_ms_mut() = cycles_per_ms;
    }
    cycles_per_ms
}

/// Convert TSC cycles to milliseconds.
pub fn cycles_to_ms(cycles: u64) -> u32 {
    let cycles_per_ms = estimate_cycles_per_ms();
    if cycles_per_ms == 0 {
        return 0;
    }
    let ms = cycles / cycles_per_ms;
    if ms > u32::MAX as u64 {
        return u32::MAX;
    }
    ms as u32
}

/// Measure elapsed time in milliseconds between two TSC readings.
#[inline]
pub fn measure_elapsed_ms(start: u64, end: u64) -> u32 {
    cycles_to_ms(end.wrapping_sub(start))
}

// =============================================================================
// Panic glue
// =============================================================================

static PANIC_SEEN: StateFlag = StateFlag::new();
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

pub fn tests_reset_panic_state() {
    PANIC_SEEN.set_inactive();
    PANIC_REPORTED.store(false, Ordering::Relaxed);
}

pub fn tests_mark_panic() {
    PANIC_SEEN.set_active();
    if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
        klog_info!("TESTS: panic observed");
    }
}

#[cfg(feature = "qemu-exit")]
pub fn tests_request_shutdown(failed: i32) {
    crate::qemu_signal::qemu_signal_exit(failed);
}

#[cfg(not(feature = "qemu-exit"))]
pub fn tests_request_shutdown(_failed: i32) {
    // No-op when qemu-exit feature is disabled (production builds).
}

// =============================================================================
// Harness driver
// =============================================================================

#[cfg(feature = "tests")]
const FQN_BUF_BYTES: usize = 512;

/// Render `module::name` into a stack scratch buffer; returns the populated
/// prefix slice. Truncates if the buffer is too small (test names that long
/// don't exist in this kernel).
#[cfg(feature = "tests")]
fn full_name_into<'a>(desc: &TestDesc, buf: &'a mut [u8; FQN_BUF_BYTES]) -> &'a [u8] {
    let m = desc.module.as_bytes();
    let n = desc.name.as_bytes();
    let mut i = 0usize;
    let cap = buf.len();
    let take_m = m.len().min(cap.saturating_sub(i));
    buf[i..i + take_m].copy_from_slice(&m[..take_m]);
    i += take_m;
    if i + 2 <= cap {
        buf[i] = b':';
        buf[i + 1] = b':';
        i += 2;
    }
    let take_n = n.len().min(cap.saturating_sub(i));
    buf[i..i + take_n].copy_from_slice(&n[..take_n]);
    i += take_n;
    &buf[..i]
}

#[cfg(not(feature = "tests"))]
pub fn tests_run_all(_config: &TestConfig, _summary: &mut TestRunSummary) -> i32 {
    // No-op in production: the harness body would spike stack frames
    // above the 2 KiB gate. The real body compiles under the `tests`
    // feature (which lifts the gate via scripts/build_kernel.sh).
    0
}

#[cfg(not(feature = "tests"))]
pub fn tests_run_userland(_config: &TestConfig, _summary: &mut TestRunSummary) -> i32 {
    // No-op in production builds; the userland-test runner does not exist.
    0
}

#[cfg(feature = "tests")]
pub fn tests_run_all(cfg: &TestConfig, summary: &mut TestRunSummary) -> i32 {
    run_phase(cfg, summary, TestKind::Kernel, "kernel")
}

/// Run only `TestKind::Userland` entries. Invoked from the `services`
/// phase (after `boot_step_init_launch`) so the runner can spawn `/bin/*`
/// binaries against a mounted filesystem with `init` already running.
///
/// Emits its own KTAP plan/header/footer + `TESTS SUMMARY:` line so each
/// phase is self-contained. The host-side wrapper (Phase 4) merges the
/// two phases into a single dotted progress display.
#[cfg(feature = "tests")]
pub fn tests_run_userland(cfg: &TestConfig, summary: &mut TestRunSummary) -> i32 {
    run_phase(cfg, summary, TestKind::Userland, "userland")
}

#[cfg(feature = "tests")]
fn run_phase(
    cfg: &TestConfig,
    summary: &mut TestRunSummary,
    kind_filter: TestKind,
    phase_label: &str,
) -> i32 {
    *summary = TestRunSummary::default();
    if !cfg.enabled {
        klog_info!("TESTS: Harness disabled ({} phase)", phase_label);
        return 0;
    }

    klog_info!("TESTS: Starting {} phase", phase_label);
    register_panic_klog_cleanup();

    let descs = match registry_sorted() {
        Ok(v) => v,
        Err(_) => {
            klog_info!("TESTS: registry_sorted alloc failed");
            return -1;
        }
    };

    // First pass: count entries that pass the kind filter AND the user
    // glob filter so the KTAP plan matches actual emission count.
    let mut name_buf = [0u8; FQN_BUF_BYTES];
    let mut planned: u32 = 0;
    for desc in &descs {
        if desc.kind != kind_filter {
            continue;
        }
        let fqn = full_name_into(desc, &mut name_buf);
        if cfg.passes_filter(fqn) {
            planned += 1;
        }
    }
    crate::ktap::emit_header(planned);

    let start_cycles = slopos_arch::tsc::rdtsc();
    let mut idx: u32 = 0;
    let mut bailed = false;

    for desc in &descs {
        if PANIC_SEEN.is_active() {
            summary.panics = summary.panics.saturating_add(1);
            if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                klog_info!("TESTS: panic flagged, stopping registry walk");
            }
            break;
        }
        if desc.kind != kind_filter {
            continue;
        }
        let fqn = full_name_into(desc, &mut name_buf);
        if !cfg.passes_filter(fqn) {
            continue;
        }
        idx += 1;

        let outcome = run_one(desc, cfg, idx);

        match outcome.outcome {
            TestResult::Pass | TestResult::OverTime | TestResult::Skipped => {
                summary.passed = summary.passed.saturating_add(1);
            }
            TestResult::Fail => {
                summary.failed = summary.failed.saturating_add(1);
            }
            TestResult::Panic => {
                summary.panics = summary.panics.saturating_add(1);
                summary.failed = summary.failed.saturating_add(1);
            }
        }
        if outcome.outcome == TestResult::OverTime {
            summary.over_time = summary.over_time.saturating_add(1);
        }

        if outcome.bail {
            crate::ktap::emit_bail(desc.name);
            bailed = true;
            break;
        }
    }

    summary.total = idx;
    let end_cycles = slopos_arch::tsc::rdtsc();
    summary.elapsed_ms = measure_elapsed_ms(start_cycles, end_cycles);

    klog_info!(
        "TESTS SUMMARY ({} phase): total={} passed={} failed={} elapsed_ms={}",
        phase_label,
        summary.total,
        summary.passed,
        summary.failed,
        summary.elapsed_ms,
    );

    crate::ktap::emit_footer(
        summary.elapsed_ms,
        summary.passed,
        summary.failed,
        summary.skipped,
        summary.over_time,
    );

    if bailed || summary.failed > 0 {
        -1
    } else {
        0
    }
}

#[cfg(feature = "tests")]
struct OutcomeRecord {
    outcome: TestResult,
    bail: bool,
}

#[cfg(feature = "tests")]
fn run_one(desc: &TestDesc, cfg: &TestConfig, idx: u32) -> OutcomeRecord {
    let truncated;
    let raw_outcome;
    let time_ms;
    let captured_log: &[u8];
    {
        let _g = crate::capture::begin();
        let t0 = slopos_arch::tsc::rdtsc();
        raw_outcome = (desc.run)();
        let t1 = slopos_arch::tsc::rdtsc();
        time_ms = measure_elapsed_ms(t0, t1);
        // Userland-test thunks run from `/sbin/init`'s syscall context and
        // can execute on any CPU; their klog (subtest emits, exit-record
        // diagnostics) lands in *that* CPU's per-CPU ring, not CPU 0. Use
        // `drain_current_cpu` so the YAML log block is populated regardless
        // of which CPU dispatched the harness.
        captured_log = crate::capture::drain_current_cpu();
        truncated = crate::capture::truncated_bytes();
    }

    // EXPECTED_PANIC: a test marked with the flag treats a Panic outcome
    // as Pass-with-suffix. Used by the bootstrap panic-isolation canary.
    let expected_panic = (desc.flags & crate::registry::FLAG_EXPECTED_PANIC) != 0;
    let outcome = if raw_outcome == TestResult::Panic && expected_panic {
        TestResult::Pass
    } else {
        raw_outcome
    };

    let final_outcome = if outcome == TestResult::Pass && cfg.warn_ms > 0 && time_ms > cfg.warn_ms {
        TestResult::OverTime
    } else {
        outcome
    };

    let suppress_pass = matches!(cfg.verbosity, Verbosity::Quiet);
    let pass_suffix: Option<&str> = if expected_panic && raw_outcome == TestResult::Panic {
        Some("EXPECTED_PANIC")
    } else if final_outcome == TestResult::OverTime {
        Some("OVER_TIME")
    } else {
        None
    };

    match final_outcome {
        TestResult::Pass | TestResult::OverTime => {
            if !suppress_pass {
                crate::ktap::emit_ok(idx, desc, time_ms, pass_suffix);
            }
            if matches!(cfg.verbosity, Verbosity::Verbose) {
                emit_verbose_log(captured_log, truncated);
            }
        }
        TestResult::Skipped => {
            if !suppress_pass {
                crate::ktap::emit_skip(idx, desc, "test returned Skipped");
            }
        }
        TestResult::Fail | TestResult::Panic => {
            crate::ktap::emit_not_ok(idx, desc, time_ms, final_outcome, captured_log, truncated);
        }
    }

    let bail = desc.name.starts_with("bootstrap_") && final_outcome.is_failure();
    OutcomeRecord {
        outcome: final_outcome,
        bail,
    }
}

#[cfg(feature = "tests")]
fn emit_verbose_log(cpu0_log: &[u8], truncated_bytes: usize) {
    let has_foreign = crate::capture::drain_all().any(|(cpu, slice)| cpu != 0 && !slice.is_empty());
    if cpu0_log.is_empty() && !has_foreign {
        return;
    }
    klog_info!("KTAP\t  ---");
    klog_info!("KTAP\t  log: |");
    for line in cpu0_log.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let s = core::str::from_utf8(line).unwrap_or("<non-utf8 log line>");
        klog_info!("KTAP\t   {}", s);
    }
    if truncated_bytes > 0 {
        klog_info!(
            "KTAP\t   [cpu0 tail trimmed: {} bytes lost to ring overflow]",
            truncated_bytes
        );
    }
    for (cpu, slice) in crate::capture::drain_all() {
        if cpu == 0 || slice.is_empty() {
            continue;
        }
        klog_info!("KTAP\t   --- cpu{} ---", cpu);
        for line in slice.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let s = core::str::from_utf8(line).unwrap_or("<non-utf8 log line>");
            klog_info!("KTAP\t   {}", s);
        }
    }
    klog_info!("KTAP\t  ...");
}

#[cfg(feature = "tests")]
static PANIC_HOOK_REGISTERED: StateFlag = StateFlag::new();

#[cfg(feature = "tests")]
fn register_panic_klog_cleanup() {
    if PANIC_HOOK_REGISTERED.is_active() {
        return;
    }
    PANIC_HOOK_REGISTERED.set_active();
    slopos_utils::panic_recovery::register_panic_cleanup(klog_panic_cleanup);
}

#[cfg(feature = "tests")]
fn klog_panic_cleanup() {
    slopos_utils::klog_force_restore_default();
}
