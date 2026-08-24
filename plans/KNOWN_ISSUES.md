# SlopOS Known Issues

Last updated: 2026-08-23


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

## Aging-backstop and kernel-io-freeze tests fail when vCPUs are oversubscribed

**Status**: Open (test-harness assumption, no kernel impact observed)
**Severity**: Low
**Component**: `sched/src/sched_tests.rs:7183`, `sched/src/task/task_lifecycle.rs:1228`

`test_low_priority_is_not_starved_by_busy_normal` fails intermittently in CI with
`SCHED: kernel-io task 'netpoll' did not freeze in time` immediately before it.
The CI runner is `blacksmith-4vcpu` and the test ISO boots `QEMU_SMP=4`, so the
four guest vCPUs contend for four host cores; a vCPU descheduled by the *host*
mid-test looks to the guest like a CPU that stopped making progress.

Pre-existing, and not caused by the sleep-queue clock change: reproduced on
unmodified `develop` (commit before `f4db028b`) in **4 of 6 runs** under
`taskset -c 0-3` plus 24 spinning host processes, with the same `netpoll` freeze
warning. Without host contention it does not reproduce — 24 clean runs, 12 on
each side of that commit, all green. Other tests fail in the same runs
(`tcp_data_tests::test_recv_delayed_ack_*`, `test_remote_inbox_drops_non_ready_tasks`,
`test_effective_load_accuracy`), which is the signature of a scheduling
assumption rather than of one broken test.

Both assertions assume the guest owns its CPUs:

- **The aging backstop's bound.** `AGING_THRESHOLD` (`sched/src/fair.rs:33`)
  bounds the wait in *dispatches*, and the test dequeues in a loop expecting the
  `Low` tier to be served within `4 * AGING_THRESHOLD` rounds. A vCPU stolen by
  the host mid-loop breaks that accounting.
- **`freeze_kernel_io_all`'s 50 ms budget** (`KERNEL_IO_FREEZE_MS`,
  `task_lifecycle.rs:1228`) waits on wall-clock HPET time for every kernel-I/O
  thread to acknowledge. A `netpoll` vCPU that is not scheduled by the host
  cannot acknowledge, however healthy the guest is.

The freeze budget is worth re-measuring specifically. It was chosen while sleep
deadlines expired `cpu_count` times early, so a parked kthread noticed a freeze
request up to `cpu_count`x sooner than it asked to; `f4db028b` made those parks
last their full requested duration, which is correct but consumes margin the
50 ms was measured against. No failure has been attributed to that, and the
reproduction above predates the commit — recorded so the next occurrence is read
against it rather than rediscovered.

The fix is to make both bounds robust to a descheduled vCPU rather than to widen
them: the aging test should drive the runqueue without depending on wall-clock
progress, and the freeze wait should distinguish "thread is not running" from
"thread is wedged".

`vcpu-steal-robustness.md` is the implementation plan. Stages 1-2 have landed
(`80b8ce4e`, `317de749`, `0d9e8419`) and addressed a larger finding from the
same reproduction: the 33 failures were one stolen vCPU plus deterministic
fallout, because a CPU taking the watchdog's fatal NMI halted with
`executing_task` still set and only that CPU could ever clear it, so every later
`pause_all_aps` timed out. That part was a real-hardware invariant bug, not a
virtualisation one. The contended reproduction now runs 4/4 green; what remains
below is the wall-clock inflation, not a failure.

Repro: `taskset -c 0-3 builddir/run_tests --raw --no-color` with
`for j in $(seq 1 24); do while :; do :; done & done` running.

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
