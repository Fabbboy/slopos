# Rust-Native SMP Safety Plan

## Overview

This document outlines a comprehensive plan to leverage Rust's type system and ownership model to eliminate the race conditions currently causing boot crashes in SlopOS's SMP scheduler. The design is inspired by **Redox OS** (Rust-native kernel) and **Linux kernel** synchronization patterns, adapted for SlopOS's architecture.

### Problem Statement

The current scheduler has a critical race condition:
1. When roulette task terminates, it unblocks shell/compositor tasks
2. `release_task_dependents()` calls `unblock_task()` which calls `schedule_task()`
3. `schedule_task()` enqueues to an AP's queue and sends IPI
4. AP wakes up and dequeues the task **before memory writes are visible**
5. AP attempts context switch with corrupted `context.rip` -> **PAGE FAULT**

**Root Cause**: Task state and context can be modified without holding any lock, and there are no memory barriers ensuring cross-CPU visibility.

### Design Philosophy

> "If you need a lock to access data, the type system should require you to hold that lock."

The key insight from Redox OS: wrap `Task` in `RwLock<Task>`, so you **cannot** read or write task fields without holding the lock. Rust's borrow checker enforces this at compile time.

---

## Table of Contents

1. [Current Architecture Problems](#1-current-architecture-problems)
2. [Target Architecture](#2-target-architecture)
3. [Phase 1: TaskLock Wrapper](#phase-1-tasklock-wrapper)
4. [Phase 2: Status Enum](#phase-2-status-enum)
5. [Phase 3: Type-Safe State Transitions](#phase-3-type-safe-state-transitions)
6. [Phase 4: Atomic Unblock-and-Schedule](#phase-4-atomic-unblock-and-schedule)
7. [Phase 5: Switch Holds Guards](#phase-5-switch-holds-guards)
8. [Phase 6: Migration Strategy](#phase-6-migration-strategy)
9. [Testing Plan](#testing-plan)
10. [Risk Assessment](#risk-assessment)

---

## 1. Current Architecture Problems

### 1.1 Unprotected Task Access

```rust
// Current: Anyone can read/write task fields through raw pointer
pub fn task_find_by_id(task_id: u32) -> *mut Task {
    // Returns raw pointer - no synchronization!
}

pub fn task_set_state(task_id: u32, new_state: u8) -> c_int {
    let task = task_find_by_id(task_id);
    // Lock acquired briefly, then released
    unsafe { (*task).state = new_state };  // Write OUTSIDE lock!
}
```

### 1.2 No Memory Barriers

```rust
// bootstrap.rs - writes context.rip without barrier
ptr::write_unaligned(ptr::addr_of_mut!((*task_info).context.rip), new_entry);
// NO fence here - AP may not see this write!

// schedule_task enqueues immediately
schedule_task(task_info);  // AP can pick up task with stale context
```

### 1.3 Split Unblock/Schedule

```rust
// task.rs - release_task_dependents
if scheduler::unblock_task(*dep) != 0 { ... }
// unblock_task does: set_state(READY) + schedule_task()
// But between these, another CPU could access the task!
```

### 1.4 Comparison: How Redox Does It

```rust
// Redox: Task wrapped in RwLock
pub type ContextLock = RwLock<L2, Context>;

// You MUST hold the lock to access task fields
let guard = context_lock.write();  // Acquire lock
guard.status = Status::Runnable;   // Modify under lock
// Lock released when guard drops - implicit memory barrier
```

---

## 2. Target Architecture

### 2.1 Core Principles

1. **Task wrapped in lock**: `Arc<IrqRwLock<Task>>` protects all task data
2. **Status is enum**: No magic u8 constants, exhaustive matching
3. **State transitions require `&mut Task`**: Only possible with write guard
4. **Schedule requires guard**: `schedule_task(guard)` not `schedule_task(ptr)`
5. **Context switch holds both guards**: Like Redox's `SwitchResultInner`

### 2.2 Type Hierarchy

```
┌─────────────────────────────────────────────────────────────────────┐
│                        NEW TYPE HIERARCHY                           │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  TaskRef = Arc<TaskLock>                                            │
│      │                                                              │
│      └── TaskLock = IrqRwLock<Task>                                 │
│              │                                                      │
│              ├── .read()  -> TaskReadGuard<'_>   (shared access)    │
│              │                                                      │
│              └── .write() -> TaskWriteGuard<'_>  (exclusive access) │
│                      │                                              │
│                      └── Can modify task.status, task.context, etc. │
│                                                                     │
│  TaskStatus = enum { Invalid, Ready, Running{cpu}, Blocked{reason}, │
│                      Terminated{code} }                             │
│                                                                     │
│  BlockReason = enum { WaitingOn(task_id), Sleep(time), Io, ... }    │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 2.3 Synchronization Flow

```
SAFE UNBLOCK + SCHEDULE FLOW:

    release_task_dependents(completed_task_id)
           │
           ▼
    ┌──────────────────────────────────────┐
    │  for each dependent task_ref:        │
    │    let mut guard = task_ref.write(); │  ◄── Acquire exclusive lock
    │    guard.unblock()?;                 │  ◄── State: BLOCKED -> READY
    │    let target_cpu = select_cpu();    │
    │    enqueue_to_cpu(target_cpu,        │
    │                   task_ref.clone()); │  ◄── Enqueue while holding lock
    │    drop(guard);                      │  ◄── Release + memory barrier
    │    if target_cpu != current_cpu {    │
    │        send_ipi(target_cpu);         │  ◄── Wake AP AFTER barrier
    │    }                                 │
    └──────────────────────────────────────┘

AP RECEIVES IPI:
           │
           ▼
    ┌──────────────────────────────────────┐
    │  let task_ref = dequeue();           │
    │  let guard = task_ref.write();       │  ◄── Memory barrier on acquire
    │  // Now guaranteed to see all writes │
    │  assert!(guard.context.rip >= 0x400000);
    │  do_context_switch(guard);           │
    └──────────────────────────────────────┘
```

---

## Phase 1: TaskLock Wrapper

**Goal**: Wrap `Task` in `IrqRwLock` so all access requires holding a lock.

### 1.1 New Types

**File**: `abi/src/task.rs` (add near existing Task)

```rust
// Re-export for convenience
pub use core::sync::atomic::{AtomicU8, Ordering};

// Task status as atomic for lock-free reads (optimization)
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Invalid = 0,
    Ready = 1,
    Running = 2,
    Blocked = 3,
    Terminated = 4,
}

impl TaskStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Blocked,
            4 => Self::Terminated,
            _ => Self::Invalid,
        }
    }
}
```

**File**: `core/src/scheduler/task_lock.rs` (new file)

```rust
//! Task locking primitives for SMP-safe task access.
//!
//! This module provides the `TaskLock` type that wraps `Task` in an `IrqRwLock`,
//! ensuring all task access is properly synchronized across CPUs.

use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};
use slopos_abi::task::Task;
use slopos_lib::IrqRwLock;

/// A reference-counted, locked task.
/// 
/// This is the primary way to reference tasks in SlopOS. The inner `Task`
/// can only be accessed by acquiring the lock, which provides:
/// - Mutual exclusion for writers
/// - Memory barriers on lock acquire/release
/// - Compile-time enforcement of synchronization
pub type TaskRef = Arc<TaskLock>;

/// A task protected by an IRQ-safe read-write lock.
pub type TaskLock = IrqRwLock<Task>;

/// Guard for read access to a task.
pub type TaskReadGuard<'a> = slopos_lib::IrqRwLockReadGuard<'a, Task>;

/// Guard for write access to a task.
pub type TaskWriteGuard<'a> = slopos_lib::IrqRwLockWriteGuard<'a, Task>;

/// Create a new TaskRef from a Task.
pub fn new_task_ref(task: Task) -> TaskRef {
    Arc::new(IrqRwLock::new(task))
}

/// Attempt to acquire a write guard, returning None if would block.
pub fn try_write(task_ref: &TaskRef) -> Option<TaskWriteGuard<'_>> {
    task_ref.try_write()
}

/// Acquire a write guard, spinning if necessary.
pub fn write(task_ref: &TaskRef) -> TaskWriteGuard<'_> {
    task_ref.write()
}

/// Acquire a read guard.
pub fn read(task_ref: &TaskRef) -> TaskReadGuard<'_> {
    task_ref.read()
}
```

### 1.2 Task Manager Changes

**File**: `core/src/scheduler/task.rs`

```rust
// Change task storage from array of Task to array of Option<TaskRef>
struct TaskManagerInner {
    tasks: [Option<TaskRef>; MAX_TASKS],  // Changed from [Task; MAX_TASKS]
    // ... rest unchanged
}

// Change task_find_by_id to return TaskRef
pub fn task_find_by_id(task_id: u32) -> Option<TaskRef> {
    if task_id == INVALID_TASK_ID {
        return None;
    }
    with_task_manager(|mgr| {
        for task_opt in mgr.tasks.iter() {
            if let Some(task_ref) = task_opt {
                let guard = task_ref.read();
                if guard.task_id == task_id {
                    return Some(Arc::clone(task_ref));
                }
            }
        }
        None
    })
}
```

### 1.3 Tasks

- [ ] Create `core/src/scheduler/task_lock.rs` with `TaskRef`, `TaskLock` types
- [ ] Add `IrqRwLock` to `slopos_lib` if not present (or use existing)
- [ ] Change `TaskManagerInner.tasks` from `[Task; MAX_TASKS]` to `[Option<TaskRef>; MAX_TASKS]`
- [ ] Update `task_find_by_id` to return `Option<TaskRef>`
- [ ] Update `task_create` to return `TaskRef`
- [ ] Audit all `task_find_by_id` call sites

### 1.4 Compatibility Layer

During migration, provide both old and new APIs:

```rust
// Deprecated: Returns raw pointer for legacy code
#[deprecated(note = "Use task_find_by_id_ref instead")]
pub fn task_find_by_id(task_id: u32) -> *mut Task {
    task_find_by_id_ref(task_id)
        .map(|r| {
            // SAFETY: Caller must ensure proper synchronization
            // This is only for migration - remove after full migration
            Arc::as_ptr(&r) as *mut Task
        })
        .unwrap_or(ptr::null_mut())
}

// New: Returns TaskRef
pub fn task_find_by_id_ref(task_id: u32) -> Option<TaskRef> {
    // ... implementation
}
```

---

## Phase 2: Status Enum

**Goal**: Replace `u8` state constants with a proper enum that enables exhaustive matching.

### 2.1 Status Types

**File**: `abi/src/task.rs`

```rust
/// Task execution status.
/// 
/// This enum represents all valid states a task can be in.
/// Using an enum instead of u8 constants provides:
/// - Exhaustive pattern matching (compiler warns on missing cases)
/// - Invalid states are unrepresentable
/// - Self-documenting code
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    /// Slot is unused
    Invalid,
    
    /// Ready to run, waiting in a queue
    Ready,
    
    /// Currently executing on a CPU
    Running { cpu: u8 },
    
    /// Waiting for some condition
    Blocked { reason: BlockReason },
    
    /// Finished execution
    Terminated { exit_code: u32 },
}

/// Reason a task is blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// Waiting for another task to terminate
    WaitingOnTask { task_id: u32 },
    
    /// Sleeping until a specific time
    Sleep { wake_time: u64 },
    
    /// Waiting for I/O
    Io,
    
    /// Waiting for a mutex/semaphore
    Sync,
    
    /// Stopped by debugger/signal
    Stopped,
}

impl TaskStatus {
    pub fn is_runnable(&self) -> bool {
        matches!(self, Self::Ready)
    }
    
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
    
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
    
    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::Terminated { .. })
    }
    
    /// Convert to legacy u8 for compatibility during migration.
    pub fn to_legacy_u8(&self) -> u8 {
        match self {
            Self::Invalid => 0,
            Self::Ready => 1,
            Self::Running { .. } => 2,
            Self::Blocked { .. } => 3,
            Self::Terminated { .. } => 4,
        }
    }
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Invalid
    }
}
```

### 2.2 Task Struct Update

**File**: `abi/src/task.rs`

```rust
#[repr(C)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    
    // OLD: pub state: u8,
    // NEW:
    pub status: TaskStatus,
    
    pub priority: u8,
    // ... rest unchanged
    
    // Remove: pub waiting_on_task_id: u32,
    // (now encoded in BlockReason::WaitingOnTask)
}
```

### 2.3 Migration Helper

```rust
impl Task {
    /// Get legacy state for compatibility.
    #[deprecated]
    pub fn state(&self) -> u8 {
        self.status.to_legacy_u8()
    }
    
    /// Set state from legacy u8.
    #[deprecated]
    pub fn set_state_legacy(&mut self, state: u8) {
        self.status = match state {
            1 => TaskStatus::Ready,
            2 => TaskStatus::Running { cpu: 0 },
            3 => TaskStatus::Blocked { reason: BlockReason::Io },
            4 => TaskStatus::Terminated { exit_code: 0 },
            _ => TaskStatus::Invalid,
        };
    }
}
```

### 2.4 Tasks

- [ ] Add `TaskStatus` enum to `abi/src/task.rs`
- [ ] Add `BlockReason` enum
- [ ] Replace `state: u8` with `status: TaskStatus` in `Task` struct
- [ ] Remove `waiting_on_task_id` field (now in `BlockReason`)
- [ ] Add migration helpers (`to_legacy_u8`, `set_state_legacy`)
- [ ] Update all state checks to use enum matching
- [ ] Remove `TASK_STATE_*` constants after migration

---

## Phase 3: Type-Safe State Transitions

**Goal**: State transitions can only happen through methods that require `&mut Task`.

### 3.1 Transition Methods

**File**: `core/src/scheduler/task.rs`

```rust
impl Task {
    /// Attempt to mark this task as ready.
    /// 
    /// # Returns
    /// - `Ok(())` if transition was valid (from Blocked or newly created)
    /// - `Err(InvalidTransition)` if task cannot become ready
    pub fn mark_ready(&mut self) -> Result<(), InvalidTransition> {
        match self.status {
            TaskStatus::Blocked { .. } => {
                self.status = TaskStatus::Ready;
                Ok(())
            }
            TaskStatus::Invalid => {
                // New task being initialized
                self.status = TaskStatus::Ready;
                Ok(())
            }
            _ => Err(InvalidTransition {
                from: self.status,
                to: TaskStatus::Ready,
            }),
        }
    }
    
    /// Attempt to mark this task as running on a CPU.
    /// 
    /// # Returns
    /// - `Ok(())` if transition was valid (from Ready)
    /// - `Err(InvalidTransition)` if task cannot run
    pub fn mark_running(&mut self, cpu: u8) -> Result<(), InvalidTransition> {
        match self.status {
            TaskStatus::Ready => {
                self.status = TaskStatus::Running { cpu };
                Ok(())
            }
            _ => Err(InvalidTransition {
                from: self.status,
                to: TaskStatus::Running { cpu },
            }),
        }
    }
    
    /// Attempt to block this task.
    /// 
    /// # Returns
    /// - `Ok(())` if transition was valid (from Ready or Running)
    /// - `Err(InvalidTransition)` if task cannot be blocked
    pub fn mark_blocked(&mut self, reason: BlockReason) -> Result<(), InvalidTransition> {
        match self.status {
            TaskStatus::Ready | TaskStatus::Running { .. } => {
                self.status = TaskStatus::Blocked { reason };
                Ok(())
            }
            _ => Err(InvalidTransition {
                from: self.status,
                to: TaskStatus::Blocked { reason },
            }),
        }
    }
    
    /// Attempt to terminate this task.
    /// 
    /// # Returns
    /// - `Ok(())` if transition was valid
    /// - `Err(InvalidTransition)` if already terminated/invalid
    pub fn mark_terminated(&mut self, exit_code: u32) -> Result<(), InvalidTransition> {
        match self.status {
            TaskStatus::Invalid | TaskStatus::Terminated { .. } => {
                Err(InvalidTransition {
                    from: self.status,
                    to: TaskStatus::Terminated { exit_code },
                })
            }
            _ => {
                self.status = TaskStatus::Terminated { exit_code };
                Ok(())
            }
        }
    }
}

/// Error returned when a state transition is invalid.
#[derive(Debug)]
pub struct InvalidTransition {
    pub from: TaskStatus,
    pub to: TaskStatus,
}
```

### 3.2 State Machine Diagram

```
                    ┌─────────────────────────────────────────────┐
                    │            TASK STATE MACHINE               │
                    └─────────────────────────────────────────────┘

     ┌─────────┐         mark_ready()         ┌─────────┐
     │ Invalid │ ───────────────────────────► │  Ready  │
     └─────────┘                              └────┬────┘
          ▲                                        │
          │ mark_terminated()                      │ mark_running(cpu)
          │ + cleanup                              ▼
     ┌────┴──────┐                           ┌─────────┐
     │Terminated │ ◄──────────────────────── │ Running │
     └───────────┘     mark_terminated()     └────┬────┘
          ▲                                       │
          │                                       │ mark_blocked(reason)
          │              mark_ready()             ▼
          └────────────────────────────────  ┌─────────┐
                                             │ Blocked │
                                             └─────────┘

     VALID TRANSITIONS:
     - Invalid -> Ready (task_create)
     - Ready -> Running (context switch to task)
     - Running -> Ready (preemption/yield)
     - Running -> Blocked (wait/sleep/io)
     - Blocked -> Ready (wakeup/unblock)
     - Any -> Terminated (exit/kill)
     - Terminated -> Invalid (slot reuse)
```

### 3.3 Tasks

- [ ] Add `mark_ready()`, `mark_running()`, `mark_blocked()`, `mark_terminated()` methods
- [ ] Add `InvalidTransition` error type
- [ ] Update `task_set_state()` to use new methods
- [ ] Remove direct `task.status = ...` writes outside Task impl
- [ ] Add state transition logging for debugging

---

## Phase 4: Atomic Unblock-and-Schedule

**Goal**: Unblock + schedule is a single atomic operation that holds the lock throughout.

### 4.1 New unblock_and_schedule Function

**File**: `core/src/scheduler/scheduler.rs`

```rust
/// Unblock a task and schedule it atomically.
/// 
/// This function:
/// 1. Acquires exclusive lock on the task
/// 2. Transitions state from Blocked -> Ready
/// 3. Enqueues to target CPU's queue (while still holding lock)
/// 4. Releases lock (implicit memory barrier)
/// 5. Sends IPI if needed (after barrier ensures visibility)
/// 
/// # Arguments
/// * `task_ref` - Reference to the task to unblock
/// 
/// # Returns
/// * `Ok(())` - Task was unblocked and scheduled
/// * `Err(...)` - Task was not blocked or other error
pub fn unblock_and_schedule(task_ref: &TaskRef) -> Result<(), UnblockError> {
    // Step 1: Acquire exclusive lock
    let mut guard = task_ref.write();
    
    // Step 2: Validate and transition state
    guard.mark_ready().map_err(|e| UnblockError::InvalidState(e))?;
    
    // Step 3: Select target CPU and enqueue (still holding lock!)
    let target_cpu = select_target_cpu_for_task(&guard);
    let current_cpu = slopos_lib::get_current_cpu();
    
    // Clone the TaskRef for the queue (Arc clone is cheap)
    let task_ref_for_queue = Arc::clone(task_ref);
    
    // Step 4: Enqueue to per-CPU queue
    per_cpu::enqueue_task_ref(target_cpu, task_ref_for_queue)?;
    
    // Step 5: Release lock (implicit memory barrier via lock release)
    drop(guard);
    
    // Step 6: Send IPI AFTER lock release (now writes are visible)
    if target_cpu != current_cpu && slopos_lib::is_cpu_online(target_cpu) {
        send_reschedule_ipi(target_cpu);
    }
    
    Ok(())
}

#[derive(Debug)]
pub enum UnblockError {
    InvalidState(InvalidTransition),
    QueueFull,
    InvalidCpu,
}
```

### 4.2 Update release_task_dependents

**File**: `core/src/scheduler/task.rs`

```rust
fn release_task_dependents(completed_task_id: u32) {
    // Collect dependent TaskRefs (not raw pointers!)
    let dependents: Vec<TaskRef> = with_task_manager(|mgr| {
        let mut result = Vec::new();
        for task_opt in mgr.tasks.iter() {
            if let Some(task_ref) = task_opt {
                let guard = task_ref.read();
                if let TaskStatus::Blocked { reason: BlockReason::WaitingOnTask { task_id } } = guard.status {
                    if task_id == completed_task_id {
                        result.push(Arc::clone(task_ref));
                    }
                }
            }
        }
        result
    });
    
    // Unblock each dependent atomically
    for task_ref in dependents {
        match scheduler::unblock_and_schedule(&task_ref) {
            Ok(()) => {
                let guard = task_ref.read();
                klog_debug!("Unblocked dependent task {}", guard.task_id);
            }
            Err(e) => {
                let guard = task_ref.read();
                klog_debug!("Failed to unblock task {}: {:?}", guard.task_id, e);
            }
        }
    }
}
```

### 4.3 Per-CPU Queue with TaskRef

**File**: `core/src/scheduler/per_cpu.rs`

```rust
use super::task_lock::TaskRef;

struct ReadyQueue {
    // Change from *mut Task to TaskRef
    tasks: Vec<TaskRef>,
    lock: SpinLock<()>,
}

impl ReadyQueue {
    pub fn enqueue(&self, task: TaskRef) -> Result<(), ()> {
        let _guard = self.lock.lock();
        if self.tasks.len() >= MAX_QUEUE_SIZE {
            return Err(());
        }
        self.tasks.push(task);
        Ok(())
    }
    
    pub fn dequeue(&self) -> Option<TaskRef> {
        let _guard = self.lock.lock();
        // Dequeue highest priority task
        // (simplified - actual impl would sort by priority)
        self.tasks.pop()
    }
}

pub fn enqueue_task_ref(cpu_id: usize, task: TaskRef) -> Result<(), UnblockError> {
    with_cpu_scheduler(cpu_id, |sched| {
        let priority = {
            let guard = task.read();
            guard.priority as usize
        };
        sched.ready_queues[priority].enqueue(task)
            .map_err(|_| UnblockError::QueueFull)
    }).ok_or(UnblockError::InvalidCpu)?
}
```

### 4.4 Tasks

- [ ] Create `unblock_and_schedule()` function
- [ ] Update `release_task_dependents()` to use new function
- [ ] Change per-CPU queues to store `TaskRef` instead of `*mut Task`
- [ ] Add queue lock to per-CPU queues
- [ ] Update `dequeue_highest_priority()` to return `TaskRef`
- [ ] Remove old `unblock_task()` after migration

---

## Phase 5: Switch Holds Guards

**Goal**: Context switch holds write guards on both tasks throughout the switch.

### 5.1 Switch Context Structure

**File**: `core/src/scheduler/scheduler.rs`

```rust
use core::cell::Cell;

/// Holds task guards across a context switch.
/// 
/// This is stored in per-CPU data and keeps the guards alive
/// from when we start the switch until `switch_finish_hook` is called.
/// 
/// Like Redox's `SwitchResultInner`, this ensures:
/// - Both tasks are locked during the switch
/// - No other CPU can touch these tasks while switching
/// - Memory barriers on guard release ensure visibility
struct SwitchGuards {
    prev_guard: Option<TaskWriteGuard<'static>>,
    next_guard: TaskWriteGuard<'static>,
}

// Per-CPU storage for switch guards
// SAFETY: Only accessed by the owning CPU during context switch
thread_local! {
    static SWITCH_GUARDS: Cell<Option<SwitchGuards>> = Cell::new(None);
}
```

### 5.2 New Context Switch Flow

```rust
fn do_context_switch_safe(
    prev_task: Option<&TaskRef>,
    next_task: &TaskRef,
    preempt_guard: PreemptGuard,
) {
    // Step 1: Acquire guards on both tasks
    let prev_guard = prev_task.map(|t| {
        // SAFETY: Guard lifetime extended via Cell storage
        unsafe { core::mem::transmute::<TaskWriteGuard<'_>, TaskWriteGuard<'static>>(t.write()) }
    });
    
    let mut next_guard = unsafe {
        core::mem::transmute::<TaskWriteGuard<'_>, TaskWriteGuard<'static>>(next_task.write())
    };
    
    // Step 2: Validate next task before switch
    assert!(
        next_guard.status.is_runnable(),
        "Cannot switch to non-ready task"
    );
    assert!(
        next_guard.context.rip >= 0x400000 || next_guard.context.rip == 0,
        "Invalid RIP 0x{:x} for task {}",
        next_guard.context.rip,
        next_guard.task_id
    );
    
    // Step 3: Update states
    if let Some(ref mut prev) = prev_guard {
        if prev.status.is_running() {
            prev.status = TaskStatus::Ready;
        }
    }
    next_guard.status = TaskStatus::Running { cpu: slopos_lib::get_current_cpu() as u8 };
    
    // Step 4: Store guards to keep alive across switch
    SWITCH_GUARDS.with(|cell| {
        cell.set(Some(SwitchGuards {
            prev_guard,
            next_guard,
        }));
    });
    
    // Step 5: Do the actual switch
    // The guards remain held - we'll release them in switch_finish_hook
    unsafe {
        let prev_ctx = prev_task.map(|t| &raw mut (*Arc::as_ptr(t)).context);
        let next_ctx = &raw const (*Arc::as_ptr(next_task)).context;
        
        if is_user_mode(next_task) {
            context_switch_user(prev_ctx.unwrap_or(ptr::null_mut()), next_ctx);
        } else {
            context_switch(prev_ctx.unwrap_or(ptr::null_mut()), next_ctx);
        }
    }
    
    // NOTE: After switch, we return here with different stack!
    // switch_finish_hook will release the guards
}

/// Called after context switch completes to release guards.
/// 
/// # Safety
/// Must only be called from assembly context switch return path.
#[no_mangle]
pub unsafe extern "C" fn switch_finish_hook() {
    SWITCH_GUARDS.with(|cell| {
        // Take and drop the guards, releasing the locks
        let _ = cell.take();
    });
}
```

### 5.3 Assembly Integration

Update `context_switch.s` to call `switch_finish_hook`:

```asm
context_switch:
    # ... save old context ...
    # ... load new context ...
    
    # Before returning to new task, call finish hook
    call switch_finish_hook
    
    retq
```

### 5.4 Tasks

- [ ] Create `SwitchGuards` structure
- [ ] Add per-CPU storage for switch guards (thread_local or per-CPU array)
- [ ] Implement `do_context_switch_safe()` that holds both guards
- [ ] Add RIP validation before switch
- [ ] Create `switch_finish_hook()` to release guards
- [ ] Update assembly to call `switch_finish_hook`
- [ ] Test: Verify locks are held during switch

---

## Phase 6: Migration Strategy

### 6.1 Migration Phases

```
┌──────────────────────────────────────────────────────────────────────┐
│                      MIGRATION PHASES                                 │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  PHASE 1: TaskLock                                                   │
│  ├── Add TaskLock, TaskRef types                                     │
│  ├── Change TaskManager storage                                      │
│  ├── Provide compatibility shim for raw pointers                     │
│  └── Tests pass with shim                                            │
│           │                                                          │
│           ▼                                                          │
│  PHASE 2: Status Enum                                                │
│  ├── Add TaskStatus enum                                             │
│  ├── Update Task struct                                              │
│  ├── Add migration helpers                                           │
│  └── Gradual callsite migration                                      │
│           │                                                          │
│           ▼                                                          │
│  PHASE 3: State Transitions                                          │
│  ├── Add mark_* methods                                              │
│  ├── Update all state changes to use methods                         │
│  ├── Remove direct state writes                                      │
│  └── Tests verify transitions                                        │
│           │                                                          │
│           ▼                                                          │
│  PHASE 4: Atomic Unblock                                             │
│  ├── Implement unblock_and_schedule()                                │
│  ├── Change queues to use TaskRef                                    │
│  ├── Update release_task_dependents                                  │
│  └── Boot test: WIN path works                                       │
│           │                                                          │
│           ▼                                                          │
│  PHASE 5: Switch Guards                                              │
│  ├── Add SwitchGuards storage                                        │
│  ├── Implement safe context switch                                   │
│  ├── Update assembly                                                 │
│  └── Full SMP boot test                                              │
│           │                                                          │
│           ▼                                                          │
│  CLEANUP: Remove Deprecated Code                                     │
│  ├── Remove raw pointer APIs                                         │
│  ├── Remove legacy state constants                                   │
│  ├── Remove compatibility shims                                      │
│  └── Final audit                                                     │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

### 6.2 Compatibility During Migration

During migration, maintain both APIs:

```rust
// DEPRECATED: Will be removed
pub fn task_set_state(task_id: u32, new_state: u8) -> c_int;
pub fn task_find_by_id(task_id: u32) -> *mut Task;
pub fn unblock_task(task: *mut Task) -> c_int;

// NEW: Use these
pub fn task_find_by_id_ref(task_id: u32) -> Option<TaskRef>;
pub fn unblock_and_schedule(task_ref: &TaskRef) -> Result<(), UnblockError>;
```

### 6.3 Feature Flag

Use a feature flag to enable new code paths:

```toml
# Cargo.toml
[features]
default = []
safe-smp = []  # Enable Rust-native SMP safety
```

```rust
#[cfg(feature = "safe-smp")]
pub fn unblock_task(task_ref: &TaskRef) -> Result<(), UnblockError> {
    unblock_and_schedule(task_ref)
}

#[cfg(not(feature = "safe-smp"))]
pub fn unblock_task(task: *mut Task) -> c_int {
    // Old implementation
}
```

---

## Testing Plan

### Unit Tests

```rust
#[test]
fn test_task_status_transitions() {
    let task_ref = new_task_ref(Task::new());
    let mut guard = task_ref.write();
    
    // Invalid -> Ready
    assert!(guard.mark_ready().is_ok());
    assert!(guard.status.is_runnable());
    
    // Ready -> Running
    assert!(guard.mark_running(0).is_ok());
    assert!(guard.status.is_running());
    
    // Running -> Blocked
    assert!(guard.mark_blocked(BlockReason::Io).is_ok());
    assert!(guard.status.is_blocked());
    
    // Blocked -> Ready
    assert!(guard.mark_ready().is_ok());
    
    // Invalid transition: Ready -> Blocked (should fail, need Running first)
    // Actually this is valid per our state machine... adjust test
}

#[test]
fn test_unblock_and_schedule_atomic() {
    let task_ref = new_task_ref(Task::new());
    
    // Set up blocked task
    {
        let mut guard = task_ref.write();
        guard.mark_ready().unwrap();
        guard.mark_running(0).unwrap();
        guard.mark_blocked(BlockReason::WaitingOnTask { task_id: 99 }).unwrap();
    }
    
    // Unblock should work
    assert!(unblock_and_schedule(&task_ref).is_ok());
    
    // Task should be ready
    let guard = task_ref.read();
    assert!(guard.status.is_runnable());
}

#[test]
fn test_concurrent_task_access() {
    let task_ref = new_task_ref(Task::new());
    
    // Spawn multiple threads trying to modify task
    // Verify no data races (would panic with debug assertions)
    std::thread::scope(|s| {
        for _ in 0..10 {
            let task = Arc::clone(&task_ref);
            s.spawn(move || {
                for _ in 0..100 {
                    let guard = task.write();
                    // Modify something
                    drop(guard);
                }
            });
        }
    });
}
```

### Integration Tests

```rust
#[test]
fn test_roulette_win_path() {
    // Create roulette, shell, compositor tasks
    // Block shell and compositor on roulette
    // Terminate roulette
    // Verify shell and compositor are scheduled without crash
}

#[test]
fn test_cross_cpu_unblock() {
    // Task blocked on CPU 0
    // Unblocked from CPU 1
    // Verify task executes correctly on target CPU
}
```

### Boot Tests

```bash
# Test with new code path
FEATURES=safe-smp make test
FEATURES=safe-smp VIDEO=1 make boot  # Win path should work
```

---

## Risk Assessment

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| Breaking existing tests during migration | High | Medium | Maintain compatibility shim, incremental migration |
| Deadlock from lock ordering issues | High | Low | Strict lock ordering: TaskManager > TaskLock > per-CPU queue |
| Performance regression from locking | Medium | Medium | Profile critical paths, consider RCU for reads |
| Incorrect lifetime handling with guards | High | Medium | Careful unsafe code review, Miri testing |
| Assembly/Rust FFI mismatch | High | Low | Thorough testing, keep assembly minimal |

### Lock Ordering

To prevent deadlocks, always acquire locks in this order:
1. `TaskManager` (global task list)
2. Individual `TaskLock`s (by task_id ascending)
3. Per-CPU queue locks (by CPU ID ascending)

```rust
// CORRECT: Lock task manager first, then individual task
let task_ref = with_task_manager(|mgr| {
    mgr.find_task(task_id)
})?;
let guard = task_ref.write();

// WRONG: Holding task lock while accessing task manager
let guard = task_ref.write();
with_task_manager(|mgr| { ... }); // DEADLOCK RISK!
```

---

## Success Criteria

1. **All 358+ tests pass** with `safe-smp` feature enabled
2. **Boot WIN path works** without page fault crash
3. **No deadlocks** under stress test (100 task create/terminate cycles)
4. **No data races** detected by Miri or similar tools
5. **Comparable performance** to current implementation (within 10%)
6. **Code compiles** with `#![forbid(unsafe_op_in_unsafe_fn)]`

---

## References

- [Redox OS kernel/src/context](https://gitlab.redox-os.org/redox-os/kernel/-/tree/master/src/context) - Rust-native context switching
- [Linux kernel/sched/core.c](https://github.com/torvalds/linux/blob/master/kernel/sched/core.c) - SMP scheduler reference
- [Rust Atomics and Locks](https://marabos.nl/atomics/) - Memory ordering in Rust
- [The Rustonomicon](https://doc.rust-lang.org/nomicon/) - Unsafe Rust guidelines
