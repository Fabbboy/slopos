# Boot Stability Fixes - Current State & Remaining Work

## Date: January 22, 2026

## Overview

This document describes the current state of scheduler and boot stability in SlopOS, what was fixed, and what remains to be addressed.

---

## What Was Fixed

### 1. Test Suite SUITE13 Crash (RESOLVED)

**Problem:** The test suite crashed with triple faults during SUITE13 (scheduler tests), specifically during `test_priority_ordering` or `test_idle_priority_last` tests.

**Root Cause:** When tests called `setup_test_environment()` and `teardown_test_environment()`, the APs (Application Processors) continued running their scheduler loops. During reinitialization:
1. Test teardown would terminate tasks
2. APs would dequeue those tasks from their queues
3. By the time APs tried to execute, tasks were terminated/invalid
4. AP tried to execute garbage memory → triple fault

**Fix Applied:** Added `pause_all_aps()` / `resume_all_aps()` calls to test setup/teardown in `core/src/scheduler/sched_tests.rs`:

```rust
fn setup_test_environment() -> i32 {
    pause_all_aps();  // Stop APs before reinitialization
    task_shutdown_all();
    scheduler_shutdown();
    // ... init code ...
    resume_all_aps();  // Resume APs after reinitialization complete
    0
}

fn teardown_test_environment() {
    pause_all_aps();
    task_shutdown_all();
    scheduler_shutdown();
    resume_all_aps();
}
```

**Result:** All 358 tests across 27 suites now pass consistently.

### 2. Page Allocator Overflow Guard (RESOLVED)

**Problem:** Panic with "attempt to shift left with overflow" at `mm/src/page_alloc.rs:167`.

**Root Cause:** The `order_block_pages()` function performed `1u32 << order` without validating that `order < 32`.

**Fix Applied:** Added guard in `mm/src/page_alloc.rs`:

```rust
fn order_block_pages(order: u32) -> u32 {
    if order >= 32 {
        panic!("order_block_pages: invalid order {} >= 32", order);
    }
    1u32 << order
}
```

---

## Remaining Issue: Boot Crash After Roulette WIN

### Symptom

When the kernel boots and the roulette game results in a WIN:
1. Roulette task terminates normally ("Terminating task 'roulette'")
2. Shell and compositor tasks (which were blocked waiting on roulette) get unblocked
3. Scheduler tries to context switch to one of these tasks
4. **CRASH:** Page fault with RIP pointing to kernel data section

### Crash Details

```
EXCEPTION: Vector 14 (Page Fault)
FATAL: Page fault
Fault address: 0xffffffff802afcd8  (or similar kernel data address)
Error code: 0x11 (Page present) (Read) (Supervisor)
RIP: 0xffffffff802afcd8  (same as fault address - trying to execute data!)
```

The fault address is in the kernel's `.rodata` or `.data` section, NOT in code. This means the CPU is trying to execute a data address as code.

### Analysis

**What we know:**
1. Task context.rip is VALID when `unblock_task()` is called (confirmed via debug logging)
2. Task context.rip becomes INVALID (points to kernel data) by the time context switch executes
3. The corruption happens between scheduling and context switch execution
4. Tests pass because they don't involve the full userland boot path with blocked/unblocked tasks

**Suspected Causes:**

1. **Race Condition in Task Scheduling:**
   - When roulette terminates, it calls `release_task_dependents()` which unblocks shell and compositor
   - Both tasks get scheduled, possibly to different CPUs
   - Some race between BSP and AP accessing the same task structure could corrupt context

2. **Memory Aliasing/Corruption:**
   - Task structure might be getting partially overwritten
   - The context.rip field specifically is being corrupted
   - Could be a use-after-free or double-scheduling issue

3. **SMP Synchronization Issue:**
   - The task might be picked up by an AP while BSP is still modifying it
   - Memory barriers might be missing in critical paths

### Boot Flow Analysis

```
1. Boot completes, tasks created:
   - shell (ID 2) - BLOCKED waiting on roulette
   - compositor (ID 3) - BLOCKED waiting on roulette  
   - roulette (ID 4) - READY, scheduled

2. Scheduler starts, roulette runs

3. Roulette spins wheel, displays result

4. If WIN:
   - roulette calls sys_exit()
   - task_terminate() is called
   - release_task_dependents(4) unblocks shell and compositor
   - Both get schedule_task() called → enqueued to CPU ready queues
   - Context switch to shell or compositor
   - CRASH: RIP is corrupted

5. If LOSS:
   - roulette triggers kernel reboot
   - System reboots, try again
```

### Files Involved

- `core/src/scheduler/task.rs` - `release_task_dependents()`, task termination
- `core/src/scheduler/scheduler.rs` - `unblock_task()`, `schedule_task()`, `do_context_switch()`
- `core/src/scheduler/per_cpu.rs` - Per-CPU scheduler queues
- `userland/src/bootstrap.rs` - Task creation and blocking setup

### Debugging Hints

1. **Add memory barriers** around task context access in `schedule_task()` and context switch paths

2. **Check for double-scheduling** - ensure a task can't be enqueued twice

3. **Verify task pointer validity** before context switch:
   ```rust
   // In prepare_switch() or do_context_switch()
   assert!((*new_task).task_id != INVALID_TASK_ID);
   assert!((*new_task).context.rip >= 0x400000);  // User code range
   ```

4. **Add atomic flag** to prevent concurrent access to task being scheduled

5. **Check if AP picks up task** while BSP is still in `unblock_task()`:
   - The `schedule_task()` call enqueues to a CPU queue
   - An AP might immediately dequeue and try to run it
   - Meanwhile BSP might still be iterating in `release_task_dependents()`

### Proposed Fix Strategy

1. **Immediate:** Add validation in context switch to panic with useful info rather than random crash

2. **Short-term:** Add locking or atomic flags to prevent task from being scheduled while still being modified

3. **Long-term:** Review entire task lifecycle for SMP safety:
   - Task creation → scheduling → execution → termination
   - Ensure proper memory barriers at each transition
   - Consider per-task spinlock for state modifications

---

## Test Status

| Component | Status | Notes |
|-----------|--------|-------|
| Test Suite | ✅ PASS | All 358 tests pass |
| Boot (Loss) | ✅ OK | Reboots as expected |
| Boot (Win) | ❌ CRASH | Page fault after roulette termination |

---

## Commands

```bash
# Run test suite (should pass)
make test

# Boot interactively (may crash on WIN)
VIDEO=1 make boot

# Boot with logging
make boot-log

# Kill stuck QEMU
pkill -9 qemu-system
```
