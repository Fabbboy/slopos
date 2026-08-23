# SlopOS Known Issues

Last updated: 2026-08-08


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

**The `on_cpu` handover spin** (`scheduler.rs:1286`) is the one genuinely
unbounded wait a dispatching AP can hit. Having dequeued a task whose prior CPU
has not finished its switch-out tail, the AP spins on that CPU's Release store of
`on_cpu` with no bound and no fallback. The window is short by construction — it
is the tail of a context switch — but a prior CPU that takes an interrupt inside
it extends the spin by exactly that handler's duration. The spin now services
pending cross-CPU work (`sync::spin_relax`), so it can no longer be half of a
mutual wait with a peer blocked on a shootdown this CPU owes; what remains is
latency, not a hang.

**`unschedule_task`'s per-CPU sweep** (`scheduler.rs:1017-1026`) takes every
CPU's scheduler in turn on every termination, to find the one queue the task
might be on. O(CPUs) lock acquisitions per termination, on a path a
spawn-heavy workload runs constantly.

**The task graveyard drains only from the idle dispatcher**
(`task_reclaim.rs:174-208`), so under sustained load dead tasks' kernel stacks and
address spaces accumulate until some CPU goes idle. The fix is a per-CPU deferred-work
list so reclaim stops depending on CPU 0 being idle; recorded here because it shares this
shape.

Fixes for the first two: bound the `on_cpu` spin, re-enqueueing rather than
spinning past a threshold; and record the owning CPU on the task so
`unschedule_task` takes one lock instead of `n`. Neither is scheduled work.

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
