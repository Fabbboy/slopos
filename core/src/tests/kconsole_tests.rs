//! Kernel-side tests for the diagnostic console.
//!
//! These assert on state a probe command recorded rather than on log text,
//! because swapping the klog backend is process-global.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_ostd::cpu::x86_64::interrupts::IrqDisabled;
use slopos_ostd::kconsole::{self, KCMD_DESTRUCTIVE, KCMD_INFORMATIONAL, KConfig, KConsole};
use slopos_ostd::kline;
use slopos_ostd::sync::bh;
use slopos_sched::per_cpu::{ApPauseError, pause_all_aps};
use slopos_testing::TestResult;
use slopos_testing::fail;

static PROBE_RUNS: AtomicU32 = AtomicU32::new(0);
static DESTRUCTIVE_RUNS: AtomicU32 = AtomicU32::new(0);
/// Lines the probe should try to emit on its next run.
static PROBE_EMIT: AtomicU32 = AtomicU32::new(0);
/// Lines the probe actually got to emit.
static PROBE_EMITTED: AtomicU32 = AtomicU32::new(0);
static PROBE_TRUNCATED: AtomicU32 = AtomicU32::new(0);
static PROBE_MARK: AtomicBool = AtomicBool::new(false);

/// A line no other test asks for, so a ring scan cannot match an earlier probe's output.
const MARKER: &str = "kconsole probe marker";

slopos_ostd::kcommand! {
    name = probe,
    key = b'z',
    help = "test probe (test-hooks builds only)",
    flags = KCMD_INFORMATIONAL,
    run = run_probe,
}

slopos_ostd::kcommand! {
    name = probe_destructive,
    key = b'y',
    help = "destructive test probe (test-hooks builds only)",
    flags = KCMD_DESTRUCTIVE,
    run = run_probe_destructive,
}

fn run_probe(kc: &mut KConsole<'_>) {
    PROBE_RUNS.fetch_add(1, Ordering::Relaxed);
    if PROBE_MARK.swap(false, Ordering::Relaxed) {
        kline!(kc, "{}", MARKER);
    }
    let want = PROBE_EMIT.swap(0, Ordering::Relaxed);
    let mut emitted = 0u32;
    for i in 0..want {
        kline!(kc, "kconsole probe line {}", i);
        emitted += 1;
    }
    PROBE_EMITTED.store(emitted, Ordering::Relaxed);
    PROBE_TRUNCATED.store(kc.truncated() as u32, Ordering::Relaxed);
}

fn run_probe_destructive(_kc: &mut KConsole<'_>) {
    DESTRUCTIVE_RUNS.fetch_add(1, Ordering::Relaxed);
}

/// Run whatever is queued through the real bottom-half point; nothing here
/// forges a `BhContext`.
fn pump() {
    bh::raise();
    bh::run_pending_if_due();
}

/// A parked AP runs no bottom half, leaving the machine-wide pending bitmap one claimant.
fn with_aps_parked<R>(f: impl FnOnce() -> R) -> Result<R, ApPauseError> {
    let _parked = pause_all_aps()?;
    Ok(f())
}

/// Masks this CPU's interrupts too, so `f` must call `kconsole::drain` rather than [`pump`].
fn with_sole_drain<R>(f: impl FnOnce() -> R) -> Result<R, ApPauseError> {
    with_aps_parked(|| IrqDisabled::with(|_irq| f()))
}

/// `klog_len` saturates once the ring is full, so an offset captured earlier cannot start a scan.
const SCAN_TAIL: usize = 16 * 1024;

/// Windows overlap by `needle.len() - 1` so a needle straddling a read boundary is still found.
fn klog_holds(needle: &[u8]) -> Option<bool> {
    const WINDOW: usize = 8192;

    let mut buf = slopos_ostd::KBox::<[u8; WINDOW]>::zeroed().ok()?;
    let end = slopos_ostd::klog::klog_len();
    let mut offset = end.saturating_sub(SCAN_TAIL);
    while offset < end {
        let read = slopos_ostd::klog::klog_read(offset, &mut buf[..]);
        if read < needle.len() {
            break;
        }
        if buf[..read].windows(needle.len()).any(|w| w == needle) {
            return Some(true);
        }
        offset += read - (needle.len() - 1);
    }
    Some(false)
}

/// Swap in a policy for the duration of a test and put the old one back.
fn with_policy<R>(cfg: KConfig, f: impl FnOnce() -> R) -> R {
    let saved = kconsole::policy();
    kconsole::install(cfg);
    let out = f();
    kconsole::install(saved);
    out
}

pub fn test_kcon_registry_is_populated() -> TestResult {
    let cmds = kconsole::commands();
    if cmds.is_empty() {
        return fail!("the kconsole registry is empty — the link section is not being collected");
    }
    for cmd in cmds {
        if cmd.name.is_empty() || cmd.help.is_empty() {
            return fail!("command '{}' has no name or no help text", cmd.key as char);
        }
        if !cmd.key.is_ascii_graphic() {
            return fail!(
                "command '{}' has a non-graphic key {:#x}",
                cmd.name,
                cmd.key
            );
        }
    }
    if !cmds.iter().any(|c| c.key == b'h') {
        return fail!("no help command is registered");
    }
    TestResult::Pass
}

/// The linker concatenates colliding registry entries happily; at runtime one
/// key then produces two dumps.
pub fn test_kcon_keys_are_unique() -> TestResult {
    let cmds = kconsole::commands();
    for (i, a) in cmds.iter().enumerate() {
        for b in &cmds[i + 1..] {
            if a.key == b.key {
                return fail!(
                    "commands '{}' and '{}' both claim key '{}'",
                    a.name,
                    b.name,
                    a.key as char
                );
            }
        }
    }
    TestResult::Pass
}

/// The mask is a bitwise test: an entry with no class bit never runs, and one
/// with both runs under the informational-only default.
pub fn test_kcon_flags_are_exclusive() -> TestResult {
    for cmd in kconsole::commands() {
        let class = cmd.flags & (KCMD_INFORMATIONAL | KCMD_DESTRUCTIVE);
        if class != KCMD_INFORMATIONAL && class != KCMD_DESTRUCTIVE {
            return fail!(
                "command '{}' declares flags {:#x}, which is not exactly one class",
                cmd.name,
                cmd.flags
            );
        }
    }
    TestResult::Pass
}

pub fn test_kcon_end_to_end_via_bottom_half() -> TestResult {
    with_policy(KConfig::defaults(), || {
        // Interrupts stay on: this is the one test that must reach the real bottom-half point.
        let observed = with_aps_parked(|| {
            let before = PROBE_RUNS.load(Ordering::Relaxed);
            let drains_before = bh::drains();
            kconsole::request(b'z');
            pump();
            (
                bh::drains() != drains_before,
                PROBE_RUNS.load(Ordering::Relaxed) == before + 1,
            )
        });
        let (drained, ran_once) = match observed {
            Ok(pair) => pair,
            Err(err) => return fail!("the APs would not park: {:?}", err),
        };
        if !drained {
            return fail!("the bottom half never drained");
        }
        if !ran_once {
            return fail!("the queued command did not run exactly once");
        }
        TestResult::Pass
    })
}

pub fn test_kcon_request_is_idempotent() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        let drains = with_sole_drain(|| {
            kconsole::request(b'z');
            kconsole::request(b'z');
            kconsole::request(b'z');
            (kconsole::drain(), kconsole::drain())
        });
        let (claimed, empty) = match drains {
            Ok(pair) => pair,
            Err(err) => return fail!("the APs would not park: {:?}", err),
        };
        if !claimed {
            return fail!("the drain after three requests reported no work");
        }
        if PROBE_RUNS.load(Ordering::Relaxed) != before + 1 {
            return fail!("three requests before one drain ran the command more than once");
        }
        if empty {
            return fail!("an empty drain claimed it did work");
        }
        TestResult::Pass
    })
}

pub fn test_kcon_budget_truncates() -> TestResult {
    let cfg = KConfig {
        max_lines: 8,
        ..KConfig::defaults()
    };
    let observed = with_policy(cfg, || {
        with_sole_drain(|| {
            PROBE_EMIT.store(64, Ordering::Relaxed);
            kconsole::request(b'z');
            let claimed = kconsole::drain();
            (
                claimed,
                PROBE_EMITTED.load(Ordering::Relaxed),
                PROBE_TRUNCATED.load(Ordering::Relaxed),
            )
        })
    });
    let (claimed, emitted, truncated) = match observed {
        Ok(triple) => triple,
        Err(err) => return fail!("the APs would not park: {:?}", err),
    };
    if !claimed {
        return fail!("the drain never claimed the request, so the counters are stale");
    }
    if emitted != 64 {
        return fail!("the probe stopped early: {} of 64 attempts", emitted);
    }
    if truncated == 0 {
        return fail!("64 lines against a budget of 8 did not register as truncated");
    }
    TestResult::Pass
}

/// The mask is checked at `request`, so a trigger never reaches the registry
/// at all.
pub fn test_kcon_disabled_drops_requests() -> TestResult {
    let off = KConfig {
        mask: 0,
        ..KConfig::defaults()
    };
    let observed = with_policy(off, || {
        with_sole_drain(|| {
            let before = PROBE_RUNS.load(Ordering::Relaxed);
            kconsole::request(b'z');
            let queued = kconsole::drain();
            (queued, PROBE_RUNS.load(Ordering::Relaxed) != before)
        })
    });
    let (queued, ran) = match observed {
        Ok(pair) => pair,
        Err(err) => return fail!("the APs would not park: {:?}", err),
    };
    if ran {
        return fail!("a command ran while the console was disabled");
    }
    if queued {
        return fail!("a request made while disabled was queued anyway");
    }
    TestResult::Pass
}

pub fn test_kcon_destructive_needs_the_mask_bit() -> TestResult {
    let refused = with_policy(KConfig::defaults(), || {
        with_sole_drain(|| {
            let before = DESTRUCTIVE_RUNS.load(Ordering::Relaxed);
            kconsole::request(b'y');
            let claimed = kconsole::drain();
            (claimed, DESTRUCTIVE_RUNS.load(Ordering::Relaxed) == before)
        })
    });
    match refused {
        Ok((true, true)) => {}
        Ok((false, _)) => return fail!("the request was never queued, so nothing was refused"),
        Ok((true, false)) => return fail!("a destructive command ran under the default policy"),
        Err(err) => return fail!("the APs would not park: {:?}", err),
    }

    let permitted = KConfig {
        mask: KCMD_INFORMATIONAL | KCMD_DESTRUCTIVE,
        ..KConfig::defaults()
    };
    let ran = with_policy(permitted, || {
        with_sole_drain(|| {
            let before = DESTRUCTIVE_RUNS.load(Ordering::Relaxed);
            kconsole::request(b'y');
            kconsole::drain();
            DESTRUCTIVE_RUNS.load(Ordering::Relaxed) == before + 1
        })
    });
    match ran {
        Ok(true) => TestResult::Pass,
        Ok(false) => fail!("a destructive command was refused with its mask bit set"),
        Err(err) => fail!("the APs would not park: {:?}", err),
    }
}

pub fn test_kcon_unknown_key_is_consumed() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        if kconsole::commands().iter().any(|c| c.key == b'0') {
            return fail!("this test needs a key no command claims");
        }
        let drained = match with_sole_drain(|| {
            kconsole::request(b'0');
            kconsole::drain()
        }) {
            Ok(drained) => drained,
            Err(err) => return fail!("the APs would not park: {:?}", err),
        };
        if !drained {
            return fail!("a queued unknown key was not drained");
        }
        if PROBE_RUNS.load(Ordering::Relaxed) != before {
            return fail!("an unknown key ran a command");
        }
        TestResult::Pass
    })
}

/// The build-time symbol table can fail open into an empty one.
pub fn test_kcon_kernel_text_symbolizes() -> TestResult {
    let probe: fn() -> TestResult = test_kcon_kernel_text_symbolizes;
    let addr = probe as usize as u64;
    match slopos_ostd::ksym::lookup(addr) {
        None => fail!("the kernel symbol table resolved nothing for a known function"),
        Some(s) if s.symbol.is_empty() => fail!("the symbol table returned an empty name"),
        Some(_) => TestResult::Pass,
    }
}

/// Everything else here only proves dispatch worked; `log_forced` bypasses the
/// level filter.
pub fn test_kcon_output_reaches_the_log_ring() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let len_before = slopos_ostd::klog::klog_len();
        let ran = with_aps_parked(|| {
            let runs_before = PROBE_RUNS.load(Ordering::Relaxed);
            PROBE_MARK.store(true, Ordering::Relaxed);
            kconsole::request(b'z');
            pump();
            PROBE_RUNS.load(Ordering::Relaxed) != runs_before
        });
        match ran {
            Ok(true) => {}
            Ok(false) => {
                PROBE_MARK.store(false, Ordering::Relaxed);
                return fail!("the command never ran, so the ring had nothing to gain");
            }
            Err(err) => {
                PROBE_MARK.store(false, Ordering::Relaxed);
                return fail!("the APs would not park: {:?}", err);
            }
        }
        match klog_holds(MARKER.as_bytes()) {
            Some(true) => TestResult::Pass,
            Some(false) => fail!(
                "the command's output never reached the ring (ring length {} -> {})",
                len_before,
                slopos_ostd::klog::klog_len()
            ),
            None => fail!("could not allocate the read window"),
        }
    })
}

/// The probe's slot protocol, exercised without sending an NMI: a CPU already
/// being probed is left alone, and the reaper never clears a slot the watchdog
/// has re-armed — a stale probe NMI at a `Fatal` slot takes the machine down.
pub fn test_kcon_probe_slot_protocol() -> TestResult {
    use slopos_ostd::watchdog::{self, NmiDisposition};

    // An index no machine this runs on has, so the lockup detector's own probes cannot contend.
    const VICTIM: usize = slopos_arch::MAX_CPUS - 1;

    if watchdog::probe_disposition(VICTIM) != NmiDisposition::Unsolicited {
        return fail!("cpu {} already had a probe armed before the test", VICTIM);
    }

    if !watchdog::arm_probe(VICTIM, NmiDisposition::Probe) {
        return fail!("arming a free probe slot failed");
    }
    // A second claim must fail, or a per-tick check would storm a CPU still
    // emitting its dump.
    if watchdog::arm_probe(VICTIM, NmiDisposition::Probe) {
        watchdog::release_probe(VICTIM);
        return fail!("a busy probe slot was claimed twice");
    }

    if watchdog::release_probe_if(VICTIM, NmiDisposition::Fatal) {
        return fail!("the conditional release cleared a slot it did not own");
    }
    if watchdog::probe_disposition(VICTIM) != NmiDisposition::Probe {
        watchdog::release_probe(VICTIM);
        return fail!("a failed conditional release still changed the slot");
    }
    if !watchdog::release_probe_if(VICTIM, NmiDisposition::Probe) {
        watchdog::release_probe(VICTIM);
        return fail!("the conditional release did not take back its own slot");
    }
    if watchdog::probe_disposition(VICTIM) != NmiDisposition::Unsolicited {
        return fail!("the slot was not freed");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_kcon_registry_is_populated, suite = kconsole);
slopos_testing::stest!(name = test_kcon_probe_slot_protocol, suite = kconsole);
slopos_testing::stest!(
    name = test_kcon_output_reaches_the_log_ring,
    suite = kconsole
);
slopos_testing::stest!(name = test_kcon_keys_are_unique, suite = kconsole);
slopos_testing::stest!(name = test_kcon_flags_are_exclusive, suite = kconsole);
slopos_testing::stest!(
    name = test_kcon_end_to_end_via_bottom_half,
    suite = kconsole
);
slopos_testing::stest!(name = test_kcon_request_is_idempotent, suite = kconsole);
slopos_testing::stest!(name = test_kcon_budget_truncates, suite = kconsole);
slopos_testing::stest!(name = test_kcon_disabled_drops_requests, suite = kconsole);
slopos_testing::stest!(
    name = test_kcon_destructive_needs_the_mask_bit,
    suite = kconsole
);
slopos_testing::stest!(name = test_kcon_unknown_key_is_consumed, suite = kconsole);
slopos_testing::stest!(name = test_kcon_kernel_text_symbolizes, suite = kconsole);
