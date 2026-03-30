# Kernel Poll Wakeup Race Condition

## Summary

`syscall_poll` in `core/src/syscall/fs/poll_ioctl_handlers.rs` has an SMP race condition where wakeup signals are lost, causing poll to sleep for up to 100ms even when data is immediately available. This affects all poll users including the compositor protocol.

## The Bug

The poll handler calls `sleep_current_task_ms()` which forcefully sets the task state to `Blocked` (via `task_set_state_with_reason` at `sleep.rs:199`). This overwrites the `WillBlock` state set by `prepare_to_wait()`. If a wakeup arrives between `prepare_to_wait` and `sleep_current_task_ms`, the `unblock_task` call correctly flips the state from `WillBlock` to `Running` — but then `sleep_current_task_ms` overwrites `Running` with `Blocked`, destroying the wake signal.

## Timeline of the Race

```
Task A (poll caller)              Task B (data sender)
─────────────────────             ────────────────────
prepare_to_wait()
  → state = WillBlock
register on WaitQueue
check readiness → empty
                                  write data to socket
                                  wake_all(RECV_WQ)
                                    → unblock_task(A)
                                    → state = Running ✓
sleep_current_task_ms(100)
  → task_set_state(Blocked)  ← OVERWRITES Running!
  → unschedule + schedule
  → sleeps for 100ms          ← WAKE LOST
```

## What Was Tried

### Fix 1: Register-before-check (partial, committed)
Restructured the poll loop from CHECK → REGISTER → SLEEP to REGISTER → CHECK → SLEEP (matching Linux's `do_poll`). This narrows the race window but doesn't eliminate it because `sleep_current_task_ms` still overwrites WillBlock/Running states.

### Fix 2: Use `block_current_task_with_timeout` (correct but failed)
Replaced `sleep_current_task_ms` with `block_current_task_with_timeout` which checks `task_is_will_block()` before blocking (line 243 in `sleep.rs`). Empirically made things **worse** (7/10 failures vs 5/10). This suggests the `WillBlock` / `unblock_task` SMP interaction itself has a deeper issue — likely missing memory barriers or atomic ordering on the task state transitions.

## How Linux Solves This

Linux's `do_poll()` in `fs/select.c` uses a fundamentally different approach:

1. The file's `->poll()` method receives a `poll_table *` callback
2. Inside `->poll()`, `poll_wait()` registers the waiter on the wait queue
3. Then `->poll()` checks and returns current readiness
4. Registration and check happen under the **same subsystem lock** (e.g., the socket lock)
5. `set_current_state(TASK_INTERRUPTIBLE)` is set before registration
6. `schedule()` checks the task state — if `try_to_wake_up()` already set it to `TASK_RUNNING`, schedule is a no-op

The critical difference: Linux's task state transitions (`TASK_INTERRUPTIBLE` → `TASK_RUNNING`) are atomic operations with proper memory barriers via `set_current_state()` and `try_to_wake_up()`. SlopOS's `WillBlock` → `Running` → `Blocked` sequence has gaps where state can be overwritten.

## Root Cause Hypothesis

The `WillBlock` mechanism in `sync/src/waitqueue.rs` works correctly for `WaitQueue::wait_event()` because it uses `block_current_task()` which checks WillBlock atomically. But `syscall_poll` uses `sleep_current_task_ms()` which is a timer-based sleep that doesn't participate in the WillBlock protocol.

`block_current_task_with_timeout` should fix this (it checks WillBlock at line 243), but empirically fails. This suggests either:
1. Missing memory barriers between `prepare_to_wait()` / `unblock_task()` / `block_current_task_with_timeout()` on SMP
2. A race in the sleep queue's `upsert` interacting with the WillBlock check
3. The `unblock_task` running on a different CPU sees a stale task state due to cache coherency

## Recommended Fix

Implement Linux's fused poll approach: modify `FileOps::poll_events` to accept a `register: bool` parameter. When true, the implementation both registers the waiter AND returns readiness under the same lock, eliminating the race window entirely. This requires touching every `FileOps` implementor but is the only approach that eliminates the race by construction rather than relying on SMP memory ordering.

Alternatively, audit and fix the memory ordering in:
- `task_set_state_with_reason()` — should use `Release` ordering
- `task_is_will_block()` — should use `Acquire` ordering
- `unblock_task()` — needs a full memory barrier between reading WillBlock and setting Running

## Current Workaround

The compositor protocol defers the OutputInfo handshake to `ensure_output_info()` during surface creation instead of during `Client::connect()`. By the time apps create surfaces, the compositor has had multiple frame cycles to accept connections and send OutputInfo. The data is already in the socket buffer, so `wait_recv` finds it immediately without needing poll wakeup. This is actually the correct design (Wayland also doesn't block during connect), but it doesn't fix the underlying kernel poll bug which affects all poll users.

## Files Involved

- `core/src/syscall/fs/poll_ioctl_handlers.rs` — poll syscall handler (register-before-check fix committed)
- `core/src/scheduler/sleep.rs:167` — `sleep_current_task_ms` (overwrites WillBlock)
- `core/src/scheduler/sleep.rs:222` — `block_current_task_with_timeout` (checks WillBlock but SMP-unsafe)
- `core/src/scheduler/scheduler.rs:860` — `block_current_task` (used by WaitQueue)
- `sync/src/waitqueue.rs:158` — `wait_event` (correct pattern, works for non-poll uses)
