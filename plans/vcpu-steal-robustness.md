# Surviving a descheduled vCPU

Making the kernel's liveness bounds robust when the *host* stops running a
guest vCPU. Written against `273ed3ba`; rebaselined against `8fec5938`.

Supersedes the "fix is not scheduled work" note in the
`KNOWN_ISSUES.md` entry *"Aging-backstop and kernel-io-freeze tests fail when
vCPUs are oversubscribed"*.

---

## 0. Status — what has landed

**Read this before doing anything from this file.**

The two changes that made the reproduction green are **implemented and
committed**. Take the tree as already containing them.

**There are exactly two remaining work items: §4 (Stage 1 of 2) and §5
(Stage 2 of 2).** Nothing else in this document is a task:

- §1-§3 are history and reasoning. §3 describes code that is **already in the
  tree** — do not re-apply it.
- §6 is a finding that deliberately requires **no code change**.
- §7 is explicitly **not planned**; it has a stated trigger that has not fired.

Neither remaining stage is load-bearing for correctness — the reproduction is
green at 4/4 contended runs. Do not treat them as outstanding breakage.

| Landed | Commit | Effect |
|---|---|---|
| A dying CPU publishes offline, force-acks its shootdowns and releases its dispatch flag; `executing_ap` skips an offline AP; `lockdep_ab_ba_is_detected` attributes a fatal-abort bypass | `80b8ce4e` | One stolen vCPU costs at most one test instead of 33 |
| No fatal watchdog escalation where a stalled heartbeat is not evidence (three-state `watchdog.panic=` override); two tests stop reading host steal as kernel misbehaviour | `317de749`, `0d9e8419` | A healthy kernel is no longer aborted |

Measured against the reproduction in §1:

| | Before | After |
|---|---|---|
| AP-pause failures | 32 | 0 |
| `System halted` | 1 | 0 |
| Test failures | 33 | 0 |
| Kernel phase | `passed=2936 failed=33` | `passed=2972 failed=0` |

`LOCKDEP[post-kernel-tests]` reads `ACTIVE` again rather than
`DISABLED (fatal bypass latched)`. Test-count baseline re-measured 2972 → 3006
(31 of that was pre-existing drift; +3 is this work). No other gate file moved:
the changes add no lock and no acquire order.

**Two corrections the implementation forced, recorded so they are not
re-proposed.** The original §4 guards for the watchdog and LAPIC tests
were the wrong shape and still failed 2-in-3 contended runs. A tick count over
the stall window is satisfied by a descheduled watcher bursting its ticks
*afterwards*, so the test must ask what the watcher actually observed
(`max_stall`). And closest-of-N calibration selects the sample nearest a
baseline that may itself be spoiled — steal can only make a calibration read
*low*, so the correct form is largest-of-N compared only on downward drift.
§3b records both corrections.

What is left is **wall-clock inflation under contention**, which fails no test:
the kernel phase completes green, but tty tests inflate (14 ms → 78 s) and the
userland phase can exhaust the harness budget. That inflation is lock-holder
preemption — a *host* scheduling artefact, not guest lock contention — and §6
explains why it needs no kernel fix.

### One separate bug this work split out, tracked here so it is not lost

The **`poll(2)` lost wake** (`drivers/src/tty/poll.rs:149`, in-tree TODO) is a
genuine correctness defect on the real userland path: a wake landing between
enqueue and the block CAS is lost, so a poll that should return immediately
sleeps out the full 100 ms. §6 *disproves* it as the cause of the 78 s test, but
the bug itself is real and is arguably higher value than either remaining stage
here, because it affects userland rather than test timing. **It is out of scope
for this plan** and wants its own iteration: one `wait_event_timeout` whose
predicate re-checks readiness after enqueue, not N queues.

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

This ranking drove the staging: the change that stops one stolen vCPU from
failing 33 tests is a correctness fix worth making even if steal time did not
exist. It landed first (§3).

---

## 2. Can we consume a paravirtualised steal-time signal?

**Available, and deliberately not used.**

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

**Decision: do not build it.** §3b gets the same
*decision* from one CPUID bit at zero cost. Steal time is recorded in §8 as
the optional upgrade that would turn §3b's blunt "never escalate under a
hypervisor" into a precise "escalate under a hypervisor only when steal time
proves the target actually got CPU". If it is ever built, its best consumer is
**not** the watchdog but §4's AP-pause classifier, where "the target got
0 ns of CPU during this window" is a direct answer to the exact question.

### What in-tree signals actually distinguish "not scheduled" from "wedged"?

The brief asks for these to be evaluated against the code. They were.

| Candidate | Verdict |
|---|---|
| **HPET** (`slopos_kernel_services::clock::monotonic_ns`) | Steal-*immune* as a clock — it is host wall time and keeps advancing regardless. That makes it the right thing to bound a *wait* with (§4). It says nothing about whether the target ran. |
| **TSC delta, self-reported** | **Useless here.** The TSC keeps counting while a vCPU is descheduled — KVM does not stop it — so a large TSC delta is equally consistent with "wedged for 4 s" and "descheduled for 4 s". Discard. |
| **Retired-instruction count** (PMU fixed counter 0, `INST_RETIRED.ANY`) | **Genuinely discriminating**: a descheduled vCPU retires zero, a vCPU spinning with `IF=0` retires billions — which is precisely the case the watchdog most wants to catch. But it needs per-CPU PMU MSR setup, a vPMU the host may not expose, and the same degradation matrix as steal time. Named, not built. |
| **A self-reported "I am at a poll point" counter** | The kernel **already has one**: `ProcessorControlRegion::heartbeat`, bumped from the timer tick (`watchdog.rs:tick`). The signal is not the problem. |

That last row is the crux, and the brief asks for it to be argued explicitly:

> **A descheduled CPU and a wedged CPU are indistinguishable from the watcher
> side without a host signal.** Both stop bumping the heartbeat. Both stop
> acking. Both look identical to every predicate the guest can evaluate.

So perfecting the detection is not available at acceptable cost. The plan
therefore makes the **consequence proportionate** instead: report rather than
abort (§3b, landed), bound the wait on wall time and retry rather than panic on
first failure (§4), and — above all — make one dead CPU cost one test rather
than thirty-three (§3a, landed).

The heartbeat does buy one useful *partial* discriminator, used in §4 as
a timeout classifier rather than as a gate: if the target's heartbeat advanced
during the wait, the target is running and is refusing to park (a real bug);
if it did not advance, the target is either stolen or wedged (report both, blame
neither). That is honest about what it can and cannot tell.

---

## 3. Implemented — the two fixes that made the reproduction green

**This section is history. Nothing in it is a work item; do not re-apply it.**
The code described here is in the tree as of `8fec5938`. It is kept because the
*reasoning* is what a future reader needs when touching these paths — the
step-by-step diffs that were here have been removed so they cannot be mistaken
for instructions.

### 3a. A dead CPU must not hold the AP pause hostage (`80b8ce4e`)

A CPU taking the fatal watchdog NMI halted inside the handler with its
scheduler's `executing_task` still set. Only the owning CPU clears that flag, so
`pause_all_aps` waited on a CPU that would never run again, and every later
`KernelTestScope::enter` panicked.

The fix, in `boot/src/idt.rs` (`nmi_die` and the peer-stop branch) and
`boot/src/panic.rs` (`panic_abort_raw`): a dying CPU force-acks its outstanding
TLB shootdowns, leaves the shootdown target set, marks itself offline and
releases its dispatch flag, all before the backtrace walk that can fault and
never return. `executing_ap` in `sched/src/per_cpu.rs` skips an offline AP, so
either half alone breaks the cascade.

Three properties worth preserving if this is ever touched:

- **`force_ack_local_shootdowns` is not redundant with `notify_cpu_offline`.**
  The latter only removes the CPU from *future* target selection; a shootdown
  already in `wait_for_acks` would still burn 256 re-sends into a panic.
- **`mark_cpu_offline` is load-bearing beyond the AP pause.** `synchronize_rcu`
  gates on `is_cpu_online`, and `test_fixture.rs` calls it immediately after the
  pause — without the offline publication the hang would simply move there.
- **The publication is Release/Acquire only**, against a poison walk that
  force-releases locks a peer takes at once. It narrows the window rather than
  ordering the two events; do not read it as a proved happens-before.

`abandon_dispatch_for_dying_cpu` is safe from a non-returning NMI with
interrupts off because `with_cpu_scheduler` is a bounds check plus a raw index
(`snapshot_for_cpu` takes no `PreemptGuard`) and `set_executing_task` is one
atomic store. It deliberately leaves the dying CPU's task reference alone: that
drop must run on the task's own stack (**I3**).

### 3b. The watchdog must not abort a healthy kernel (`317de749`, `0d9e8419`)

`slopos-ostd/src/watchdog.rs` opened with a premise that is false under
*partial* oversubscription:

> A host that stalls the target stalls the watcher identically.

The host descheduled vCPU 1 while vCPU 0 kept running, so the watcher reached
800 unchanged samples against a healthy CPU and escalated to a fatal NMI.

Per §2 the detection cannot be made correct without a host signal, so the
*consequence* was made proportionate instead: `Report` still fires, still NMIs
the target for its registers and still records the stall in the end-of-phase
maximum, but it no longer takes the machine down where the sample count is not
evidence. `PANIC_ENABLED` became a three-state override so `watchdog.panic=on`
forces escalation back on. Bare metal is unaffected — CPUID.1:ECX[31] reads zero
there, so the default is unchanged.

**The two test guards were wrong on the first attempt and are worth not
repeating.** Both shipped in `317de749`, still failed 2-in-3 contended runs, and
were replaced in `0d9e8419`:

- Counting the *watcher's* ticks across the stall window proves the watcher ran,
  not that it ever looked: a descheduled watcher delivers its ticks in a burst
  once the host runs it again, satisfying any count. Ask what it actually
  observed (`watchdog::max_stall`) instead.
- Closest-of-N calibration selects the sample nearest a baseline that may itself
  be spoiled. Calibration counts LAPIC ticks across an HPET window, so steal can
  only make a read come out *low*; take the largest of N and assert only on
  downward drift.

Neither tolerance was widened, and that is the point: a bound that is loosened
to absorb a spoiled measurement has stopped being a bound.

---

## 4. Stage 1 of 2 (next) — bound the AP pause on wall time, and classify its failures

`sched/src/per_cpu.rs:1102`:

```rust
const AP_PAUSE_SPIN_BUDGET: u32 = 100_000;
```

The budget is measured in the **waiter's** retired instructions, which have no
relation to whether the target got any CPU at all. A descheduled vCPU burns the
entire budget having executed nothing.

### 4a. Wall-clock deadline

`sched` already depends on `slopos-kernel-services` and `sched/src/sleep.rs:23`
already calls `slopos_kernel_services::clock::monotonic_ns()`, so this adds no
dependency and no lockdep class.

```rust
/// Wall-clock budget for the pause wait. Measured, not chosen: see §4e.
const AP_PAUSE_BUDGET_NS: u64 = /* measured; see §4e */;

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

### 4b. Classify the failure: stolen/dead vs. genuinely refusing to park

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

### 4c. Does the AP need to acknowledge? Yes — as evidence, not as the gate

Today the BSP *infers* parking from `is_executing_task()`, and §3a exists
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
An offline AP is excluded, exactly as in §3a. The existing
`nudge_aps_to_poll_point` already delivers the reschedule IPI that wakes an AP
parked in `sti_hlt_cli_atomic`, so an idle AP can ack too.

**Why ack-as-evidence and not ack-as-gate.** Requiring an ack unconditionally
would mean an AP that is legitimately idle-parked must be woken to answer,
turning every pause into an IPI round trip on the test suite's hottest fixture
path. Keeping `!executing` as the fast success condition and the ack as the
corroborating signal keeps the common case at its current cost while making
`NotParking` vs `NotRunning` provable rather than inferred.

### 4d. Proportionate consequence at the call site

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
thirty-two, and §3a has already done that.

`sched/src/task/task_lifecycle.rs:1339` (`task_shutdown`) already steps over a
failed pause deliberately and correctly; it only needs its `match` arm widened
to the two new variants.

### 4e. How to measure `AP_PAUSE_BUDGET_NS`

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

### Stage 1 tests

- `test_pause_deadline_passed_uses_wall_clock_when_available` — pure-function
  test of `pause_deadline_passed` over (start=0, start=live) × (before, after).
- `test_ap_pause_failure_names_a_running_ap_as_not_parking` — the existing
  `test_ap_pause_timeout_is_reported_and_rolled_back` (`:6235`) already holds an
  **online** AP's flag while that AP keeps ticking, so it should now observe
  `NotParking`; update its assertion and keep its rollback check.
- `test_ap_pause_acks_from_the_poll_point` — assert the ack generation advances
  for every online AP across a successful pause.

### Stage 1 gate impact

- `check_lockdep_headroom.sh`: `pause_ack` is an `AtomicU32` inside an existing
  struct — **no new lock class, no new edge.** Verify; do not pre-emptively
  raise anything.
- `check_stack_sizes.sh`: see the `MAX_CPUS` hazard above. Runs on every build.
- `check_wait_result_handling.sh` / `check_wait_predicate_purity.sh`: the new
  `ApPauseError` arms must be matched out explicitly, never `let _ =`'d.
- `check_test_count.sh`: +3 → re-measure.

---

## 5. Stage 2 of 2 — the harness must recognise a halted kernel

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

### Stage 2 tests

Host-side only, via `just check-tests-host` (`go test ./tools/run_tests/...`),
no QEMU:

- `parser_test.go::TestKernelAbortBannerEmitsEvent` — banner + reason line
  produce one `EvKernelAbort` with the reason; a bare banner with no follow-up
  does not panic the parser.
- `parser_test.go::TestKernelAbortDoesNotDisturbKtapParsing` — a KTAP result
  line immediately after the banner still parses (this is the `ts1.log` case).
- `verdict_test.go::TestKernelAbortIsNotGreen` — abort + zero failures ⇒
  non-zero exit and a non-empty diagnostic.

### Stage 2 gate impact

None. No kernel change, no ratchet.

---

## 6. Finding, not a stage — the per-test time inflation is *not* the lost-wake bug

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
  §5 addresses the latter.
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

## 7. Not planned — steal time, only if the landed suppression proves insufficient

Only if the landed suppression (§3b) turns out to hide a real wedge that
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
- Must degrade to §3b's landed behaviour when the leaf is absent (TCG, `-cpu max`,
  bare metal). The absence path is the one to test first, because it is the one
  CI might silently take.

---

## 8. Ordering and acceptance

| Stage | § | Change | Value | Risk |
|---|---|---|---|---|
| ~~done~~ | §3a | Dead CPU publishes offline; AP pause honours it; lockdep test attributes the bypass | **One stolen vCPU costs 1 test, not 33.** Also a real-hardware correctness fix | — |
| ~~done~~ | §3b | No fatal escalation where a stalled heartbeat is not evidence | Stops aborting a healthy kernel; fixes two knife-edge tests | — |
| **1 of 2** | §4 | AP pause on an HPET deadline; ack handshake; classified failure; bounded retry | Removes the last instruction-count bound; makes failures diagnosable | Medium |
| **2 of 2** | §5 | Harness recognises the abort banner and tightens its silence budget | 6-minute diagnosis → ~30 s | Low |

That is the whole remaining sequence. §6 and §7 are **not** rows in this table
because neither is a change: §6 is a finding whose entire value is preventing a
wrong fix, and §7 has a trigger that has not fired.

Land the two as separate commits. Either may be skipped indefinitely without
risk — neither is load-bearing for correctness now the reproduction is green.
Stage 1 removes a real latent bug (an iteration-count bound that nothing
currently trips); Stage 2 is pure harness ergonomics.

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

  Both landed stages met their acceptance: `AP pause failed` 32 → **0** and
  `System halted` 1 → **0**. Stage 2's acceptance is that the gap between the
  last kernel line and process exit is **≤ 25 s**, where it was 134 s.
- Every ratchet that moves gets a fresh `--emit-allowlist` in the same commit
  and a commit-message line naming what added the delta. Never hand-edit
  `scripts/gates/**`.

## 9. Open questions

1. ~~**`panic_abort_raw` marking the caller offline unconditionally.**~~
   **Resolved, shipped unconditional.** The proposed conditional predicate had
   no discriminating power: `stopped_cpu_count()` is bumped only from the
   peer-stop branch, which has not run at `panic_abort_raw` entry, so the guard
   would read the same on every path it was meant to distinguish. Publishing on
   a machine that is going down entirely is inert — every consumer reads offline
   as "do not wait on it", which is what a CPU entering `halt_loop` wants.
2. **Whether the ack handshake should ever become the gate** rather than
   corroboration (§4c). Keeping `!executing` as the fast path preserves
   current cost; making the ack mandatory is stricter but adds an IPI round trip
   to the test suite's hottest fixture path. I chose cost; it is a judgement
   call and reversible.
3. **`AP_PAUSE_BUDGET_NS`'s value is unmeasured.** The procedure is in §4e and
   must be run before the constant is written. A measured maximum in the tens of
   milliseconds should halt the work and be investigated as a separate defect.
4. **The 78 s attribution to lock-holder preemption is inference, not
   measurement** (§6). The elimination of the lost-wake path is conclusive; the
   positive attribution is not. `QEMU_SMP=1` is the cheap confirmation.
5. **Skipping `lockdep_ab_ba_is_detected` on an attributable bypass** (§3).
   Shipped, but it is the one change here that trades a tripwire away. The flag
   is global and sticky, so after any fatal abort the test is skipped for the
   rest of that boot — including in a run that also has a genuine A→B/B→A
   regression. Judged acceptable because `validator_alive()` is *definitionally*
   false after `enter_fatal_bypass()`, so nothing provable is given up, and the
   skip logs its reason and the abort count. An *unattributable* bypass still
   fails, which is the property the assertion exists for. Worth revisiting if
   fatal aborts ever become routine. Confirmed the obvious `oops_count() > 0`
   spelling would have fired in green runs (it reaches 3) and retired the
   tripwire silently.
