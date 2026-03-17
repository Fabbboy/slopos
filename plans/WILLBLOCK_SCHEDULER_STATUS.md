# WillBlock Scheduler Refactor — Status

## Summary

Replaced the broken `pending_wakeup` per-task flag with a Linux-style `WillBlock`
task state. Cross-subsystem wakeup contamination is eliminated.

## What was wrong

`pending_wakeup` was a per-task `AtomicBool`. When `unblock_task()` was called on
a Running task, it set this flag. The next `block_current_task()` call — from ANY
subsystem — consumed the flag and skipped blocking. A wakeup from `task_wait_for`
(roulette termination) would poison a subsequent `virtio-blk` block, causing I/O
timeouts.

Neither Linux nor Redox have this problem. Linux uses task state gating
(`TASK_INTERRUPTIBLE`); Redox uses per-condition wait lists. Both make
`try_to_wake_up` on a Running task a no-op.

## What changed

| Component | Change |
|-----------|--------|
| `abi/src/task.rs` | Added `WillBlock = 5` with transitions Running↔WillBlock→Blocked, WillBlock→Ready (preemption) |
| `core/src/scheduler/scheduler.rs` | Added `prepare_to_wait()`, `finish_wait()`. Rewrote `block_current_task()` (WillBlock-gated), `unblock_task()` (WillBlock→Running cancel, Running = no-op). Removed `pending_wakeup`. |
| `core/src/scheduler/sleep.rs` | Added `block_current_task_with_timeout()` with WillBlock gating |
| `core/src/scheduler/task_struct.rs` | Removed `pending_wakeup: AtomicBool` field |
| `core/src/scheduler/task.rs` | Added `task_is_will_block()`, WillBlock in state machine |
| `core/src/scheduler/scheduler.rs` | `requeue_running_task` handles WillBlock (preemption safety) |
| `lib/src/waitqueue.rs` | `wait_event`/`wait_once` use `prepare_to_wait`/`finish_wait` |
| `fs/src/fileio.rs` | Pipe read/write use `prepare_to_wait`/`finish_wait` |
| `core/src/scheduler/futex.rs` | `futex_wait` uses `prepare_to_wait`/`finish_wait` |
| `drivers/src/virtio/mod.rs` | Split `QueueEvent` into `CompletionEvent` + `IrqEdgeEvent` |
| `drivers/src/virtio_blk.rs` | Uses `CompletionEvent` |
| `drivers/src/virtio_net.rs` | Uses `IrqEdgeEvent` |

## Current state

- `pending_wakeup`: **zero references** in codebase
- WillBlock state machine: **working** for WaitQueue, pipes, futex, task_wait_for
- CompletionEvent: **HPET poll only** (scheduler path disabled)
- All 61 test suites pass, full boot to compositor + shell works

## Open: CompletionEvent scheduler integration

The `CompletionEvent` (virtio-blk) does not yet use the scheduler-backed blocking
path (`prepare_to_wait` + `block_current_task_with_timeout`). It uses HPET
cli/sti/hlt polling instead.

The scheduler path has an unresolved hang: after `task_wait_for` wakes the init
task, the next `CompletionEvent` block never completes. All other blocking callers
(WaitQueue, pipes, futex) work correctly with the WillBlock state machine. The
issue is specific to CompletionEvent's single-waiter + waiter-pointer + IRQ-driven
`unblock_task` pattern interacting with the CAS sequence in
`block_current_task_with_timeout`.

Debugging approach for the next session: add `klog_debug` tracing inside
`block_current_task_with_timeout` to log which guard clause triggers (early return
vs actual block vs CAS failure), then reproduce the hang and read serial output.

Impact of the HPET fallback: virtio-blk I/O works correctly but burns CPU cycles
during the wait instead of yielding to the scheduler. Under QEMU this is
negligible (device responds in microseconds). On real hardware with slower devices
it would matter more.
