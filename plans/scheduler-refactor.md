# Scheduler & Bridge Refactoring Plan

**Status:** Planning
**Author:** Claude (with kernel-architect guidance)
**Date:** 2026-01-10

---

## Executive Summary

SlopOS's scheduler subsystem has grown organically with significant technical debt: pervasive `static mut` globals, raw pointer manipulation throughout, a bridge pattern with 8 separate trait object storages, and mixed paradigms (traits + legacy function pointers). This plan proposes a systematic cleanup to improve type safety, reduce unsafe surface area, and consolidate scattered state while maintaining the existing scheduling semantics.

---

## Problem Statement

### Current Architecture (The Mess)

```
                           ┌─────────────────────────────┐
                           │     abi/sched_traits.rs     │
                           │  7 trait definitions        │
                           └──────────────┬──────────────┘
                                          │ implemented by
                                          ▼
                           ┌─────────────────────────────┐
                           │   sched/src/sched_impl.rs   │
                           │  SchedImpl (zero-sized)     │
                           │  implements all 4 sched     │
                           │  traits on single struct    │
                           └──────────────┬──────────────┘
                                          │ registered to
                                          ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                    drivers/src/sched_bridge.rs                              │
│  ─────────────────────────────────────────────────────────────────────────  │
│  static mut TIMING: Option<&'static dyn SchedulerTiming>                    │
│  static mut EXECUTION: Option<&'static dyn SchedulerExecution>              │
│  static mut STATE: Option<&'static dyn SchedulerState>                      │
│  static mut FATE: Option<&'static dyn SchedulerFate>                        │
│  static mut BOOT: Option<&'static dyn BootServices>                         │
│  static mut SCHED_FOR_BOOT: Option<&'static dyn SchedulerForBoot>           │
│  static mut CLEANUP_HOOK: Option<&'static dyn TaskCleanupHook>              │
│  static mut VIDEO_CLEANUP_FN: Option<fn(u32)>  ◄── legacy function pointer  │
└─────────────────────────────────────────────────────────────────────────────┘
                                          │
              ┌───────────────────────────┼───────────────────────────┐
              ▼                           ▼                           ▼
      ┌─────────────┐            ┌─────────────┐            ┌─────────────┐
      │  irq.rs     │            │  tty.rs     │            │  syscall_   │
      │  keyboard.rs│            │  ioapic.rs  │            │  handlers.rs│
      │             │            │             │            │             │
      │ 68 usages   │            │             │            │             │
      └─────────────┘            └─────────────┘            └─────────────┘
```

### Scheduler Internal State (Also a Mess)

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         sched/src/scheduler.rs                              │
│  ─────────────────────────────────────────────────────────────────────────  │
│  static mut SCHEDULER: Scheduler = Scheduler { ... }  ◄── 16 fields        │
│  static mut IDLE_WAKEUP_CB: Option<fn() -> c_int>     ◄── separate global  │
│                                                                             │
│  struct Scheduler {                                                         │
│      ready_queue: ReadyQueue,        // head/tail raw pointers              │
│      current_task: *mut Task,        // raw pointer                         │
│      idle_task: *mut Task,           // raw pointer                         │
│      return_context: TaskContext,    // inline 200-byte struct              │
│      ... 12 more fields                                                     │
│  }                                                                          │
└─────────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────────┐
│                           sched/src/task.rs                                 │
│  ─────────────────────────────────────────────────────────────────────────  │
│  static mut TASK_MANAGER: TaskManager = TaskManager::new()                  │
│                                                                             │
│  struct TaskManager {                                                       │
│      tasks: [Task; MAX_TASKS],       // 32 tasks inline                     │
│      exit_records: [TaskExitRecord; MAX_TASKS],                             │
│      num_tasks: u32,                                                        │
│      next_task_id: u32,                                                     │
│      ... stats counters                                                     │
│  }                                                                          │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Critical Issues

| Issue | Severity | Location |
|-------|----------|----------|
| 8 separate `static mut` trait storages in bridge | HIGH | `drivers/src/sched_bridge.rs` |
| 140+ `unsafe` blocks in scheduler crate | HIGH | `sched/src/*.rs` |
| Raw `*mut Task` pointers everywhere | HIGH | `scheduler.rs`, `task.rs` |
| `static mut SCHEDULER` with 16 fields | HIGH | `sched/src/scheduler.rs` |
| `static mut TASK_MANAGER` with task array | HIGH | `sched/src/task.rs` |
| Mixed traits + legacy function pointer | MEDIUM | `VIDEO_CLEANUP_FN` |
| No registration validation | MEDIUM | Bridge returns defaults silently |
| `TaskHandle = *mut c_void` loses type info | MEDIUM | `abi/sched_traits.rs` |
| Ready queue uses raw linked list pointers | MEDIUM | `scheduler.rs` |
| Duplicate scheduler subset for boot | LOW | `SchedulerForBoot` trait |

### Affected Files

**Bridge Layer:**
- `drivers/src/sched_bridge.rs` - 271 lines, 8 static muts, 68 usages across codebase
- `abi/src/sched_traits.rs` - 154 lines, 7 traits

**Scheduler Core:**
- `sched/src/scheduler.rs` - 700+ lines, `static mut SCHEDULER`
- `sched/src/task.rs` - 700+ lines, `static mut TASK_MANAGER`
- `sched/src/sched_impl.rs` - 155 lines, trait implementations
- `sched/src/ffi_boundary.rs` - Assembly FFI for context switch

**Callers (68 usages of sched_bridge::):**
- `drivers/src/irq.rs` - Timer tick, post-IRQ, panic
- `drivers/src/tty.rs` - Task blocking for input
- `drivers/src/keyboard.rs` - Reschedule on keypress
- `drivers/src/syscall_handlers.rs` - All syscall implementations
- `boot/src/idt.rs` - Exception handlers
- `sched/src/scheduler.rs` - GDT RSP0 updates

---

## Research: How Others Solve This

### Linux Kernel

Linux uses per-CPU scheduler state:

| Concept | Linux | SlopOS Current |
|---------|-------|----------------|
| Current task | `current` macro (per-CPU) | `static mut SCHEDULER.current_task` |
| Run queue | `struct rq` per-CPU | Single `ReadyQueue` |
| Task list | Intrusive linked list | Array `[Task; 32]` |
| Blocking | Wait queues | Direct state mutation |

Key insight: Linux's `current` is a zero-cost per-CPU variable, not a function call through trait indirection.

### Redox OS (Rust)

Redox uses `spin::Once` and careful interior mutability:

```rust
static CONTEXTS: RwLock<Vec<Arc<RwLock<Context>>>> = ...;
static CONTEXT_ID: AtomicUsize = ...;
```

Key insight: `Arc<RwLock<T>>` for task ownership, avoiding raw pointers.

### Theseus OS (Rust)

Theseus uses cell-based patterns:

```rust
pub static TASKLIST: Once<MutexIrqSafe<TaskList>> = Once::new();
```

Key insight: Single initialization with `Once`, then lock-based access.

---

## Proposed Architecture

### Design Goals

1. **Consolidate bridge state** - Single struct instead of 8 separate statics
2. **Type-safe task handles** - Replace `*mut c_void` with typed wrapper
3. **Reduce unsafe surface** - Encapsulate raw pointers in safe abstractions
4. **Interior mutability** - Replace `static mut` with proper synchronization
5. **Eliminate legacy APIs** - Remove function pointer fallbacks
6. **Validate registration** - Panic early on missing registrations

### New Architecture

```
                           ┌─────────────────────────────┐
                           │     abi/sched_traits.rs     │
                           │  Consolidated trait +       │
                           │  TaskRef type               │
                           └──────────────┬──────────────┘
                                          │
                           ┌──────────────┴──────────────┐
                           │                             │
                           ▼                             ▼
              ┌─────────────────────────┐   ┌─────────────────────────┐
              │   sched/src/core.rs     │   │   boot/src/services.rs  │
              │  SchedulerCore impl     │   │   BootServices impl     │
              └──────────────┬──────────┘   └──────────────┬──────────┘
                             │                             │
                             └──────────────┬──────────────┘
                                            ▼
                           ┌─────────────────────────────┐
                           │  drivers/src/sched_bridge.rs│
                           │  ───────────────────────────│
                           │  static BRIDGE: Once<Bridge>│
                           │                             │
                           │  struct Bridge {            │
                           │    sched: &'static dyn ...  │
                           │    boot: &'static dyn ...   │
                           │  }                          │
                           └─────────────────────────────┘
```

---

## Detailed Design

### Phase 1: Consolidate Bridge State

**Goal:** Replace 8 separate `static mut Option<...>` with single `Once<Bridge>`.

```rust
// drivers/src/sched_bridge.rs - AFTER

use spin::Once;

struct Bridge {
    sched: &'static dyn SchedulerServices,
    boot: &'static dyn BootServices,
}

static BRIDGE: Once<Bridge> = Once::new();

pub fn init(sched: &'static dyn SchedulerServices, boot: &'static dyn BootServices) {
    BRIDGE.call_once(|| Bridge { sched, boot });
}

fn bridge() -> &'static Bridge {
    BRIDGE.get().expect("sched_bridge not initialized")
}

// All wrappers become simple delegations
pub fn timer_tick() {
    bridge().sched.timer_tick();
}
```

**Benefits:**
- Single initialization point
- Panics immediately if used before init (fail-fast)
- No more `unsafe` for every call
- Removes 7 static muts

### Phase 2: Type-Safe Task Handles

**Goal:** Replace `TaskHandle = *mut c_void` with typed wrapper.

```rust
// abi/src/sched_traits.rs - AFTER

/// Type-safe task reference. Cannot be dereferenced outside sched crate.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TaskRef(NonNull<()>);

impl TaskRef {
    pub const NULL: Self = unsafe { Self(NonNull::new_unchecked(1 as *mut ())) };
    
    pub fn is_null(self) -> bool {
        self.0.as_ptr() as usize == 1
    }
    
    /// Create from raw pointer. Only callable from sched crate.
    pub(crate) fn from_ptr(ptr: *mut Task) -> Self {
        match NonNull::new(ptr as *mut ()) {
            Some(nn) => Self(nn),
            None => Self::NULL,
        }
    }
    
    /// Convert back to raw pointer. Only callable from sched crate.
    pub(crate) fn as_ptr<T>(self) -> *mut T {
        if self.is_null() { core::ptr::null_mut() } else { self.0.as_ptr() as *mut T }
    }
}
```

**Benefits:**
- Type system prevents misuse
- NULL is explicit, not ambiguous 0
- Conversion only in sched crate

### Phase 3: Consolidate Scheduler Traits

**Goal:** Merge 4 scheduler traits into 1, remove boot duplication.

**Current:** 7 traits (SchedulerTiming, SchedulerExecution, SchedulerState, SchedulerFate, BootServices, SchedulerForBoot, TaskCleanupHook)

**After:** 3 traits
- `SchedulerServices` - All scheduler operations (timing + execution + state + fate)
- `BootServices` - Kernel services (unchanged)
- `CleanupHook` - Task cleanup (unchanged, but trait-only)

```rust
pub trait SchedulerServices: Send + Sync {
    // Timing
    fn timer_tick(&self);
    fn handle_post_irq(&self);
    fn request_reschedule(&self);
    
    // Execution
    fn current_task(&self) -> TaskRef;
    fn yield_cpu(&self);
    fn schedule(&self);
    fn terminate(&self, task_id: u32) -> Result<(), SchedError>;
    fn block_current(&self);
    fn unblock(&self, task: TaskRef) -> Result<(), SchedError>;
    
    // State
    fn is_enabled(&self) -> bool;
    fn is_preemption_enabled(&self) -> bool;
    fn task_stats(&self) -> TaskStats;
    fn scheduler_stats(&self) -> SchedulerStats;
    fn register_idle_wakeup(&self, cb: Option<fn() -> bool>);
    
    // Fate
    fn fate_spin(&self) -> FateResult;
    fn fate_set_pending(&self, res: FateResult, task_id: u32) -> Result<(), SchedError>;
    fn fate_take_pending(&self, task_id: u32) -> Option<FateResult>;
    fn fate_apply_outcome(&self, res: &FateResult, resolution: u32, award: bool);
}
```

**Benefits:**
- Single trait object instead of 4
- Removes `SchedulerForBoot` (was subset duplication)
- Removes legacy function pointer API

### Phase 4: Interior Mutability for Scheduler State

**Goal:** Replace `static mut SCHEDULER` with safe pattern.

```rust
// sched/src/scheduler.rs - AFTER

use spin::{Mutex, Once};

struct SchedulerInner {
    ready_queue: ReadyQueue,
    current_task: Option<TaskRef>,
    idle_task: Option<TaskRef>,
    stats: SchedulerStats,
    config: SchedulerConfig,
}

static SCHEDULER: Once<Mutex<SchedulerInner>> = Once::new();

fn with_scheduler<R>(f: impl FnOnce(&mut SchedulerInner) -> R) -> R {
    let mut guard = SCHEDULER.get().expect("scheduler not initialized").lock();
    f(&mut guard)
}

pub fn schedule() {
    with_scheduler(|sched| {
        // ... scheduling logic
    })
}
```

**Note:** Context switching still needs raw pointer manipulation for register save/restore. The goal is to minimize the unsafe surface, not eliminate it entirely.

### Phase 5: Safe Task Storage

**Goal:** Replace `static mut TASK_MANAGER` with safe pattern.

```rust
// sched/src/task.rs - AFTER

use spin::Mutex;

struct TaskSlot {
    task: Option<Task>,
    exit_record: Option<TaskExitRecord>,
}

struct TaskManager {
    slots: [Mutex<TaskSlot>; MAX_TASKS],
    next_id: AtomicU32,
    stats: AtomicTaskStats,
}

static TASKS: Once<TaskManager> = Once::new();

pub fn find_task(id: u32) -> Option<TaskRef> {
    let manager = TASKS.get()?;
    for slot in &manager.slots {
        let guard = slot.lock();
        if let Some(ref task) = guard.task {
            if task.task_id == id {
                return Some(TaskRef::from_ptr(task as *const _ as *mut _));
            }
        }
    }
    None
}
```

**Trade-off:** Per-slot locks vs single lock. Per-slot allows concurrent access to different tasks but adds complexity. Start with single `Mutex<TaskManager>` and optimize if needed.

---

## Migration Plan

### Phase 1: Consolidate Bridge (Non-Breaking)

**Tasks:**
1. Create `Bridge` struct in `sched_bridge.rs`
2. Add `Once<Bridge>` alongside existing statics
3. Add new `init()` function
4. Migrate callers one-by-one to use consolidated bridge
5. Remove old static muts after all callers migrated

**Files to modify:**
- `drivers/src/sched_bridge.rs`
- `sched/src/sched_impl.rs` (update registration)
- `boot/src/boot_impl.rs` (update registration)

### Phase 2: Type-Safe TaskRef (Breaking)

**Tasks:**
1. Add `TaskRef` type to `abi/src/sched_traits.rs`
2. Update trait signatures to use `TaskRef`
3. Update `sched_impl.rs` implementations
4. Update all callers (68 usages)
5. Remove `TaskHandle` type alias

**Files to modify:**
- `abi/src/sched_traits.rs`
- `drivers/src/sched_bridge.rs`
- `sched/src/sched_impl.rs`
- All 15+ caller files

### Phase 3: Trait Consolidation (Breaking)

**Tasks:**
1. Create `SchedulerServices` trait
2. Implement on `SchedImpl`
3. Update bridge to use single trait
4. Remove old traits
5. Remove `SchedulerForBoot`

**Files to modify:**
- `abi/src/sched_traits.rs`
- `drivers/src/sched_bridge.rs`
- `sched/src/sched_impl.rs`
- `boot/src/idt.rs` (uses `SchedulerForBoot`)

### Phase 4: Scheduler Interior Mutability

**Tasks:**
1. Create `SchedulerInner` struct
2. Add `Once<Mutex<SchedulerInner>>`
3. Create `with_scheduler` helper
4. Migrate all scheduler functions
5. Remove `static mut SCHEDULER`

**Files to modify:**
- `sched/src/scheduler.rs`

### Phase 5: Task Manager Interior Mutability

**Tasks:**
1. Create safe `TaskManager` with `Mutex`
2. Create accessor functions
3. Migrate all task functions
4. Remove `static mut TASK_MANAGER`

**Files to modify:**
- `sched/src/task.rs`

---

## Testing Strategy

### Existing Tests

The scheduler has integration tests via `make test`:
- Boot verification
- Task creation/termination
- Context switching
- Interrupt handling

### New Tests Needed

1. **Bridge initialization** - Verify panic on use before init
2. **TaskRef safety** - Verify NULL handling, type safety
3. **Concurrent access** - Verify mutex doesn't deadlock in scheduler
4. **Preemption** - Verify timer-based preemption still works

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Deadlock in scheduler mutex | Medium | Critical | Careful lock ordering, no nested locks |
| Performance regression from locks | Low | Medium | Benchmark critical paths, use spinlocks |
| Breaking interrupt-context code | Medium | High | Context switch path stays raw pointers |
| Incomplete migration | Medium | Medium | Phase-by-phase approach |

---

## Success Criteria

1. **Bridge consolidated** - Single `Once<Bridge>` instead of 8 statics
2. **Type-safe handles** - `TaskRef` replaces `*mut c_void`
3. **Reduced unsafe** - 50%+ reduction in unsafe blocks
4. **No static mut** - All scheduler state uses interior mutability
5. **All tests pass** - `make test` succeeds
6. **Boot verified** - Kernel boots and runs normally

---

## Deferred Items

These are explicitly out of scope for this refactor:

1. **Per-CPU scheduler state** - Would require significant arch changes
2. **Priority-based scheduling** - Algorithm change, not cleanup
3. **Wait queues** - Blocking abstraction improvement
4. **SMP support** - Multi-core scheduling

---

## References

- [Redox OS Scheduler](https://gitlab.redox-os.org/redox-os/kernel/-/tree/master/src/scheme)
- [Theseus OS Task Management](https://github.com/theseus-os/Theseus/tree/main/kernel/task)
- [Linux CFS Scheduler](https://www.kernel.org/doc/html/latest/scheduler/sched-design-CFS.html)
- [spin crate documentation](https://docs.rs/spin)

---

## Appendix: Current Unsafe Inventory

| File | Unsafe Blocks | Primary Reason |
|------|---------------|----------------|
| `scheduler.rs` | 65 | Static mut access, raw pointers |
| `task.rs` | 45 | Static mut access, raw pointers |
| `sched_bridge.rs` | 30 | Static mut trait objects |
| `test_tasks.rs` | 18 | Test harness, raw pointers |
| `fate_api.rs` | 2 | Static mut access |
| `ffi_boundary.rs` | 4 | Assembly FFI |
| **Total** | **164** | |

**Target:** Reduce to ~20-30 (FFI + context switch only)
