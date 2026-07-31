# SlopOS Known Issues

Last updated: 2026-07-30


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
tick until the NMI watchdog fires (`NMI WATCHDOG: CPU N not responding`). This is the latent
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

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `slopos-ostd/src/cpu/x86_64/pcr.rs` for architecture details.
