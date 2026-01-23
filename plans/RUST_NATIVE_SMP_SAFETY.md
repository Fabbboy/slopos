# Rust-Native SMP Safety Plan

## Current Status (January 2026)

### Implementation State

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1a | IrqRwLock in lib | ✅ COMPLETED |
| Phase 1b | task_lock.rs with type aliases | ✅ COMPLETED |
| Phase 1c | task_find_by_id_ref() function | ❌ CANCELLED |
| Phase 2 | TaskStatus/BlockReason enums in abi | ❌ CANCELLED |
| Phase 3 | Type-safe state transitions | ❌ CANCELLED |
| Phase 4 | Memory barriers in unblock_task | ✅ COMPLETED |
| Phase 5 | Memory barriers in schedule_task | ✅ COMPLETED |

### Test Results

```
TESTS SUMMARY: total=358 passed=358 failed=0
```

### Boot Behavior

| Path | Result |
|------|--------|
| LOSE | ✅ Works - kernel reboots correctly |
| WIN | ❌ CRASHES - page fault at invalid kernel address |

---

## Problem Statement

The current scheduler has a critical race condition when the roulette task terminates with a WIN:

1. Roulette task calls `sys_exit()` after WIN
2. `task_terminate()` is called
3. `release_task_dependents()` unblocks shell/compositor tasks
4. AP picks up unblocked task and attempts to execute
5. **CRASH**: Execution jumps to invalid address

### Current Observed Crash (after memory barrier fixes)

```
EXCEPTION: Vector 14 (Page Fault)
FATAL: Page fault
Fault address: 0xffffffff80180128
Error code: 0x11 (Page present) (Read) (Supervisor)
RIP: 0xffffffff80180128
```

The crash is now a page fault - execution is jumping to an invalid kernel address (past `_user_text_start` but not valid kernel code).

### Boot Sequence on WIN

```
ROULETTE: start
ROULETTE: fb_info ok, drawing wheel
Terminating task 'roulette' (ID 4)
release_task_dependents: task 2 ptr=0xffffffff8019b7a0 rip=0x400010 state=3
release_task_dependents: Unblocked task 2
release_task_dependents: task 3 ptr=0xffffffff8019bb40 rip=0x400010 state=3
release_task_dependents: Unblocked task 3
EXCEPTION: Vector 14 (Page Fault)
```

Note: Task RIP values are VALID (`0x400010`) when unblocked. Corruption occurs during/after context switch.

---

## What Was Implemented

### Phase 1a: IrqRwLock

**File**: `lib/src/spinlock.rs`

Added `IrqRwLock<T>` - a reader-writer lock that disables interrupts during critical sections:

```rust
pub struct IrqRwLock<T: ?Sized> { ... }
pub struct IrqRwLockReadGuard<'a, T: ?Sized> { ... }
pub struct IrqRwLockWriteGuard<'a, T: ?Sized> { ... }
```

Exported from `lib/src/lib.rs`.

### Phase 1b: task_lock Module

**File**: `core/src/scheduler/task_lock.rs`

Created type aliases for future use:

```rust
pub type TaskRef = Arc<TaskLock>;
pub type TaskLock = IrqRwLock<Task>;
pub type TaskReadGuard<'a> = IrqRwLockReadGuard<'a, Task>;
pub type TaskWriteGuard<'a> = IrqRwLockWriteGuard<'a, Task>;
```

### Phase 4: Memory Barriers in unblock_task

**File**: `core/src/scheduler/scheduler.rs`

Added memory barrier after state transition and early return on failure:

```rust
pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
        // ... logging ...
        return -1;  // Early return on failure
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    schedule_task(task)
}
```

### Phase 5: Memory Barriers in schedule_task

**File**: `core/src/scheduler/scheduler.rs`

Added memory barrier before sending IPI to another CPU:

```rust
// In schedule_task(), when task goes to another CPU:
core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

if target_cpu != current_cpu && slopos_lib::is_cpu_online(target_cpu) {
    send_reschedule_ipi(target_cpu);
}
```

### Debugging Additions

**File**: `core/src/scheduler/per_cpu.rs`

Added self-pointer sanity checks in `enqueue_local()` and `dequeue_highest_priority()`.

**File**: `core/src/scheduler/task.rs`

Added debug logging in `release_task_dependents` showing task ptr, RIP, and state when unblocking.

---

## What Was Cancelled

### ABI Crate Modification Issue

**CRITICAL**: Any modification to `abi/src/task.rs` causes mysterious test failures.

#### What Was Attempted

Adding `TaskStatus` enum to end of file caused tests to panic at `mm/src/page_alloc.rs:957`. Even adding UNUSED code caused failures.

#### Hypothesis

The ABI crate is compiled with different settings and linked into multiple components. Adding code changes binary layout and symbol addresses.

#### Cancelled Phases

| Phase | What It Would Have Done |
|-------|------------------------|
| 1c | `task_find_by_id_ref()` returning `Option<TaskRef>` |
| 2a | `TaskStatus` enum in abi |
| 2b | `BlockReason` enum in abi |
| 2c | `block_reason` field in Task struct |
| 3a | `mark_ready()`, `mark_running()` etc. methods |
| 3b | `InvalidTransition` error type |

---

## Files Modified

| File | Changes |
|------|---------|
| `lib/src/spinlock.rs` | Added IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard |
| `lib/src/lib.rs` | Export new lock types |
| `core/src/scheduler/task_lock.rs` | NEW FILE - type aliases |
| `core/src/scheduler/mod.rs` | Added task_lock module |
| `core/src/scheduler/scheduler.rs` | Memory barriers in unblock_task and schedule_task |
| `core/src/scheduler/per_cpu.rs` | Self-pointer sanity checks |
| `core/src/scheduler/task.rs` | Debug logging in release_task_dependents |

---

## Next Steps for Investigation

### Priority 1: WIN Path Page Fault

The crash changed from syscall argument corruption to a page fault at an invalid address. Need to investigate:

1. Why is RIP jumping to `0xffffffff80180128` (past `_user_text_start`)?
2. Is there corruption in the task context during AP pickup?
3. Is CR3/page table incorrect when AP starts executing?
4. Is there a race between BSP context save and AP context restore?

### Priority 2: ABI Crate Issue

Investigate why ABI modifications cause failures:

1. Check if `abi` crate uses different optimization levels
2. Verify linker script handles abi symbols correctly
3. Check for hardcoded offsets in assembly

### Priority 3: Complete Original Plan

Once blockers are resolved:

1. Add TaskStatus enum (if ABI issue fixed)
2. Convert to type-safe state transitions
3. Implement full TaskRef-based scheduling

---

## Commands for Testing

```bash
# Run tests (MUST see passed=358 failed=0)
make test 2>&1 | tail -10

# Boot with extended timeout to see WIN path
BOOT_LOG_TIMEOUT=180 make boot-log

# Check for specific output
grep -E "(ROULETTE|release_task|panic|EXCEPTION)" test_output.log

# Kill stuck QEMU
pkill -9 qemu-system
```

---

## Original Design (For Reference)

The original plan was to implement Redox OS-style task locking:

```rust
pub type TaskRef = Arc<IrqRwLock<Task>>;

pub fn unblock_and_schedule(task_ref: &TaskRef) -> Result<(), Error> {
    let mut guard = task_ref.write();
    guard.mark_ready()?;
    enqueue_to_cpu(target_cpu, task_ref.clone());
    drop(guard);
    send_ipi(target_cpu);
    Ok(())
}
```

This design ensures:
- All task access requires holding the lock
- State transitions are type-checked at compile time
- Memory barriers are implicit in lock release
- No raw pointer access to task data

---

## References

- [Redox OS kernel context](https://gitlab.redox-os.org/redox-os/kernel/-/tree/master/src/context)
- [Linux sched/core.c](https://github.com/torvalds/linux/blob/master/kernel/sched/core.c)
- [Rust Atomics and Locks](https://marabos.nl/atomics/)
