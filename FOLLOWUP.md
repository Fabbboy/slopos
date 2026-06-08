# FOLLOWUP — scheduler runnable-ownership refactor

Prompt for the next agent. Read this whole file before touching code.

## Hard rule for the next session

**Do not run QEMU/test commands without an explicit short timeout.** The current
remaining failure can hang the userland phase. Use the Go wrapper's own timeout:

```bash
builddir/run_tests --timeout-secs 25 --silence-secs 25 ...
```

For no-build reruns:

```bash
builddir/run_tests --no-build --iso builddir/slop-tests.iso \
  --fs-image fs/assets/ext2-tests.img --timeout-secs 25 --silence-secs 25
```

If an interrupted run leaves the ext2 image locked, check and kill only stale
repo-local QEMU instances:

```bash
pgrep -af 'qemu-system-x86_64|builddir/run_tests|tools/run_tests'
kill <stale-qemu-pids>
```

## Current goal

The original problem was `SCHED: rescuing stranded READY task <id>` spam. The
architectural direction is correct: **`TaskStatus::Ready` must never be the only
proof of runnability.** Runnable publication must be paired with explicit
scheduler placement/ownership.

The refactor is mostly implemented, but **not fully stable**: rescue spam appears
closed in boot smokes, while the userland phase is still flaky and can hang/panic
inside `slopfut`.

## Implemented scheduler model

### New explicit placement state

`slopos-ostd/src/task/kernel_task.rs` now has:

```rust
SchedPlacement::{None, ReadyQueue, RemoteWake, OnCpu, Migrating, Waking}
```

`TaskInner` carries `sched_placement: AtomicU8` and OSTD accessors are re-exported
through `sched::task`.

Meaning:

- `ReadyQueue`, `RemoteWake`, `OnCpu`, `Migrating` are **durable scheduler
  ownership**.
- `Waking` is **not durable ownership**. It is a transient publication token:
  some actor has reserved the right/obligation to turn `Ready` into a durable
  queue/inbox/on-cpu owner.
- `None` means no scheduler owner.

Do not conflate `task_sched_placement_is_owned()` with durable ownership: that
OSTD helper currently returns true for `Waking`, which is too broad for scheduler
rescue/publication decisions. `sched/src/scheduler.rs` now has local helpers:
`placement_is_durable_owner()` / `task_has_durable_owner()`.

### New-task contract

`task_create()` deliberately returns a fully initialized but **non-runnable**
task (`Blocked + None`). It no longer publishes tasks as `Ready`.

The only intended production edge for a new task is:

```rust
scheduler::publish_new_task(task)
```

Converted paths include:

- `core/src/exec/mod.rs`
- `sched/src/runtime.rs`
- `sched/src/kthread.rs`
- fork/clone paths in `sched/src/task/task_lifecycle.rs`

### Raw Ready publisher defense

OSTD raw state setters / transitions that publish `TaskStatus::Ready` reserve
`SchedPlacement::Waking`, so bare `Ready + None` is harder to create from legacy
or test code.

### Ready queue / remote inbox ownership

`per_cpu.rs` now transfers placement explicitly:

- dequeue: `ReadyQueue -> OnCpu`
- requeue current: `OnCpu -> ReadyQueue`
- wake/new task publish: `Waking -> ReadyQueue` or `Waking -> RemoteWake`
- work stealing: `ReadyQueue -> Migrating -> ReadyQueue`
- remote inbox uses role-typed `remote_inbox_link`, so it is distinct from the
  ready-queue link.

### Wake path

`wake_blocked_task()` is now the unified sleep/block wake publisher. Important
current semantics:

- Wake of `None + Blocked`: `None -> Waking`, CAS `Blocked -> Ready`, then publish
  from `Waking`.
- Wake of `Waking`: complete the existing publication reservation.
- Wake of `ReadyQueue`/`RemoteWake`/`Migrating`: state CAS only; durable owner
  already exists.
- Wake of `OnCpu`: only convert `OnCpu -> Waking` if the task is actually
  `Blocked`. This gate is critical: `OnCpu` is also the dispatcher's transient
  claim after dequeue and before `Ready -> Running`; stealing that claim from an
  already-Ready task caused `slopfut`/userland hangs.

`publish_reserved_waking_ready()` and `publish_ready_from_current_owner()` in
`sched/src/scheduler.rs` are the intended central publication helpers.

### Rescue sweep status

Rescue is still present as a diagnostic tripwire. It now treats `Ready+Waking` as
not durably owned and, after strike threshold, completes that leaked reservation
through `enqueue_waking()`.

If rescue fires now, it is still a real invariant violation and should not be
ignored.

## Files changed in this worktree

Main scheduler/OSTD files:

- `sched/src/scheduler.rs`
- `sched/src/per_cpu.rs`
- `sched/src/sleep.rs`
- `sched/src/work_steal.rs`
- `sched/src/runtime.rs`
- `sched/src/kthread.rs`
- `sched/src/task/task_lifecycle.rs`
- `slopos-ostd/src/task/kernel_task.rs`
- `slopos-ostd/src/task/accessors.rs`
- `slopos-ostd/src/task/link_roles.rs`
- `slopos-ostd/src/task/mod.rs`
- `slopos-ostd/src/sync/intrusive.rs`
- `slopos-ostd/src/sync/wait_queue.rs`

Tests/docs touched:

- `sched/src/sched_tests.rs`
- `sched/src/context_tests.rs`
- `core/src/syscall/tests.rs`
- `docs/scheduler_wakeup_invariants.md`

There is also an untracked `test_output.log.raw`; review/remove before commit.

## Validation already done

- `cargo fmt --all` run after edits.
- `just build` green after the latest `OnCpu` wake gate.
- Scheduler kernel tests pass: `slopos_sched::*` kernel phase reported `104 pass`.
  The normal wrapper then continues into the flaky userland phase, so use
  `--timeout-secs 25` on reruns.
- Userland phase sometimes passes completely (`19/19` in ~0.7–1.0s guest time)
  under the 25s cap.
- Boot smokes: user accepted the 30-ish no-test boot loop as sufficient for the
  rescue-spam class. No `SCHED: rescuing stranded READY`, publish-failure, panic,
  or preempt-underflow was observed in the accepted boot-smoke run.

## Remaining open problem: flaky userland `slopfut` stall

The current blocker is **not the old visible rescue spam**. It is a userland-phase
flake where the harness can hang/panic around `slopfut`:

```text
thread 'main' (1) panicked at slopos-rt/src/slopfut/executor.rs:186:13:
slopfut: stalled — root Pending with no in-flight op and no ready task
```

Observed with:

```bash
builddir/run_tests --no-build --iso builddir/slop-tests.iso \
  --fs-image fs/assets/ext2-tests.img --timeout-secs 25 --silence-secs 25
```

Typical tail before the stall:

```text
fork_test: pipeline repro PASS
io_capture_test: ... nc exit=1
percore_reactor: workers=4 replies=200 distinct_cpus=[0] ... all_seen=true
thread 'main' (1) panicked at slopos-rt/src/slopfut/executor.rs:186:13:
slopfut: stalled — root Pending with no in-flight op and no ready task
```

A 5x repeat after the latest `OnCpu` gate got 3 passes, then failed on repeat 4
with the slopfut stall. So it is intermittent.

### Important harness note

`tests.run=<glob>` filters the kernel phase, but the userland phase still tends
to run all 19 utests. A command like:

```bash
builddir/run_tests --filter 'slopos_core::utests::utest_slopfut' ...
```

still reported `OK userland: 19 pass` when it passed. Do not assume it isolated
that one userland binary.

### Next debugging step

Instrument `userland/src/bin/tests/slopfut_test.rs` or
`slibc/src/test_harness.rs` to report/print **case start** as well as case end.
The slopfut test has internal cases:

- `spawn_join`
- `join2`
- `timeout`
- `notify`
- `oneshot`
- `mpsc`
- `yield_now`
- `child_wait`
- `signal_recv`

Currently `test_harness::run()` only reports after each case returns, so a stall
inside a case is opaque. Add temporary `report(Pass/Skip?)` or klog/printf before
calling each case, rebuild, and run with the 25s wrapper timeout. Remove noisy
instrumentation before handoff/commit.

Potentially relevant previous observation: when the buggy `OnCpu -> Waking`
conversion stole the dispatch claim from already-Ready tasks, userland stalled
much more reliably. The current flake may still be a scheduler handoff/Ready
publication issue, but there is no rescue log in the tail. Look for leaked
`Ready+Waking`, `Ready+None`, or a task parked `Pending` in userland with no
kernel wake source.

## Structural work still worth doing (Rust can enforce more)

The current code is safer but still uses raw enum stores/CASes in too many places.
A stronger follow-up would make illegal states harder to express:

1. Split `Waking` from durable ownership at the type/API level.
   - Durable owner token: ReadyQueue/RemoteWake/OnCpu/Migrating.
   - Publication token: Waking only.
2. Replace raw `schedule_task_from_placement(task, from, ...)` with functions
   that consume typed tokens (`WakingToken`, `OnCpuToken`, etc.).
3. Stop exposing generic placement store/CAS to normal scheduler code except in
   a small ownership module.
4. Make `Ready` publication APIs return/require a publication token instead of
   allowing `task_set_state(... Ready)` followed by a best-effort enqueue.
5. Keep OSTD as the only unsafe/intrusive domain; this can all be safe Rust in
   `sched` with zero new unsafe.

This is the direction the user wants: not a bandaid, but scheduler ownership as
a type-level protocol.

## Do not redo these already-resolved investigations

- `task_create()` returning Ready was removed. New tasks are born non-runnable.
- `publish_new_task()` is the new-task API.
- Raw Ready stores reserve `Waking`.
- Remote inbox duplicate membership is guarded by `remote_inbox_link` and
  placement CASes.
- Producer-side `PreemptGuard` in `wake_blocked_task()` was tried and removed;
  it caused/prefigured `preempt_count` underflow paths and is not the right fix.
- An OSTD SpinLock drop-order experiment was tried and reverted. Do not resurrect
  it as the scheduler fix.

## Suggested bounded commands

Build only:

```bash
cargo fmt --all
just build
```

Scheduler kernel phase with QEMU capped (the userland phase may still run and
flake, so inspect kernel summary first):

```bash
builddir/run_tests --filter 'slopos_sched::*' --timeout-secs 25 --silence-secs 25
```

Userland flake repro (after a test ISO exists):

```bash
builddir/run_tests --no-build --iso builddir/slop-tests.iso \
  --fs-image fs/assets/ext2-tests.img --timeout-secs 25 --silence-secs 25 --raw
```

JSON event capture:

```bash
rm -f builddir/userland-stall.jsonl
builddir/run_tests --no-build --iso builddir/slop-tests.iso \
  --fs-image fs/assets/ext2-tests.img --timeout-secs 25 --silence-secs 25 \
  --json builddir/userland-stall.jsonl --raw
```
