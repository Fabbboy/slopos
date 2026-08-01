# SlopOS Known Issues

Last updated: 2026-08-01


---

## Multi-CPU userland blocked by a buddy/slab cross-CPU TLB-shootdown deadlock

**Status**: Open (thread-per-core *scheduler* half resolved; the *allocator* half is the blocker)  
**Severity**: Low (no production consumer; the test suite runs userland co-located)  
**Component**: `mm/src/slab/`, `mm/src/buddy*` (the SMP alloc / reuse-drain path)

### Description

The thread-per-core **scheduler** path is complete. A `ring_enter`-parked reactor woken cross-core is
now re-dispatched on an affinity-permitted CPU (the remote-inbox flush before every dispatch pick +
affinity-honoring wake selection), a runnable thread whose affinity no longer permits its current CPU
is repatriated (`task_apply_affinity` + the switch-out-tail migration), `select_target_cpu` falls back
to an affinity-permitted online CPU instead of misplacing, and `set_cpu_affinity` re-places. Verified:
with strict per-worker pins, `percore_reactor_test`'s N reactors run on N distinct cores and the
cross-core round-trip completes (each worker on its pinned CPU, `multi_cpu && each_on_pinned_cpu`).

The remaining blocker is in the **allocator**, exposed (not caused) by the distribution the scheduler
now produces: under sustained concurrent allocation on multiple CPUs, a `SlabAllocator::alloc_one`
running under a per-CPU `CpuLocal::get_pinned_mut` pin (interrupts off) can reach the buddy reuse
path, whose synchronous cross-CPU TLB shootdown spins with interrupts off — stopping that CPU's timer
tick until the lockup detector reports it (`WATCHDOG: cpu N made no progress`). This is the latent
"heap-alloc under a cli-lock / LUF reuse-drain is a hidden cross-CPU wait" hazard. It is intermittent
(~2/12 with 4 strictly-pinned workers) and does **not** occur co-located, so `percore_reactor_test`
ships with the co-located `(1 << idx) | 1` mask and the full suite is reliably green.

### Fix (allocator SMP hardening)

The buddy reuse-path cross-CPU TLB shootdown must not spin interrupts-off under a slab per-CPU pin.
Candidates: defer the shootdown out of the pinned/IRQ-off region; make the shootdown wait
tick-pumping / interruptible; or refill slab magazines without holding the pin across a buddy reuse
drain. See the related latent notes on synchronous cross-CPU shootdown on the buddy reuse path.

### Related Files

- `mm/src/slab/allocator.rs`, `mm/src/slab/magazine.rs` — `alloc_one`, `get_pinned_mut`
- `mm/src/buddy*` — the reuse-drain / cross-CPU TLB shootdown
- `userland/src/bin/tests/percore_reactor_test.rs` — flip the worker mask to strict `1 << idx` and
  promote the `multi_cpu` / `each_on_pinned_cpu` assertions once the allocator is hardened; the
  scheduler already passes that variant except for this deadlock

---

## A task on an AP can stall behind three unbounded or O(CPUs) scheduler waits

**Status**: Open
**Severity**: Low (latency only; no correctness consequence)
**Component**: `sched/src/scheduler.rs`, `sched/src/task/task_reclaim.rs`

Task termination itself does not stall an AP — `task_terminate` serialises with a
local `PreemptGuard` (`sched/src/task/task_lifecycle.rs:836`), and the AP pause is
reached only from `task_shutdown_all` (`:1177`), whose sole production caller is
`kernel_shutdown` (`boot/src/shutdown.rs:165`). Three other waits on the ordinary
scheduling path can, and a latency-sensitive task such as the compositor is where
they would be seen.

**The `on_cpu` handover spin** (`scheduler.rs:1240-1242`) is the one genuinely
unbounded wait a dispatching AP can hit. Having dequeued a task whose prior CPU
has not finished its switch-out tail, the AP spins on that CPU's Release store of
`on_cpu` with no bound and no fallback. The window is short by construction — it
is the tail of a context switch — but a prior CPU that takes an interrupt inside
it extends the spin by exactly that handler's duration.

**`unschedule_task`'s per-CPU sweep** (`scheduler.rs:1017-1026`) takes every
CPU's scheduler in turn on every termination, to find the one queue the task
might be on. O(CPUs) lock acquisitions per termination, on a path a
spawn-heavy workload runs constantly.

**The task graveyard drains only from the idle dispatcher**
(`task_reclaim.rs:174-208`), so under sustained load dead tasks' kernel stacks and
address spaces accumulate until some CPU goes idle. Already tracked as item 2 of
`plans/deferred-work.md`; recorded here only because it shares this shape.

Fixes for the first two: bound the `on_cpu` spin, re-enqueueing rather than
spinning past a threshold; and record the owning CPU on the task so
`unschedule_task` takes one lock instead of `n`. Neither is scheduled work.

---

## Tickless idle never arms, and advertises a wake it does not deliver

**Status**: Open (unreachable code, not a regression)
**Severity**: Low (the 100 Hz periodic tick is what actually runs)
**Component**: `sched/src/scheduler.rs` (`arm_tickless_idle_if_due`, `restore_periodic_if_armed`)

`arm_tickless_idle_if_due` converts the next sleep-queue deadline to milliseconds
and returns early unless it is *under* the 10 ms periodic period. The sleep queue
counts in timer ticks and `platform::timer_frequency()` is 100 on every path, so
one tick converts to exactly 10 ms and the `>=` returns. `delta == 0` returns
earlier. There is no reachable input that reaches
`timer_program_next_wakeup_ms`, so `ONESHOT_ARMED` is never set and
`restore_periodic_if_armed` always takes its false branch.

Two claims rest on it and are false today: the doc comment promising a
`KernelIo` task that sleeps 1 ms wakes at 1 ms, and the comment at
`scheduler_timer_tick`'s head saying an unrelated IRQ restores periodic mode —
`scheduler_timer_tick` is reached only from the LAPIC timer vector.

The honest fix is a sub-10 ms unit for the sleep queue, not patching the
comparison. Sequence it **after** the lockup detector: the detector's eligibility
model assumes a CPU either ticks at a flat 100 Hz or is marked not-armed, and a
real one-shot path would need to bump the heartbeat from the one-shot ISR too.

---

## `slopos-ostd` host tests flake on interrupt-state assertions

**Status**: Open (test-harness only, no kernel impact)
**Severity**: Low
**Component**: `slopos-ostd/src/task/`, `cargo test -p slopos-ostd --lib`

`task::fpu_owner::tests::restoring_over_an_unsaved_task_is_refused` fails
intermittently with "Task dropped with interrupts disabled" from
`slopos-ostd/src/task/drop_context.rs`. The interrupt-state mock it consults is
process-global and `cargo test` runs the lib tests on parallel threads, so
another test's state reaches it. Reproduces roughly one run in ten and passes
five-for-five in isolation.

Same shape as the serialisation already applied elsewhere for a process-global
counter: the assertion needs a serial gate, not a wider tolerance.

---

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `slopos-ostd/src/cpu/x86_64/pcr.rs` for architecture details.
