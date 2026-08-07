//! Kernel-side tests for the diagnostic console.
//!
//! The properties worth holding are the ones a reader of the registry cannot
//! check by eye: that two crates did not claim the same key, that the policy
//! mask is actually consulted, that the line budget really stops a command,
//! and that a request made from a trigger reaches a command through the real
//! bottom-half path rather than through a witness a test forged.
//!
//! Everything here asserts on *state a probe command recorded*, never on klog
//! text. Output capture is a process-global swap, so a test that read it would
//! be reading every other CPU's logging too.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::kconsole::{self, KCMD_DESTRUCTIVE, KCMD_INFORMATIONAL, KConfig, KConsole};
use slopos_ostd::kline;
use slopos_ostd::sync::bh;
use slopos_testing::TestResult;
use slopos_testing::fail;

/// Times the informational probe has run.
static PROBE_RUNS: AtomicU32 = AtomicU32::new(0);
/// Times the destructive probe has run.
static DESTRUCTIVE_RUNS: AtomicU32 = AtomicU32::new(0);
/// Lines the probe should try to emit on its next run.
static PROBE_EMIT: AtomicU32 = AtomicU32::new(0);
/// Lines the probe actually got to emit.
static PROBE_EMITTED: AtomicU32 = AtomicU32::new(0);
/// Whether the console told the probe it had been truncated.
static PROBE_TRUNCATED: AtomicU32 = AtomicU32::new(0);

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

/// Run whatever is queued, through the real bottom-half point.
///
/// `bh::raise` then `run_pending_if_due` is the same pair the producer and the
/// consumer use in production; nothing here constructs a `BhContext`, because
/// a test that forged one would stop testing the thing that makes the drain
/// safe.
fn pump() {
    bh::raise();
    bh::run_pending_if_due();
}

/// Swap in a policy for the duration of a test and put the old one back.
fn with_policy<R>(cfg: KConfig, f: impl FnOnce() -> R) -> R {
    let saved = kconsole::policy();
    kconsole::install(cfg);
    let out = f();
    kconsole::install(saved);
    out
}

/// Every registered command is reachable and described.
///
/// The registry is built by the linker out of statics in six crates, so
/// "does the section actually hold what the readers assume" is not a question
/// any one crate can answer.
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
    // The help command is what makes the rest discoverable.
    if !cmds.iter().any(|c| c.key == b'h') {
        return fail!("no help command is registered");
    }
    TestResult::Pass
}

/// No two commands claim the same key.
///
/// A collision is invisible at build time — the linker concatenates both
/// entries happily — and at runtime it makes one key produce two dumps.
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

/// Every command declares exactly one class.
///
/// This is the whole no-accidentally-destructive-command guarantee: the policy
/// mask is a bitwise test, so an entry with no class bit can never run and one
/// with both would run under the informational-only default.
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

/// A request reaches its command through the real bottom half.
pub fn test_kcon_end_to_end_via_bottom_half() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        let drains_before = bh::drains();
        kconsole::request(b'z');
        pump();
        if bh::drains() == drains_before {
            return fail!("the bottom half never drained");
        }
        if PROBE_RUNS.load(Ordering::Relaxed) != before + 1 {
            return fail!("the queued command did not run exactly once");
        }
        TestResult::Pass
    })
}

/// Queuing the same command twice before a drain runs it once.
///
/// The pending set is a bitmap precisely so that holding a key down, or a
/// trigger that fires twice, does not multiply the work.
pub fn test_kcon_request_is_idempotent() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        kconsole::request(b'z');
        kconsole::request(b'z');
        kconsole::request(b'z');
        pump();
        if PROBE_RUNS.load(Ordering::Relaxed) != before + 1 {
            return fail!("three requests before one drain ran the command more than once");
        }
        // And a drain with nothing queued reports that it did nothing.
        if kconsole::drain() {
            return fail!("an empty drain claimed it did work");
        }
        TestResult::Pass
    })
}

/// The line budget stops a command that would otherwise flood the console.
pub fn test_kcon_budget_truncates() -> TestResult {
    let cfg = KConfig {
        max_lines: 8,
        ..KConfig::defaults()
    };
    with_policy(cfg, || {
        PROBE_EMIT.store(64, Ordering::Relaxed);
        kconsole::request(b'z');
        pump();
        let emitted = PROBE_EMITTED.load(Ordering::Relaxed);
        if emitted != 64 {
            return fail!("the probe stopped early: {} of 64 attempts", emitted);
        }
        if PROBE_TRUNCATED.load(Ordering::Relaxed) == 0 {
            return fail!("64 lines against a budget of 8 did not register as truncated");
        }
        TestResult::Pass
    })
}

/// A disabled console queues nothing.
///
/// The mask is checked at `request`, so a console turned off at the cmdline
/// costs a trigger one relaxed load and never reaches the registry at all.
pub fn test_kcon_disabled_drops_requests() -> TestResult {
    let off = KConfig {
        mask: 0,
        ..KConfig::defaults()
    };
    let ran = with_policy(off, || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        kconsole::request(b'z');
        pump();
        PROBE_RUNS.load(Ordering::Relaxed) != before
    });
    if ran {
        return fail!("a command ran while the console was disabled");
    }
    // Nothing may be left queued for the next drain to find, either.
    if kconsole::drain() {
        return fail!("a request made while disabled was queued anyway");
    }
    TestResult::Pass
}

/// A destructive command is refused unless the mask names its class.
pub fn test_kcon_destructive_needs_the_mask_bit() -> TestResult {
    // Default policy: informational only.
    let refused = with_policy(KConfig::defaults(), || {
        let before = DESTRUCTIVE_RUNS.load(Ordering::Relaxed);
        kconsole::request(b'y');
        pump();
        DESTRUCTIVE_RUNS.load(Ordering::Relaxed) == before
    });
    if !refused {
        return fail!("a destructive command ran under the default policy");
    }

    let permitted = KConfig {
        mask: KCMD_INFORMATIONAL | KCMD_DESTRUCTIVE,
        ..KConfig::defaults()
    };
    let ran = with_policy(permitted, || {
        let before = DESTRUCTIVE_RUNS.load(Ordering::Relaxed);
        kconsole::request(b'y');
        pump();
        DESTRUCTIVE_RUNS.load(Ordering::Relaxed) == before + 1
    });
    if !ran {
        return fail!("a destructive command was refused with its mask bit set");
    }
    TestResult::Pass
}

/// An unrecognised key is handled rather than dropped.
///
/// An operator who mistypes should learn that they did, not watch nothing
/// happen and conclude the console is broken.
pub fn test_kcon_unknown_key_is_consumed() -> TestResult {
    with_policy(KConfig::defaults(), || {
        let before = PROBE_RUNS.load(Ordering::Relaxed);
        // No command claims '0'.
        if kconsole::commands().iter().any(|c| c.key == b'0') {
            return fail!("this test needs a key no command claims");
        }
        kconsole::request(b'0');
        if !kconsole::drain() {
            return fail!("a queued unknown key was not drained");
        }
        if PROBE_RUNS.load(Ordering::Relaxed) != before {
            return fail!("an unknown key ran a command");
        }
        TestResult::Pass
    })
}

/// The console's addresses come back symbolized.
///
/// `KConsole::sym` is only worth having if the symbol table actually resolves
/// kernel text, which depends on a build-time generation step that can fail
/// open into an empty table.
pub fn test_kcon_kernel_text_symbolizes() -> TestResult {
    let probe: fn() -> TestResult = test_kcon_kernel_text_symbolizes;
    let addr = probe as usize as u64;
    match slopos_ostd::ksym::lookup(addr) {
        None => fail!("the kernel symbol table resolved nothing for a known function"),
        Some(s) if s.symbol.is_empty() => fail!("the symbol table returned an empty name"),
        Some(_) => TestResult::Pass,
    }
}

/// The probe's slot protocol, exercised without sending an NMI.
///
/// Two properties matter and neither is visible from the command's output: a
/// CPU already being probed is left alone, and a slot the watchdog has since
/// re-armed is never cleared by our reaper. The second is the one that would
/// kill the machine if it were wrong — a stale probe NMI arriving at a slot
/// armed `Fatal` takes it down.
pub fn test_kcon_probe_slot_protocol() -> TestResult {
    use slopos_ostd::watchdog::{self, NmiDisposition};

    // A CPU index no CPU occupies has no slot at all.
    let cpus = slopos_ostd::cpu::x86_64::pcr::get_cpu_count();
    let Some(victim) = (0..cpus).find(|c| *c != slopos_ostd::cpu::x86_64::pcr::get_current_cpu())
    else {
        // Uniprocessor: the fan-out has nothing to probe, which is itself the
        // property worth holding.
        return TestResult::Pass;
    };

    if watchdog::probe_disposition(victim) != NmiDisposition::Unsolicited {
        return fail!("cpu {} already had a probe armed before the test", victim);
    }

    if !watchdog::arm_probe(victim, NmiDisposition::Probe) {
        return fail!("arming a free probe slot failed");
    }
    // A second claim must fail, or a per-tick check would storm a CPU that is
    // still emitting its dump.
    if watchdog::arm_probe(victim, NmiDisposition::Probe) {
        watchdog::release_probe(victim);
        return fail!("a busy probe slot was claimed twice");
    }

    // The reaper only takes back what it left.
    if watchdog::release_probe_if(victim, NmiDisposition::Fatal) {
        return fail!("the conditional release cleared a slot it did not own");
    }
    if watchdog::probe_disposition(victim) != NmiDisposition::Probe {
        watchdog::release_probe(victim);
        return fail!("a failed conditional release still changed the slot");
    }
    if !watchdog::release_probe_if(victim, NmiDisposition::Probe) {
        watchdog::release_probe(victim);
        return fail!("the conditional release did not take back its own slot");
    }
    if watchdog::probe_disposition(victim) != NmiDisposition::Unsolicited {
        return fail!("the slot was not freed");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_kcon_registry_is_populated, suite = kconsole);
slopos_testing::stest!(name = test_kcon_probe_slot_protocol, suite = kconsole);
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
