# Rust-Native SMP Safety Plan

## Current Status (January 2026)

### Implementation State

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1a | IrqRwLock in lib | ✅ COMPLETED |
| Phase 1b | task_lock.rs with type aliases | ✅ COMPLETED |
| Phase 1c | TaskHandle safe wrapper | ✅ COMPLETED |
| Phase 2 | TaskStatus/BlockReason enums in abi | ✅ COMPLETED |
| Phase 3 | Type-safe state transitions | ✅ COMPLETED |
| Phase 4 | Memory barriers in unblock_task | ✅ COMPLETED |
| Phase 5 | Memory barriers in schedule_task | ✅ COMPLETED |

### Test Results

```
TESTS SUMMARY: total=363 passed=363 failed=0
```

### Boot Behavior

| Path | Result |
|------|--------|
| LOSE | ✅ Works - kernel reboots correctly |
| WIN | ❌ CRASHES - page fault at invalid kernel address |

---

## Revived Phases (January 2026 Update)

The previously cancelled phases have been successfully implemented. The key enabler was the introduction of `SwitchContext` and `switch_asm.rs` which uses compile-time `offset_of!` macros instead of hardcoded assembly offsets.

### Why It's Now Safe

1. **`SwitchContext`** is a separate, minimal struct (72 bytes) used only for callee-saved registers
2. The switch code uses `offset_of!()` which auto-adjusts to struct layout
3. Adding new fields to `Task` won't affect the switch as long as `SwitchContext` is accessed via `offset_of!`

### What Was Implemented

#### Phase 2: TaskStatus and BlockReason Enums

**File**: `abi/src/task.rs`

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Invalid = 0,
    Ready = 1,
    Running = 2,
    Blocked = 3,
    Terminated = 4,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockReason {
    #[default]
    None = 0,
    WaitingOnTask = 1,
    Sleep = 2,
    IoWait = 3,
    MutexWait = 4,
    KeyboardWait = 5,
    IpcWait = 6,
    Generic = 7,
}
```

#### Phase 3: Type-Safe State Transitions

**File**: `abi/src/task.rs` (Task impl)

```rust
impl Task {
    pub fn status(&self) -> TaskStatus { ... }
    pub fn set_status(&mut self, status: TaskStatus) { ... }
    pub fn try_transition_to(&mut self, target: TaskStatus) -> bool { ... }
    pub fn mark_ready(&mut self) -> bool { ... }
    pub fn mark_running(&mut self) -> bool { ... }
    pub fn block(&mut self, reason: BlockReason) -> bool { ... }
    pub fn terminate(&mut self) -> bool { ... }
    pub fn is_blocked(&self) -> bool { ... }
    pub fn is_ready(&self) -> bool { ... }
    pub fn is_running(&self) -> bool { ... }
    pub fn is_terminated(&self) -> bool { ... }
}
```

#### Phase 1c: TaskHandle Safe Wrapper

**File**: `core/src/scheduler/task_lock.rs`

```rust
pub struct TaskHandle<'a> {
    task: &'a mut Task,
}

impl<'a> TaskHandle<'a> {
    pub fn new(task: &'a mut Task) -> Option<Self> { ... }
    pub fn status(&self) -> TaskStatus { ... }
    pub fn block_reason(&self) -> BlockReason { ... }
    pub fn mark_ready(&mut self) -> bool { ... }
    pub fn mark_running(&mut self) -> bool { ... }
    pub fn block(&mut self, reason: BlockReason) -> bool { ... }
    pub fn terminate(&mut self) -> bool { ... }
}
```

#### New API: task_set_state_with_reason

**File**: `core/src/scheduler/task.rs`

```rust
pub fn task_set_state_with_reason(
    task_id: u32,
    new_status: TaskStatus,
    reason: BlockReason
) -> c_int { ... }
```

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

## Previously Cancelled (Now Resolved)

### ABI Crate Modification Issue - RESOLVED

The original hypothesis was that ABI modifications caused test failures due to hardcoded assembly offsets. This was correct - the old `context_switch.s` used byte offsets like `0x00`, `0x08`, `0x80` that would break if struct layout changed.

#### Solution

The introduction of `switch_asm.rs` with `offset_of!` macros eliminated this fragility:

```rust
// switch_asm.rs - uses offset_of! for struct access
"mov [rdi + {off_rbx}], rbx",
...
off_rbx = const offset_of!(SwitchContext, rbx),
```

Now struct layout changes are caught at compile time rather than causing silent corruption at runtime.

#### Previously Cancelled Phases - NOW COMPLETED

| Phase | Status |
|-------|--------|
| 1c | ✅ TaskHandle safe wrapper implemented |
| 2a | ✅ TaskStatus enum added to abi |
| 2b | ✅ BlockReason enum added to abi |
| 2c | ✅ block_reason field added to Task struct |
| 3a | ✅ mark_ready(), mark_running() etc. methods |
| 3b | Not needed - compile-time validation via enum |

---

## Files Modified

| File | Changes |
|------|---------|
| `lib/src/spinlock.rs` | Added IrqRwLock, IrqRwLockReadGuard, IrqRwLockWriteGuard |
| `lib/src/lib.rs` | Export new lock types |
| `abi/src/task.rs` | TaskStatus enum, BlockReason enum, block_reason field, Task methods |
| `core/src/scheduler/task_lock.rs` | Type aliases + TaskHandle safe wrapper |
| `core/src/scheduler/mod.rs` | Added task_lock module |
| `core/src/scheduler/scheduler.rs` | Memory barriers in unblock_task and schedule_task |
| `core/src/scheduler/per_cpu.rs` | Self-pointer sanity checks |
| `core/src/scheduler/task.rs` | Exports, task_set_state_with_reason(), updated transition logic |

---

## Next Steps for Investigation

### Priority 1: WIN Path Page Fault

The crash changed from syscall argument corruption to a page fault at an invalid address. Need to investigate:

1. Why is RIP jumping to `0xffffffff80180128` (past `_user_text_start`)?
2. Is there corruption in the task context during AP pickup?
3. Is CR3/page table incorrect when AP starts executing?
4. Is there a race between BSP context save and AP context restore?

### Priority 2: Incremental Migration to Type-Safe APIs

Now that the type-safe infrastructure is in place, gradually migrate callers:

1. Replace `task_set_state(id, TASK_STATE_BLOCKED)` with `task_set_state_with_reason(id, TaskStatus::Blocked, BlockReason::WaitingOnTask)`
2. Use `TaskHandle` wrapper in new code for safer task access
3. Eventually convert run queues to use `TaskRef = Arc<IrqRwLock<Task>>`

### Priority 3: Full TaskRef Migration (Future)

The ultimate goal is Redox OS-style task locking:

```rust
pub fn unblock_and_schedule(task_ref: &TaskRef) -> Result<(), Error> {
    let mut guard = task_ref.write();
    guard.mark_ready()?;
    enqueue_to_cpu(target_cpu, task_ref.clone());
    drop(guard);
    send_ipi(target_cpu);
    Ok(())
}
```

This requires converting the static task array to `Arc`-based allocation.

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
