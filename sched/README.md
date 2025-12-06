# SlopOS Scheduler Diagnostics

This kernel snapshot extends the cooperative scheduler with richer diagnostics and
termination handling. A quick recap of the metrics printed by
`print_scheduler_stats()` and how to interpret them:

- **Context switches** — incremented whenever the scheduler hands execution to a
  different task. The counter in `scheduler.c` matches the task manager’s
  `total_context_switches`. A sudden spike usually means a task is yielding (or
  exiting) more frequently than expected.
- **Voluntary yields** — counts explicit `yield()` calls. The aggregate value in
  the task manager is also exposed via `task_get_total_yields()` and the
  per-task `yield_count` column.
- **Schedule calls** — raw invocations of `schedule()`. A large delta without a
  corresponding switch often indicates that the only runnable task was the
  idle loop.
- **Ready tasks** — snapshot of how many tasks are waiting in the ready queue
  when the statistics are sampled.
- **Tasks created / active tasks** — lifecycle counters from the task manager.
  Active tasks excludes slots that have already been reclaimed after a
  termination.
- **Active task metrics block** — every runnable task prints its name, numeric
  ID, human-readable state, accumulated runtime, and yield count. Runtime is
  measured in scheduler ticks (see below). If a task never runs, its runtime
  will stay at 0.

## Timestamp and runtime units

`kdiag_timestamp()` now sources its base clock from the PIT/APIC timer
interrupt (via `irq_get_timer_ticks()`), falling back to the TSC until the
first tick arrives. Task runtime and scheduling deltas use the same tick units,
so a value of 1 means “the task held the CPU for one timer interrupt interval”.
Multiply by your configured PIT frequency to convert to real time.

Because tasks that return now funnel through `scheduler_task_exit()`, the
statistics stay consistent: terminating tasks record their final runtime slice,
remove themselves from the ready queue, and wake any dependants that were
blocked waiting for them to finish. Tasks that genuinely need to wait on another
task can call `task_wait_for(target_id)`, which records the dependency and
blocks the caller until the target reaches the termination path.
