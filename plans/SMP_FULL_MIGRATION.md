# SMP Full Migration Plan

## Overview

This document outlines the work required to fully migrate SlopOS from single-CPU scheduling to true symmetric multiprocessing where Application Processors (APs) can execute tasks independently.

### Current State

The SMP infrastructure exists but APs are idle:
- Per-CPU GDT/TSS: ✅ Implemented (`boot/src/gdt.rs`)
- Per-CPU scheduler data structures: ✅ Implemented (`core/src/scheduler/per_cpu.rs`)
- AP initialization and online detection: ✅ Implemented (`boot/src/smp.rs`)
- Work stealing infrastructure: ✅ Implemented (`core/src/scheduler/work_steal.rs`)
- Reschedule IPI: ✅ Implemented (vector 0xFC)
- **AP task execution: ❌ NOT IMPLEMENTED**

APs currently sit in `sti; hlt; cli` loop because they cannot do context switches safely.

### Why APs Cannot Execute Tasks Yet

1. **No AP idle task context**: APs need an idle task to switch FROM when picking up real work
2. **KERNEL_GS_BASE not per-CPU aware**: `context_switch_user` writes a global `SYSCALL_CPU_DATA_PTR`
3. **TSS.RSP0 not updated on AP context switch**: User→kernel transitions would use wrong stack
4. **No synchronization on per-CPU queues**: `get_cpu_scheduler()` returns `&'static mut` with no locking
5. **Global scheduler lock contention**: The `IrqMutex<SchedulerInner>` becomes a bottleneck

---

## Phase 1: Per-CPU Idle Task Infrastructure

**Goal**: Each AP gets its own idle task that it can switch away from.

### 1.1 Create Per-CPU Idle Tasks

```rust
// In per_cpu.rs - add idle task creation
pub fn create_percpu_idle_task(cpu_id: usize) -> *mut Task {
    // Allocate kernel stack for this CPU's idle task
    // Initialize TaskContext with idle_loop as entry point
    // Set task.last_cpu = cpu_id, cpu_affinity = (1 << cpu_id)
}

fn idle_loop() -> ! {
    loop {
        unsafe { core::arch::asm!("sti; hlt; cli"); }
    }
}
```

### 1.2 Initialize Idle Tasks During AP Startup

Modify `boot/src/smp.rs`:
```rust
unsafe extern "C" fn ap_entry(cpu_info: &MpCpu) -> ! {
    // ... existing init ...
    
    // Create idle task for this CPU
    let idle_task = per_cpu::create_percpu_idle_task(cpu_idx);
    per_cpu::with_cpu_scheduler(cpu_idx, |sched| {
        sched.set_idle_task(idle_task);
    });
    
    init_scheduler_for_ap(cpu_idx);
    scheduler_run_ap(cpu_idx);
}
```

### 1.3 Tasks

- [ ] Add `create_percpu_idle_task()` function
- [ ] Allocate dedicated kernel stacks for AP idle tasks
- [ ] Wire idle task creation into AP startup sequence
- [ ] Test: Verify each AP has an idle task pointer

---

## Phase 2: Per-CPU KERNEL_GS_BASE

**Goal**: Each CPU maintains its own syscall data pointer for SWAPGS.

### 2.1 Problem

`context_switch_user` in `context_switch.s` writes to `SYSCALL_CPU_DATA_PTR`:
```asm
movq SYSCALL_CPU_DATA_PTR(%rip), %rax
```

This is a single global variable. When AP does a user switch, it corrupts BSP's syscall path.

### 2.2 Solution

The KERNEL_GS_BASE MSR is already per-CPU (it's CPU-local). The issue is that we write a global variable instead of reading the CPU-local data.

**Option A**: Remove the global write, trust KERNEL_GS_BASE is already set correctly during AP init.

**Option B**: Read per-CPU data dynamically in assembly using the CPU index.

Recommended: **Option A** - The `syscall_gs_base_init_for_cpu()` already sets KERNEL_GS_BASE per CPU. We just need to ensure it stays valid across context switches.

### 2.3 Modify context_switch_user

```asm
# Instead of writing global SYSCALL_CPU_DATA_PTR, preserve KERNEL_GS_BASE
# It was already set correctly during CPU init and doesn't change per-task
# (all tasks on the same CPU use the same per-CPU syscall data)

# Remove these lines:
# movl $0xC0000102, %ecx
# movq SYSCALL_CPU_DATA_PTR(%rip), %rax
# ...
# wrmsr

# KERNEL_GS_BASE stays as initialized per-CPU
```

### 2.4 Tasks

- [ ] Audit `context_switch_user` KERNEL_GS_BASE handling
- [ ] Verify KERNEL_GS_BASE is set during `syscall_gs_base_init_for_cpu()`
- [ ] Remove global `SYSCALL_CPU_DATA_PTR` write from context_switch_user (or make it conditional)
- [ ] Test: User task runs correctly on AP after syscall

---

## Phase 3: Per-CPU TSS.RSP0 Management

**Goal**: When AP switches to a user task, TSS.RSP0 points to that task's kernel stack.

### 3.1 Problem

`gdt_set_kernel_rsp0_for_cpu()` exists but isn't called during AP context switches. When a user task traps to kernel on AP, it uses wrong RSP0.

### 3.2 Solution

Update TSS.RSP0 during every context switch to a user task:

```rust
// In scheduler.rs do_context_switch()
fn do_context_switch(info: SwitchInfo, _preempt_guard: PreemptGuard) {
    let cpu_id = get_current_cpu();
    
    // Update TSS.RSP0 for the new task's kernel stack
    let kernel_rsp = unsafe { (*info.new_task).kernel_stack_top };
    gdt_set_kernel_rsp0_for_cpu(cpu_id, kernel_rsp);
    syscall_update_kernel_rsp_for_cpu(cpu_id, kernel_rsp);
    
    // ... existing switch logic ...
}
```

### 3.3 Tasks

- [ ] Call `gdt_set_kernel_rsp0_for_cpu()` in `do_context_switch()`
- [ ] Call `syscall_update_kernel_rsp_for_cpu()` in `do_context_switch()`
- [ ] Ensure Task struct has `kernel_stack_top` field populated
- [ ] Test: User task on AP survives interrupt/syscall

---

## Phase 4: Per-CPU Queue Synchronization

**Goal**: Safe concurrent access to per-CPU scheduler queues.

### 4.1 Problem

`per_cpu.rs` has:
```rust
pub fn get_cpu_scheduler(cpu_id: usize) -> Option<&'static mut PerCpuScheduler>
```

This returns a mutable reference without synchronization. If CPU 0 steals from CPU 1 while CPU 1 modifies its queue, undefined behavior occurs.

### 4.2 Solution Options

**Option A: Per-Queue Spinlocks**
```rust
struct ReadyQueue {
    lock: SpinLock<()>,  // Or AtomicBool for try_lock
    head: *mut Task,
    tail: *mut Task,
    count: AtomicU32,
}
```

**Option B: Lock-Free Queue**
- Use atomic compare-and-swap for enqueue/dequeue
- More complex but better performance under contention

**Option C: Disable Work Stealing Initially**
- Simplest path to working SMP
- Tasks stay on their assigned CPU
- Add work stealing later with proper synchronization

Recommended: **Option C first, then Option A**

### 4.3 Tasks

- [ ] Add per-CPU queue lock (SpinLock or IrqMutex)
- [ ] Wrap queue operations with lock acquisition
- [ ] Update work stealing to acquire victim's lock
- [ ] Test: Stress test with multiple CPUs enqueuing/dequeuing

---

## Phase 5: AP Schedule Loop

**Goal**: APs pick up tasks from their local queue and execute them.

### 5.1 New scheduler_run_ap Implementation

```rust
pub fn scheduler_run_ap(cpu_id: usize) -> ! {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable());
    slopos_lib::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);
    
    // Get our idle task as the "current" task
    let idle_task = per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task)
        .expect("AP must have idle task");
    
    loop {
        // Check local queue for work
        let next_task = per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.dequeue_highest_priority()
        }).unwrap_or(ptr::null_mut());
        
        if !next_task.is_null() {
            // Switch from idle to real task
            ap_context_switch(cpu_id, idle_task, next_task);
            // When we return here, task yielded/blocked/terminated
            // Re-check queue
            continue;
        }
        
        // No work - try stealing (if enabled)
        if try_work_steal() {
            continue;
        }
        
        // Nothing to do - halt until IPI
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

fn ap_context_switch(cpu_id: usize, from: *mut Task, to: *mut Task) {
    // Update TSS.RSP0
    let kernel_rsp = unsafe { (*to).kernel_stack_top };
    gdt_set_kernel_rsp0_for_cpu(cpu_id, kernel_rsp);
    syscall_update_kernel_rsp_for_cpu(cpu_id, kernel_rsp);
    
    // Update per-CPU current task
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.current_task = to;
    });
    
    // Do the switch
    unsafe {
        if is_user_task(to) {
            context_switch_user(&mut (*from).context, &(*to).context);
        } else {
            context_switch(&mut (*from).context, &(*to).context);
        }
    }
}
```

### 5.2 Tasks

- [ ] Implement `ap_context_switch()` helper
- [ ] Rewrite `scheduler_run_ap()` to actually switch to tasks
- [ ] Handle task yield/block/terminate returning to idle
- [ ] Test: Kernel task runs on AP
- [ ] Test: User task runs on AP

---

## Phase 6: Task Routing to Per-CPU Queues

**Goal**: `schedule_task()` routes tasks to appropriate CPU queues.

### 6.1 Routing Strategy

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    // Determine target CPU
    let target_cpu = select_target_cpu(task);
    let current_cpu = get_current_cpu();
    
    // Set task state
    task_set_state(task.task_id, TASK_STATE_READY);
    
    // Enqueue to target CPU
    per_cpu::with_cpu_scheduler(target_cpu, |sched| {
        sched.enqueue_local(task);
    });
    
    // Wake target CPU if different
    if target_cpu != current_cpu {
        send_reschedule_ipi(target_cpu);
    }
    
    0
}
```

### 6.2 CPU Selection Logic

```rust
fn select_target_cpu(task: *mut Task) -> usize {
    let affinity = unsafe { (*task).cpu_affinity };
    let last_cpu = unsafe { (*task).last_cpu as usize };
    
    // Prefer last CPU if allowed (cache affinity)
    if affinity == 0 || (affinity & (1 << last_cpu)) != 0 {
        if is_cpu_online(last_cpu) {
            return last_cpu;
        }
    }
    
    // Find least loaded allowed CPU
    find_least_loaded_cpu(affinity)
}
```

### 6.3 Tasks

- [ ] Migrate `schedule_task()` to use per-CPU queues
- [ ] Implement CPU selection with affinity awareness
- [ ] Wire up reschedule IPI for cross-CPU wakeups
- [ ] Test: Task with affinity runs on correct CPU
- [ ] Test: Load balances across CPUs

---

## Phase 7: Global Queue Deprecation

**Goal**: Remove the global ready queue from SchedulerInner.

### 7.1 Migration Steps

1. Route all new tasks to per-CPU queues (Phase 6)
2. Drain global queue at boot (move existing tasks)
3. Remove global queue fields from SchedulerInner
4. Update `get_scheduler_stats()` to sum per-CPU counts

### 7.2 What Remains Global

- Task manager (task creation/termination)
- Scheduler enable/disable flag
- Statistics aggregation
- Idle wakeup callback

### 7.3 Tasks

- [ ] Add global queue drain function
- [ ] Remove ready_queues from SchedulerInner
- [ ] Update all stats collection
- [ ] Verify no code paths use global queue
- [ ] Test: Full boot with only per-CPU queues

---

## Phase 8: Testing & Validation

### 8.1 New Test Cases

```rust
// Scheduler tests to add:
fn test_ap_executes_kernel_task();
fn test_ap_executes_user_task();
fn test_cross_cpu_task_migration();
fn test_cpu_affinity_respected();
fn test_work_stealing_under_load();
fn test_reschedule_ipi_wakes_ap();
fn test_concurrent_enqueue_dequeue();
```

### 8.2 Stress Tests

- Create 100 tasks, verify distribution across CPUs
- Rapid task create/terminate cycles on multiple CPUs
- User tasks doing syscalls on AP
- Interrupt handling while AP runs user code

### 8.3 Tasks

- [ ] Add AP execution unit tests
- [ ] Add cross-CPU migration tests
- [ ] Add stress tests
- [ ] Verify all existing tests still pass
- [ ] Boot test with VIDEO=1, verify roulette on multi-CPU

---

## Implementation Order

```
Phase 1 ─────► Phase 2 ─────► Phase 3
(Idle Tasks)  (GS_BASE)     (TSS.RSP0)
                   │
                   ▼
              Phase 4 ─────► Phase 5 ─────► Phase 6
              (Locking)     (AP Loop)     (Routing)
                                              │
                                              ▼
                                         Phase 7 ─────► Phase 8
                                         (Remove Global) (Testing)
```

Phases 1-3 are prerequisites for any AP task execution.
Phase 4 is required before work stealing.
Phases 5-7 can be done incrementally.
Phase 8 runs continuously throughout.

---

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Deadlock from nested locks | High | Strict lock ordering, no nested per-CPU locks |
| Race in queue manipulation | High | Phase 4 synchronization before Phase 5 |
| TSS.RSP0 corruption | High | Phase 3 before any user tasks on AP |
| KERNEL_GS_BASE wrong | High | Phase 2 before any syscalls on AP |
| Performance regression | Medium | Benchmark before/after, optimize hot paths |
| Test coverage gaps | Medium | Continuous testing throughout |

---

## Success Criteria

1. `make test` passes with all 358+ tests
2. Boot completes with roulette rendering at normal speed
3. `htop` equivalent shows tasks on multiple CPUs
4. No deadlocks or panics under stress
5. Syscalls work correctly from tasks on any CPU
6. Work stealing moves tasks between CPUs when imbalanced
