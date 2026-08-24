# Surviving a descheduled vCPU

Making the kernel's liveness bounds robust when the *host* stops running a
guest vCPU. Written against `273ed3ba`.

Supersedes the "fix is not scheduled work" note in the
`KNOWN_ISSUES.md` entry *"Aging-backstop and kernel-io-freeze tests fail when
vCPUs are oversubscribed"*.

---

## 1. What actually happens

Reproduction (~6 min, reproduces reliably):

```sh
just _build-run-tests
for j in $(seq 1 24); do ( while :; do :; done ) & done
taskset -c 0-3 builddir/run_tests --raw --no-color
```

Uncontended: 2969/2969 in 55 s. Contended: 33 failures and a wall-timeout.

The 33 failures are **not 33 flakes**. They are one event plus deterministic
fallout. From `builddir/ts1.log`:

```
$ grep -o "AP pause failed: Timeout { cpu_id: [0-9]* }" ts1.log | sort | uniq -c
     32 AP pause failed: Timeout { cpu_id: 1 }
$ grep -c "System halted" ts1.log
      1
```

One CPU, one halt, 32 identical follow-on panics. The chain, each link
verified in-tree:

1. **The host deschedules vCPU 1.** vCPU 0's thread keeps running, so the
   watcher accumulates 100 → 200 → 400 → 800 unchanged samples of CPU 1's
   heartbeat (`slopos-ostd/src/watchdog.rs:accumulate`). At 800 ≥
   `miss_threshold * FATAL_MULTIPLE` it arms `NmiDisposition::Fatal`
   (`watchdog.rs:report_stalled_cpu`) and NMIs CPU 1.

2. **CPU 1 kills itself.** `boot/src/idt.rs:269` dispatches to `nmi_die`
   (`idt.rs:332`), which ends in `panic_abort_raw`
   (`idt.rs:370` → `boot/src/panic.rs:74`) → `cpu::disable_interrupts()` +
   `halt_loop()`. CPU 1 halts forever, *inside the NMI handler*, and it was
   mid-task — the log shows `rip=… <slopos_mm::tlb::service_local_shootdown_queue+0x87>`.
   It therefore halted with `PriorityRunQueue::executing_task == true`.

3. **The BSP is not halted.** `ts1.log` shows the abort at 214.068 s and the
   BSP still emitting quota summaries at 230.4 s. `panic_abort_raw` halts only
   the calling CPU.

4. **`executing_task` is now unclearable.** `sched/src/per_cpu.rs:1160`
   `executing_ap()` polls `is_executing_task(cpu 1)`. That `AtomicBool` is
   cleared only by the owning CPU, at `sched/src/scheduler.rs:985`, `:996`,
   `:1025` and `:1129`. CPU 1 will never execute again, so the flag is stuck
   `true` for the rest of the boot and **every** later `pause_all_aps()` burns
   its budget and returns `Timeout { cpu_id: 1 }`. Deterministic, not flaky.
   `sched/src/test_fixture.rs:96` turns each one into a panic, so all 32
   remaining `KernelTestScope` users die identically.

5. **Second-order:** `nmi_die` also calls `poison_all_held_locks_no_halt()`
   (`idt.rs:365` → `slopos-ostd/src/sync/panic_recovery.rs:27`), whose first
   act is `lock_tracking::enter_fatal_bypass()` — a **global** latch. That is
   why `zz_lockdep_tests::lockdep_ab_ba_is_detected` then fails with
   `validator is not alive (… bypass=true …)`, and why the run's final
   `LOCKDEP[post-kernel-tests]` line reads `DISABLED (fatal bypass latched)`.

6. **The harness cannot tell.** After `System halted.` the run sat idle 134 s
   until the 900 s wall timeout killed QEMU: `Unexpected QEMU exit status 0`.

### The invariant bug, independent of virtualisation

Step 4 is not a virtualisation problem. **A CPU can die with
`executing_task == true`, and nothing ever reconciles that.** On real hardware
a genuinely wedged CPU that takes the fatal NMI produces exactly the same
permanently-broken AP pause. Steal time is only what *provokes* it here.

This ranking drives the staging below: stage 1 is what stops one stolen vCPU
from failing 33 tests, and it is a correctness fix that would be worth making
even if steal time did not exist.

---

## 2. Can we consume a paravirtualised steal-time signal?

**Available, and deliberately not used in stage 1–4.**

What exists today. The kernel has full CPUID plumbing
(`slopos-ostd/src/arch/x86_64/cpuid.rs`) and full MSR plumbing
(`slopos-ostd/src/arch/x86_64/msr.rs`, `read_msr`/`write_msr` with a host
mock). It touches **no** KVM leaf: `cpuid.rs` defines leaves `0x01`, `0x07`,
`0x0D`, `0x8000_0001+`, and `kernel-services/src/clock.rs` reads `0x16`.
Nothing reads `0x4000_0000`/`0x4000_0001`, and the only occurrences of "kvm"
in the tree are a test name and a comment.

What the host offers. Verified on this machine, `-cpu host` under KVM:

```
$ … query-cpu-model-expansion … | grep -i steal
 "kvm-steal-time": true
```

CI is `blacksmith-4vcpu-ubuntu-2404` with a hard `/dev/kvm` precondition
(`.github/workflows/ci.yml:59`) and `scripts/qemu_run.sh:39` selects
`-cpu host`, so `KVM_FEATURE_STEAL_TIME` (CPUID `0x4000_0001`:EAX bit 5) would
be advertised there too.

Cost of consuming it. `MSR_KVM_STEAL_TIME` (`0x4b56_4d03`) takes a *guest
physical address* with bit 0 set, and the hypervisor writes a 64-byte
`kvm_steal_time` record there. That means: a per-CPU physical page allocated
and pinned; a `wrmsr` per CPU, re-issued on every AP bringup
(`boot/src/smp.rs:40`) and after any future CPU-hotplug; an HHDM read path;
and a graceful-degradation path for TCG, for `-cpu max`, and for real hardware.
That is a meaningful new `unsafe` surface in `slopos-ostd` and a new pinned
allocation, in service of one diagnostic.

**Decision: do not build it for stages 1–4.** Stage 2 gets the same
*decision* from one CPUID bit at zero cost. Steal time is recorded in §8 as
the optional upgrade that would turn stage 2's blunt "never escalate under a
hypervisor" into a precise "escalate under a hypervisor only when steal time
proves the target actually got CPU". If it is ever built, its best consumer is
**not** the watchdog but stage 3's AP-pause classifier, where "the target got
0 ns of CPU during this window" is a direct answer to the exact question.

### What in-tree signals actually distinguish "not scheduled" from "wedged"?

The brief asks for these to be evaluated against the code. They were.

| Candidate | Verdict |
|---|---|
| **HPET** (`slopos_kernel_services::clock::monotonic_ns`) | Steal-*immune* as a clock — it is host wall time and keeps advancing regardless. That makes it the right thing to bound a *wait* with (stage 3). It says nothing about whether the target ran. |
| **TSC delta, self-reported** | **Useless here.** The TSC keeps counting while a vCPU is descheduled — KVM does not stop it — so a large TSC delta is equally consistent with "wedged for 4 s" and "descheduled for 4 s". Discard. |
| **Retired-instruction count** (PMU fixed counter 0, `INST_RETIRED.ANY`) | **Genuinely discriminating**: a descheduled vCPU retires zero, a vCPU spinning with `IF=0` retires billions — which is precisely the case the watchdog most wants to catch. But it needs per-CPU PMU MSR setup, a vPMU the host may not expose, and the same degradation matrix as steal time. Named, not built. |
| **A self-reported "I am at a poll point" counter** | The kernel **already has one**: `ProcessorControlRegion::heartbeat`, bumped from the timer tick (`watchdog.rs:tick`). The signal is not the problem. |

That last row is the crux, and the brief asks for it to be argued explicitly:

> **A descheduled CPU and a wedged CPU are indistinguishable from the watcher
> side without a host signal.** Both stop bumping the heartbeat. Both stop
> acking. Both look identical to every predicate the guest can evaluate.

So perfecting the detection is not available at acceptable cost. The plan
therefore makes the **consequence proportionate** instead: report rather than
abort (stage 2), bound the wait on wall time and retry rather than panic on
first failure (stage 3), and — above all — make one dead CPU cost one test
rather than thirty-three (stage 1).

The heartbeat does buy one useful *partial* discriminator, used in stage 3 as
a timeout classifier rather than as a gate: if the target's heartbeat advanced
during the wait, the target is running and is refusing to park (a real bug);
if it did not advance, the target is either stolen or wedged (report both, blame
neither). That is honest about what it can and cannot tell.

---

## 3. Stage 1 — a dead CPU must not hold the AP pause hostage

**Highest value. Do this first and alone.** It converts an unbounded permanent
cascade into at most one failed test, and it is a correctness fix on real
hardware too.

Two halves: a dying CPU must *publish* that it is going down, and the AP-pause
predicate must *honour* that.

### 1a. Publish "this CPU is going down"

The requirement from the NMI handler is severe: interrupts off, inside a
non-returning NMI, no lock, no allocation, one plain atomic store. The kernel
already has exactly that primitive and already uses it on the sibling path.

`slopos_arch::pcr::mark_cpu_offline(cpu_id)`
(`slopos-ostd/src/cpu/x86_64/pcr.rs:1267`) is a single
`AtomicBool::store(false, Release)` into the target's own PCR. `boot/src/idt.rs:438`
already calls it from the `SHUTDOWN_VECTOR` handler, with a comment that states
the exact rationale:

> This CPU never runs again, so it must leave the sets that assume an answer:
> the TLB ladder would wait on its ack, the lockup detector on a tick that
> never comes.

The fatal-NMI path never got the same treatment. Add it, in three places.

**`boot/src/idt.rs`, `nmi_die()` (~line 332).** Before the backtrace walk, at
the very top of the function — the walk can fault, and publishing must not
depend on surviving it:

```rust
fn nmi_die(cpu_id: usize, frame: &slopos_arch::InterruptFrame) -> ! {
    // This CPU halts below and never clears any flag again; every set that
    // waits on an answer from it must be told before the reporting begins.
    slopos_mm::tlb::notify_cpu_offline();
    slopos_arch::pcr::mark_cpu_offline(cpu_id);
    slopos_sched::per_cpu::abandon_local_dispatch(cpu_id);
    …
```

**`boot/src/idt.rs`, `nmi_handler()`'s peer-stop branch (~line 253).** It
already calls `force_ack_local_shootdowns` for the TLB set; extend it to the
same three:

```rust
    slopos_mm::tlb::force_ack_local_shootdowns(cpu_id);
    slopos_mm::tlb::notify_cpu_offline();
    slopos_arch::pcr::mark_cpu_offline(cpu_id);
    slopos_sched::per_cpu::abandon_local_dispatch(cpu_id);
    slopos_ostd::sync::panic_recovery::poison_all_held_locks_no_halt();
```

**`boot/src/panic.rs`, `panic_abort_raw()` (~line 74).** The generic
last-resort abort ends in `halt_loop()`, so the same reasoning applies:

```rust
pub fn panic_abort_raw(msg: &'static str) -> ! {
    slopos_ostd::fblog::snapshot_tail_for_panic();
    cpu::disable_interrupts();
    slopos_arch::pcr::mark_cpu_offline(slopos_arch::get_current_cpu());
    slopos_ostd::sync::enter_fatal_bypass();
    …
```

`abandon_local_dispatch` is the new, deliberately tiny surface in
`sched/src/per_cpu.rs`. It is one store — the direct reconciliation of the
stuck flag, so the fix does not rest solely on the online bit:

```rust
/// Release the dispatch flag on behalf of a CPU that is halting and will never
/// clear it itself. One store: the only legal caller is that CPU's own
/// non-returning abort path, with interrupts off.
pub fn abandon_local_dispatch(cpu_id: usize) {
    let _ = with_cpu_scheduler(cpu_id, |sched| sched.set_executing_task(false));
}
```

**Ordering note that matters:** `mark_cpu_offline` must precede
`enter_fatal_bypass` and the poison walk in every one of these, because the
poison walk takes the slow path and can be long; a peer polling in between
should already see the CPU as gone.

**Uncertainty, flagged:** `panic_abort_raw` is also reached on the ordinary
whole-machine fatal path, where the caller is often the BSP and the machine is
going down anyway. Marking the BSP offline there is harmless (nothing later
consults `online` on a machine in `halt_loop`), but it is a behaviour change on
a path that is hard to test. If review prefers, restrict the `panic.rs` change
to the case where at least one peer is still running
(`slopos_ostd::panic::stopped_cpu_count() < get_pcr_count() - 1`); I judge the
unconditional form simpler and equally safe, but it is the one call here I would
want a second opinion on.

### 1b. Honour it in the AP-pause predicate

`sched/src/per_cpu.rs:1160`:

```rust
fn executing_ap(cpu_count: usize) -> Option<usize> {
    (1..cpu_count).find(|&cpu_id| {
        // An offline AP runs no task, and only that AP could ever clear its own
        // dispatch flag — waiting on one is a wait that cannot end.
        slopos_arch::pcr::is_cpu_online(cpu_id)
            && with_cpu_scheduler(cpu_id, |sched| sched.is_executing_task()) == Some(true)
    })
}
```

`is_cpu_online` is already used throughout this file (`:925`, `:930`, `:1044`),
so this adds no import and no dependency.

Note this is **belt and braces** with `abandon_local_dispatch`: either alone
fixes the observed cascade. Both are worth having, because they fail
independently — 1a's store can be missed by a path nobody added it to, and 1b's
`online` bit can lag if a CPU dies somewhere with no offline publication at all.

### 1c. Report, do not silently absorb

`pause_all_aps` should say when it stepped over a dead AP, once, not per call.
Add to `sched/src/per_cpu.rs` a `static SKIPPED_OFFLINE_AP: StateFlag` and log
on first observation only, at the point `wait_for_aps_to_park` first finds an
online-but-executing set that excludes an offline-and-executing CPU. Cheap, and
it means the log names the real event instead of leaving it inferable only from
a missing timeout.

### 1d. The lockdep bypass latch

`enter_fatal_bypass()` is global, and one dying CPU latches it for the
survivors. That is arguably *correct* — the poison walk force-released locks, so
the graph genuinely is no longer trustworthy — so the fix is not to un-latch it.
The fix is that `ktesting/src/zz_lockdep_tests.rs:27` should not report `Fail`
for a bypass it can attribute to a fatal abort.

The attribution needs an **exact** signal. `oops_count() > 0` is **not** one and
must not be used: `ts1.log` records `oops 1..3` at 13 s, during ordinary passing
tests, long before anything fatal — a recovered task-scoped panic bumps it too.
Gating on it would skip this test in green runs and silently retire the
tripwire.

Stage 1a already touches every abort path, so put a dedicated one-line flag
there. In `slopos-ostd/src/panic.rs`, beside the existing `STOPPED_CPUS`:

```rust
/// Set by any path that halts a CPU for good. Distinct from the oops counter,
/// which a *recovered* panic also bumps.
static FATAL_ABORTS: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn mark_fatal_abort() {
    FATAL_ABORTS.fetch_add(1, Ordering::SeqCst);
}

#[inline]
pub fn fatal_abort_observed() -> bool {
    FATAL_ABORTS.load(Ordering::Acquire) != 0
}
```

Called from the same three sites as `mark_cpu_offline` in §3.1a. Then:

```rust
    if lock_graph::fatal_bypassed() && slopos_ostd::panic::fatal_abort_observed() {
        // A fatal abort force-released locks; the graph is untrustworthy by
        // design and the cycle detector cannot fire. Nothing here can be proved.
        return TestResult::Skipped;
    }
    assert_test!(lock_graph::validator_alive(), …);
```

The tripwire is preserved exactly: a bypass latched with **no** fatal abort
still fails, which is the case the assertion was written for.

### Stage 1 tests

- `sched/src/sched_tests.rs::test_ap_pause_ignores_an_offline_ap` — mark CPU 1
  offline, `set_executing_task(true)`, assert `pause_all_aps()` returns `Ok` and
  leaves depth 0 after release. The mirror image of the existing
  `test_ap_pause_timeout_is_reported_and_rolled_back` (`:6235`), which must keep
  timing out because its held CPU is genuinely **online**. Restore state via the
  existing `PerCpuOnlineBits` hermetic entry (`sched/src/test_hermetic.rs:21`),
  which already snapshots and restores the online bitmap.
- `sched/src/sched_tests.rs::test_abandon_local_dispatch_clears_the_flag` —
  set the flag, call `abandon_local_dispatch`, assert cleared. Trivial, but it
  is the one store the whole cascade hinges on.

### Stage 1 gate impact

- `check_test_count.sh`: +2 tests → baseline rises. Re-measure with
  `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`, never guess.
- `check_lockdep_headroom.sh`: **no new lock, no new acquire order.** Expect no
  movement; if it moves, that is a finding, not a number to raise.
- `check_unsafe_outside_ostd.sh`: all of 1a/1b/1c is safe code in `boot`/`sched`
  calling existing safe OSTD surfaces. Must stay clean.
- `check_stack_sizes.sh`: `nmi_die` gains three calls and no locals. Neutral.

---

## 4. Stage 2 — the watchdog must not abort a healthy kernel

`slopos-ostd/src/watchdog.rs` opens with a premise that is **false**:

> A host that stalls the target stalls the watcher identically.

Under *partial* oversubscription it is exactly wrong: the host descheduled
vCPU 1 while vCPU 0 kept running, which is how the watcher reached 800 samples.
The samples-not-wall-clock design was chosen to be steal-immune and is not.

Per §2 the detection cannot be made correct without a host signal. So fix the
consequence.

### The decision

Escalation from `Report` to `Fatal` earns its keep in exactly one scenario: on
a machine with no supervising harness, a truly wedged CPU should abort loudly
rather than hang silently forever. Under a hypervisor, three things are true at
once: the guest cannot distinguish steal from wedge; a harness with a wall
timeout and a silence watchdog is already supervising; and the escalation's
observed effect was to abort a healthy kernel. The trade is not close.

**Default `Fatal` escalation off when the kernel can see it is virtualised;
keep the existing cmdline knob authoritative in both directions.**

`slopos-ostd/src/arch/x86_64/cpuid.rs`:

```rust
/// CPUID.1:ECX[31] is architecturally reserved-zero on physical CPUs; every
/// mainstream hypervisor sets it to advertise its presence.
pub const CPUID_FEAT_ECX_HYPERVISOR: u32 = 1 << 31;

pub fn hypervisor_present() -> bool {
    cpuid(CPUID_LEAF_FEATURES).2 & CPUID_FEAT_ECX_HYPERVISOR != 0
}
```

One CPUID read, no MSR, no allocation, works under KVM and TCG alike, and reads
`false` on bare metal. (Interface fact from the Intel SDM's reserved-bit
definition plus the hypervisor-present convention — not derived from any
implementation.)

`slopos-ostd/src/watchdog.rs`:

```rust
/// Escalation is a *decision*, so it is one testable function rather than an
/// expression inside the report path.
///
/// A watcher cannot distinguish a target the host descheduled from one that is
/// wedged: both stop bumping their heartbeat. Under a hypervisor the sustained
/// breach is therefore not evidence, and taking the machine down on it aborted
/// a healthy kernel. `watchdog.panic=` overrides in both directions.
pub fn fatal_escalation_permitted() -> bool {
    match PANIC_OVERRIDE.load(Ordering::Acquire) {
        OVERRIDE_ON => true,
        OVERRIDE_OFF => false,
        _ => !crate::arch::x86_64::cpuid::hypervisor_present(),
    }
}
```

`PANIC_ENABLED: AtomicBool` becomes a three-state `PANIC_OVERRIDE: AtomicU32`
(`Unset`/`On`/`Off`) so that `watchdog.panic=on` can force escalation *back* on
under a hypervisor — which is what the stage-2 test needs in order to prove the
escalation still works at all. `boot/src/early_init.rs:637-641` already parses
both spellings; the two arms just store the new sentinels.

`report_stalled_cpu` then reads:

```rust
    let disposition = if fatal_escalation_permitted() && stale >= fatal_at {
        NmiDisposition::Fatal
    } else {
        NmiDisposition::Report
    };
```

### What a non-fatal-but-still-useful report looks like

`Report` already does the right things and they are kept: it names the target,
the sample count and the watcher; it dumps the wait-for chain
(`dump_wait_chain`); it NMIs the target so the target dumps its own registers,
symbolised through `ksym::lookup`; and it doubles `next_report` so a long
legitimate section logs a handful of lines rather than one per tick. Two
additions:

1. **State the classification.** When escalation was permitted and the threshold
   was crossed, emit `WATCHDOG:   fatal escalation suppressed (hypervisor)` once
   per target. A suppressed abort must be visible, or the next reader concludes
   the detector is broken.
2. **Keep the evidence.** `max_stall` / `report_max_stalls` already survive to
   the end-of-phase summary. A run whose CPU stalled for 800 samples and did
   *not* die still reports the 800 there, so the ratchet-style evidence is not
   lost by declining to abort.

### A bonus this fixes

`boot/src/tests/watchdog_tests.rs:212 test_a_wedged_cpu_is_reported_and_survives`
is on a knife-edge today, and it is one of the failures in `c1-1.log`. It sets
`miss_threshold(3)`, so `fatal_at = 3 * FATAL_MULTIPLE = 15`, then stalls with
`IF=0` for 150 ms. Reports fire at `stale` = 3, 6, 12, 24 (`next_report`
doubles), so the run survives only because the stall ends after ~15 samples and
before the report at 24 — which *would* be `>= 15` and therefore **fatal**. The
margin is about 120 ms. With stage 2 the test can no longer take the machine
down under QEMU at all.

Its *observed* failure is the other direction — `"a CPU that stopped ticking for
150 ms was never reported"`, i.e. the **watcher's** vCPU was also descheduled
and never sampled. That needs its own steal-immune guard, and the heartbeat
supplies one because it is the watcher's own self-report:

```rust
    let watcher = watchdog::watcher_of(cpu)…;
    let watcher_beats_before = pcr::heartbeat_for_cpu(watcher);
    …stall…
    if pcr::heartbeat_for_cpu(watcher).wrapping_sub(watcher_beats_before) < 3 {
        // The watcher took fewer ticks than the threshold needs, so it never
        // sampled us; the run proves nothing either way.
        return TestResult::Skipped;
    }
```

`test_lapic_timer_recalibration_consistent`
(`drivers/src/tests/apic_timer_tests.rs:308`, the other `c1-1.log` failure) is
the same shape: a LAPIC-vs-HPET ratio is meaningless when the host steals the
vCPU mid-window. Fix it by taking the **best of N** calibration windows rather
than by widening the tolerance — a stolen window is a spoiled measurement, and
the assertion should still be tight on a clean one.

### Stage 2 tests

- `slopos-ostd` host test (`cargo test -p slopos-ostd`) on
  `fatal_escalation_permitted`'s override precedence — pure decision function,
  no QEMU. Drive the three override states directly.
- `boot/src/tests/watchdog_tests.rs::test_fatal_escalation_defaults_off_under_a_hypervisor`
  — under QEMU `hypervisor_present()` is always true, so assert
  `!fatal_escalation_permitted()` with the override unset, and assert it flips
  with the override forced on. `Skipped` when `!hypervisor_present()`.
- Amend `test_a_wedged_cpu_is_reported_and_survives` with the watcher-heartbeat
  guard above.

### Stage 2 gate impact

- `check_safe_contract_surface.sh`: baseline **0**. `hypervisor_present` and
  `fatal_escalation_permitted` are pure reads with no caller obligation, so
  neither may carry a `# Safety` section. Must stay 0.
- `check_unsafe_expansion.sh`: `cpuid()` is already an allowlisted `unsafe`
  site inside OSTD; the new constant and wrapper add no `unsafe`.
- `check_test_count.sh`: +2 → re-measure.

---

## 5. Stage 3 — bound the AP pause on wall time, and classify its failures

`sched/src/per_cpu.rs:1102`:

```rust
const AP_PAUSE_SPIN_BUDGET: u32 = 100_000;
```

The budget is measured in the **waiter's** retired instructions, which have no
relation to whether the target got any CPU at all. A descheduled vCPU burns the
entire budget having executed nothing.

### 3a. Wall-clock deadline

`sched` already depends on `slopos-kernel-services` and `sched/src/sleep.rs:23`
already calls `slopos_kernel_services::clock::monotonic_ns()`, so this adds no
dependency and no lockdep class.

```rust
/// Wall-clock budget for the pause wait. Measured, not chosen: see §5e.
const AP_PAUSE_BUDGET_NS: u64 = /* measured; see §5c */;

/// Retained only for the pre-clock window: `monotonic_ns` returns 0 until the
/// platform clock is wired, and `task_shutdown` can reach here before that.
const AP_PAUSE_SPIN_BUDGET: u32 = 100_000;

fn wait_for_aps_to_park(cpu_count: usize) -> Result<(), ApPauseError> {
    let Some(blamed) = executing_ap(cpu_count) else {
        return Ok(());
    };
    let blamed_beat_before = slopos_arch::pcr::heartbeat_for_cpu(blamed);

    nudge_aps_to_poll_point(cpu_count);

    let start_ns = slopos_kernel_services::clock::monotonic_ns();
    let mut iteration: u32 = 0;
    loop {
        if executing_ap(cpu_count).is_none() {
            return Ok(());
        }
        iteration = iteration.wrapping_add(1);
        if iteration % AP_PAUSE_NUDGE_INTERVAL == 0 {
            nudge_aps_to_poll_point(cpu_count);
        }
        if pause_deadline_passed(start_ns, iteration) {
            break;
        }
        core::hint::spin_loop();
    }

    match executing_ap(cpu_count) {
        Some(cpu_id) => Err(classify_pause_failure(cpu_id, blamed_beat_before)),
        None => Ok(()),
    }
}

/// Wall time when the clock is up; the iteration budget only before it is.
fn pause_deadline_passed(start_ns: u64, iteration: u32) -> bool {
    if start_ns == 0 {
        return iteration >= AP_PAUSE_SPIN_BUDGET;
    }
    let now = slopos_kernel_services::clock::monotonic_ns();
    now != 0 && now.wrapping_sub(start_ns) >= AP_PAUSE_BUDGET_NS
}
```

`pause_deadline_passed` is factored out as a pure function precisely so it can
be unit-tested against a fake clock without a live HPET.

**Stack-size hazard, called out because the gate will catch it late:** do
**not** snapshot heartbeats for all CPUs into a stack array. `MAX_CPUS` is 256,
so `[u64; MAX_CPUS]` is exactly 2048 bytes — `check_stack_sizes.sh`'s entire
2 KiB cap, before a single other local. Record one `u64` for the CPU first
observed executing, as above.

### 3b. Classify the failure: stolen/dead vs. genuinely refusing to park

```rust
pub enum ApPauseError {
    /// Still executing, and its heartbeat advanced during the wait: the AP is
    /// running and did not reach its poll point. A real scheduler bug.
    NotParking { cpu_id: usize },
    /// Still executing, and its heartbeat never moved: the AP took no timer
    /// interrupt in the whole window. Descheduled by the host, or wedged —
    /// the guest cannot tell which.
    NotRunning { cpu_id: usize },
}
```

```rust
fn classify_pause_failure(cpu_id: usize, beat_before: u64) -> ApPauseError {
    if slopos_arch::pcr::heartbeat_for_cpu(cpu_id) != beat_before {
        ApPauseError::NotParking { cpu_id }
    } else {
        ApPauseError::NotRunning { cpu_id }
    }
}
```

This is the honest half-answer from §2, and it is genuinely useful: `NotParking`
is always a bug worth chasing, `NotRunning` never is on its own.

### 3c. Does the AP need to acknowledge? Yes — as evidence, not as the gate

Today the BSP *infers* parking from `is_executing_task()`, and stage 1 exists
because that flag can be stale. A handshake cannot be stale: an ack requires the
AP to have executed code *after* the request.

`PriorityRunQueue` gains one field (an atomic, not a lock — no lockdep class):

```rust
    /// Generation of the AP pause this CPU last acknowledged from its poll
    /// point. Only this CPU writes it, so no dead CPU's value can be mistaken
    /// for a fresh ack.
    pause_ack: AtomicU32,
```

`sched/src/runtime.rs`, in `scheduler_loop`'s pause branch (~line 428), the AP
acks immediately before it parks:

```rust
        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            per_cpu::ack_ap_pause(cpu_id);
            slopos_ostd::sync::rcu_note_qs();
            …
```

`pause_all_aps` bumps a global `AP_PAUSE_GENERATION` before the SeqCst fence it
already issues, and the wait's success condition becomes: for every AP that is
online, *either* it has acked the current generation *or* it is not executing.
An offline AP is excluded, exactly as in stage 1b. The existing
`nudge_aps_to_poll_point` already delivers the reschedule IPI that wakes an AP
parked in `sti_hlt_cli_atomic`, so an idle AP can ack too.

**Why ack-as-evidence and not ack-as-gate.** Requiring an ack unconditionally
would mean an AP that is legitimately idle-parked must be woken to answer,
turning every pause into an IPI round trip on the test suite's hottest fixture
path. Keeping `!executing` as the fast success condition and the ack as the
corroborating signal keeps the common case at its current cost while making
`NotParking` vs `NotRunning` provable rather than inferred.

### 3d. Proportionate consequence at the call site

`sched/src/test_fixture.rs:91` panics on first failure. Retry instead — the AP
may simply not have been scheduled yet, and the pause depth is already rolled
back on failure (`per_cpu.rs:1148`), so a retry is clean:

```rust
        const PAUSE_ATTEMPTS: usize = 3;
        let mut last_err = None;
        let aps_paused = loop { … };  // 3 attempts, fresh deadline each
```

and on final failure panic with the classified error, so the log says *why*:
`KernelTestScope: AP pause failed: NotRunning { cpu_id: 1 } (3 attempts)`.

**Should it degrade rather than panic?** No, and this is deliberate. The scope's
entire contract is that APs cannot race the test body; running the body anyway
would report a result from a run it did not control, which is worse than a
failure. Panicking **one** test is correct. The defect was that it panicked
thirty-two, and that is stage 1's job, not this one's.

`sched/src/task/task_lifecycle.rs:1339` (`task_shutdown`) already steps over a
failed pause deliberately and correctly; it only needs its `match` arm widened
to the two new variants.

### 3e. How to measure `AP_PAUSE_BUDGET_NS`

**Do not guess it.** Follow the tree's measured-and-tracked style:

1. Add `static AP_PAUSE_MAX_NS: AtomicU64` in `sched/src/per_cpu.rs`, updated
   with a relaxed compare-and-max at the end of every successful
   `wait_for_aps_to_park`.
2. Emit it from the existing end-of-phase summary point, next to the
   `LOCKDEP[...]` and `QUOTA[...]` lines that `boot/src/boot_drivers.rs:367-368`
   already produce:
   `APPAUSE[post-kernel-tests]: max_park_ns=… attempts=… retries=…`
3. Run `builddir/run_tests --raw --no-color` **five times uncontended**, take
   the maximum across runs.
4. Set `AP_PAUSE_BUDGET_NS` to ~50x that maximum, and record the measurement
   date, the run count and the observed value in a comment above the constant.
5. Re-run the contended reproduction and confirm the budget is *still* exceeded
   there — a budget that never fires under 24-way contention is a budget that
   has stopped being a bound.

Expectation, stated so a wildly different measurement is treated as a finding:
parking is a poll-point round trip, so single-digit milliseconds; a budget in
the low hundreds of milliseconds should follow. **If the measured maximum is
already tens of milliseconds, stop and investigate — that is a second bug.**

### Stage 3 tests

- `test_pause_deadline_passed_uses_wall_clock_when_available` — pure-function
  test of `pause_deadline_passed` over (start=0, start=live) × (before, after).
- `test_ap_pause_failure_names_a_running_ap_as_not_parking` — the existing
  `test_ap_pause_timeout_is_reported_and_rolled_back` (`:6235`) already holds an
  **online** AP's flag while that AP keeps ticking, so it should now observe
  `NotParking`; update its assertion and keep its rollback check.
- `test_ap_pause_acks_from_the_poll_point` — assert the ack generation advances
  for every online AP across a successful pause.

### Stage 3 gate impact

- `check_lockdep_headroom.sh`: `pause_ack` is an `AtomicU32` inside an existing
  struct — **no new lock class, no new edge.** Verify; do not pre-emptively
  raise anything.
- `check_stack_sizes.sh`: see the `MAX_CPUS` hazard above. Runs on every build.
- `check_wait_result_handling.sh` / `check_wait_predicate_purity.sh`: the new
  `ApPauseError` arms must be matched out explicitly, never `let _ =`'d.
- `check_test_count.sh`: +3 → re-measure.

---

## 6. Stage 4 — the harness must recognise a halted kernel

After `System halted.` the run burned 134 s (potentially 670 s) before exiting.
Harness-side, Go, no kernel risk, and it turns a 6-minute diagnosis into ~30 s.

**The trap to avoid:** do *not* exit on seeing the abort. In `ts1.log` the abort
was **CPU 1 only** and the BSP went on to produce 2931 further results. Killing
at 214 s would have discarded most of the run's evidence. The kernel's abort
banner means "some CPU is gone", not "the machine is gone".

What the kernel prints (`boot/src/panic.rs:82-84`, `slopos-ostd/src/panic.rs:96-98`):

```
=== KERNEL ABORT ===
NMI watchdog: CPU made no progress, sustained
System halted.
```

These arrive as ordinary non-KTAP lines. `tools/run_tests/parser.go:138`
already funnels them into `appendKlogTail`, so the harness *sees* them and acts
on none of them.

**`tools/run_tests/parser.go`** — recognise the banner, capture the reason line
that follows it, emit a new `EvKernelAbort{Reason string}`:

```go
const kernelAbortBanner = "=== KERNEL ABORT ==="
// … in Feed's non-KTAP branch, before appendKlogTail:
if strings.Contains(line, kernelAbortBanner) {
    p.abortPending = true
} else if p.abortPending {
    p.abortPending = false
    events = append(events, &EvKernelAbort{Reason: strings.TrimSpace(line)})
}
```

**`tools/run_tests/recorder.go`** — `RunSummary` gains
`KernelAbort bool` and `KernelAbortReason string`.

**`tools/run_tests/driver.go`** — a `TightenSilence(d time.Duration)` method
that lowers the live silence limit. Two details in the existing goroutine make
this less trivial than it looks, and both must be handled:

- `limit` is a **captured local** (`driver.go:102`), so it has to become an
  `atomic.Int64` on the driver that the loop re-loads *per tick*.
- `interval` is **also** a captured local, fixed at `SilenceSec/4` — 30 s at the
  120 s default (`cmdline.go:106`). Lowering only `limit` to 20 s would still be
  detected no sooner than the next 30 s tick, i.e. the tightening would do
  nothing. Recompute the ticker when the limit drops (`t.Reset(newLimit/4)`), or
  simply fix the ticker at 1 s — it is one wakeup per second on a run that lasts
  minutes.
- If `--silence-secs=0` the goroutine never starts at all, so `TightenSilence`
  must be a documented no-op there rather than a silent one.

`main.go`'s `onLine` calls it on `EvKernelAbort`. Default
`PostAbortSilenceSec = 20`: long enough for a surviving BSP to finish a phase
and emit its summaries, short enough that a fully-dead machine costs ~20 s
instead of 670.

**`tools/run_tests/verdict.go`** — `ClassifyRun` reports the abort. It is
already `Code: 1` via `failedOverall` when tests failed; the value added is the
diagnostic, so the operator reads the cause instead of
`Unexpected QEMU exit status 0`:

```go
    if s.KernelAbort {
        v.Diagnostic = fmt.Sprintf(
            "run_tests: the kernel aborted on some CPU during this run: %s\n"+
                "  Results after that point come from the surviving CPUs and may cascade.",
            s.KernelAbortReason)
    }
```

An abort with **zero** test failures must not be green: add `s.KernelAbort` to
the `failedOverall` disjunction.

### Stage 4 tests

Host-side only, via `just check-tests-host` (`go test ./tools/run_tests/...`),
no QEMU:

- `parser_test.go::TestKernelAbortBannerEmitsEvent` — banner + reason line
  produce one `EvKernelAbort` with the reason; a bare banner with no follow-up
  does not panic the parser.
- `parser_test.go::TestKernelAbortDoesNotDisturbKtapParsing` — a KTAP result
  line immediately after the banner still parses (this is the `ts1.log` case).
- `verdict_test.go::TestKernelAbortIsNotGreen` — abort + zero failures ⇒
  non-zero exit and a non-empty diagnostic.

### Stage 4 gate impact

None. No kernel change, no ratchet.

---

## 7. Stage 5 — the per-test time inflation is *not* the lost-wake bug

The brief asks whether `drivers/src/tty/poll.rs:149`'s in-tree TODO —

> a wake landing between enqueue and the block CAS is lost, so this waits out
> the full 100 ms — fix is one `wait_event_timeout` queue, not N

— is what turns a 14 ms test into a 78 s one. **It is not, and the evidence is
conclusive.**

1. `poll_sleep_on` is reached only from the syscall adapters
   (`drivers/src/syscall_services_init.rs:127`, `:171-172`) and from its own
   slot-less sibling `poll_sleep` (`poll.rs:185`). Its single occurrence in
   `tty_tests` is `test_ldisc_regression.rs:2461`, which is a re-export
   *signature* check (`let _: fn(&[u8]) = tty::poll_sleep_on;`) and never calls
   it. **No `tty_tests` test invokes it.**
2. The worst inflation is
   `test_literal_next_vintr_does_not_bypass_throttle` (14 → 78 559 ms). It calls
   `tty::write(master, b"\x03", true)` — `nonblock: true` — and
   `wait_for_write_ready` (`drivers/src/tty/io.rs:593`) takes the non-blocking
   branch, which returns `Err(WouldBlock)` **without ever reaching**
   `wait_event_interruptible`. The test cannot block.
3. Arithmetically: the lost wake costs at most 100 ms per occurrence. Reaching
   78 s would need ~785 of them in a test that provably never blocks once.

The actual cause is **lock-holder preemption**, the classic oversubscribed-VM
pathology. `throttled_priority_setup` pushes `THROTTLE_HIGH_WATER + 1` = 6145
bytes through `tty::push_input`, each taking the `TTY_SLOTS[slot]` spinlock
(`io.rs:93`) and running `deferred.execute()`. When the host deschedules a vCPU
holding that spinlock, every other vCPU spins for the whole deschedule quantum.
The signature fits: it explains why *many* tty tests inflated (they all hammer
`TTY_SLOTS`), why the inflation is not a multiple of 100 ms, and why
`test_canonical_input_over_1024` (2001 pushes, 90 → 15 304 ms) scales roughly
with push count.

**Conclusions.**

- The inflation is a **consequence of the same steal**, not a separate kernel
  bug, and it needs no kernel fix. No test asserts on wall-clock duration; only
  `OVER_TIME` marking (informational) and the harness wall budget care, and
  stage 4 addresses the latter.
- The **lost-wake bug is real and separate**. It is a correctness defect on the
  real `poll(2)` path — a poll that should return immediately instead sleeps up
  to 100 ms — and it deserves its own iteration: replace the N-queue register /
  block / unregister dance with a single `wait_event_timeout` whose predicate
  re-checks readiness after enqueue, closing the window. **Out of scope here.**
- The general LHP mitigation is paravirtual spinlocks
  (`KVM_FEATURE_PV_UNHALT` and friends) — a large feature with the same
  degradation matrix as steal time, and not justified by a test-suite timing
  artefact. Recorded, not planned.

**Uncertainty, flagged:** I did not instrument where the 78 s was actually
spent. The elimination of the lost-wake path is conclusive (it is unreachable
from that test); the positive attribution to lock-holder preemption is inference
from the call shape and the scaling, not measurement. If it matters, the cheap
confirmation is to re-run the reproduction with `QEMU_SMP=1`: LHP requires a
contended lock across vCPUs and would vanish, while a lost wake would not.

---

## 8. Optional follow-up — steal time, if stage 2 proves insufficient

Only if stage 2's blunt suppression turns out to hide a real wedge that
mattered. Build it in `slopos-ostd`, and wire it to the **AP-pause classifier**
first, the watchdog second.

- `cpuid.rs`: `0x4000_0000` (max hypervisor leaf + signature) and
  `0x4000_0001`:EAX bit 5 (`KVM_FEATURE_STEAL_TIME`).
- `msr.rs`: `MSR_KVM_STEAL_TIME = 0xC000_0000 + …` → address `0x4b56_4d03`,
  written as `gpa | 1`.
- Per-CPU 64-byte record, pinned; registered from `run_ap_init`
  (`boot/src/smp.rs:40`) so every AP has one before it can be watched.
- Consumer: `classify_pause_failure` gains a third arm — `Stolen { cpu_id,
  steal_ns }` when the target's steal counter advanced by ~the whole wait
  window. That is a *proof*, not an inference, and it is the only thing that
  would let the fatal escalation be re-enabled under a hypervisor honestly.
- Must degrade to stage 2's behaviour when the leaf is absent (TCG, `-cpu max`,
  bare metal). The absence path is the one to test first, because it is the one
  CI might silently take.

---

## 9. Ordering and acceptance

| Stage | Change | Value | Risk |
|---|---|---|---|
| **1** | Dead CPU publishes offline; AP pause honours it; lockdep test attributes the bypass | **One stolen vCPU costs 1 test, not 33.** Also a real-hardware correctness fix | Low |
| **2** | No fatal escalation under a hypervisor by default | Stops aborting a healthy kernel; fixes two knife-edge tests | Low |
| **3** | AP pause on an HPET deadline; ack handshake; classified failure; bounded retry | Removes the last instruction-count bound; makes failures diagnosable | Medium |
| **4** | Harness recognises the abort banner and tightens its silence budget | 6-minute diagnosis → ~30 s | Low |
| **5** | *No kernel change.* Records the inflation as LHP; splits the lost-wake bug out | Prevents a wrong fix | None |

Land them as separate commits. Stage 1 alone is worth shipping and should not
wait on the rest.

Per-stage acceptance, in addition to the standard pre-commit sequence in
`AGENTS.md`:

- `cargo fmt --all`, `just fmt`, `just test-host`, `just build`,
  `just check-framekernel-gates`.
- One `builddir/run_tests --raw --no-color` capture, fed to
  `check_test_count.sh`, `check_lockdep_headroom.sh`, `check_quota_headroom.sh`
  via `--log`.
- **Plus the reproduction**, which is the only thing that proves the point:

  ```sh
  for j in $(seq 1 24); do ( while :; do :; done ) & done
  taskset -c 0-3 builddir/run_tests --raw --no-color
  ```

  Stage 1 acceptance: `grep -c "AP pause failed" ` must be **≤ 1**, where it is
  currently 32. Stage 2: `grep -c "System halted"` must be **0**. Stage 4: the
  gap between the last kernel line and process exit must be **≤ 25 s**, where it
  is currently 134 s.
- Every ratchet that moves gets a fresh `--emit-allowlist` in the same commit
  and a commit-message line naming what added the delta. Never hand-edit
  `scripts/gates/**`.

## 10. Open questions

1. **`panic_abort_raw` marking the caller offline unconditionally** (§3.1a).
   Correct as far as I can tell, but it is on the whole-machine fatal path where
   the caller is usually the BSP and testing is awkward. A conditional form is
   available if review prefers it.
2. **Whether the ack handshake should ever become the gate** rather than
   corroboration (§3.3c). Keeping `!executing` as the fast path preserves
   current cost; making the ack mandatory is stricter but adds an IPI round trip
   to the test suite's hottest fixture path. I chose cost; it is a judgement
   call and reversible.
3. **`AP_PAUSE_BUDGET_NS`'s value is unmeasured.** The procedure is in §5e and
   must be run before the constant is written. A measured maximum in the tens of
   milliseconds should halt the work and be investigated as a separate defect.
4. **The 78 s attribution to lock-holder preemption is inference, not
   measurement** (§7). The elimination of the lost-wake path is conclusive; the
   positive attribution is not. `QEMU_SMP=1` is the cheap confirmation.
5. **Skipping `lockdep_ab_ba_is_detected` on an attributable bypass** (§3.1d)
   weakens a tripwire in exchange for not reporting known fallout as a fresh
   failure. The dedicated `fatal_abort_observed()` flag keeps an
   *unattributable* bypass failing, which is the property that matters; I moved
   to it after confirming the obvious `oops_count() > 0` spelling would have
   fired in green runs and retired the tripwire silently.
