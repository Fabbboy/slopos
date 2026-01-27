# SlopOS Known Issues

Last updated: 2026-01-27

---

## Fixed: AP (Application Processor) User-Mode Context Switch Failure

**Status**: Fixed (2026-01-27)  
**Severity**: High  
**Component**: `lib/src/pcr.rs`, `boot/src/smp.rs`, `core/context_switch.s`

### Description

CPU 1 (and other APs) could not safely execute user-mode tasks. When `ap_execute_task()` attempted to context switch to a user-mode task, a page fault occurred.

### Root Cause

Two critical bugs in the per-CPU infrastructure:

1. **AP TSS.rsp0 = 0**: APs never had their kernel stack pointer initialized in the TSS, so when interrupts/exceptions occurred in user mode, the CPU loaded RSP0=0 from TSS, causing page faults.

2. **Wrong KERNEL_GS_BASE for APs**: The `SYSCALL_CPU_DATA_PTR` global only contained CPU 0's per-CPU data pointer. When `context_switch_user` set up KERNEL_GS_BASE for SWAPGS, APs got the wrong pointer.

### Fix Applied

Implemented a Unified Processor Control Region (PCR) following Redox OS patterns:

1. Created `lib/src/pcr.rs` with `ProcessorControlRegion` struct containing embedded GDT, TSS, and per-CPU kernel stack
2. Updated BSP boot (`boot/src/early_init.rs`) to use PCR instead of old global arrays
3. Updated AP boot (`boot/src/smp.rs`) to allocate and initialize PCR per-AP
4. Updated SYSCALL assembly (`boot/idt_handlers.s`) to use new PCR offsets (8, 16 instead of 0, 8)
5. Updated `context_switch_user` (`core/context_switch.s`) to read KERNEL_GS_BASE from `gs:[0]` (PCR self_ref) instead of global
6. Removed the workaround in `select_target_cpu()` that forced user-mode tasks to CPU 0

### Verification

All 360 tests pass with user-mode tasks now running on any CPU.

### Related Files

- `lib/src/pcr.rs` - Unified ProcessorControlRegion structure
- `boot/src/smp.rs` - AP initialization using PCR
- `boot/src/early_init.rs` - BSP initialization using PCR
- `boot/idt_handlers.s` - SYSCALL entry/exit assembly
- `core/context_switch.s` - Context switch to user mode
- `core/src/scheduler/per_cpu.rs` - Removed user-mode workaround

---

## Fixed: Compositor Not Running After Roulette Win

**Status**: Fixed (2025-01-24)  
**Severity**: High  
**Component**: `core/src/scheduler`

### Description

After roulette terminated with a win, the compositor task was unblocked but never actually ran, leaving the roulette wheel visible on screen.

### Root Cause

When a task terminates, it:
1. Calls `pause_all_aps()` to pause AP scheduler loops
2. Calls `release_task_dependents()` which unblocks waiting tasks and enqueues them
3. Calls `resume_all_aps()` to unpause

The problem: tasks enqueued during step 2 sent IPIs to wake APs, but since APs were paused, they checked `are_aps_paused()`, saw `true`, and went back to HLT. After step 3 set the flag to `false`, the APs were already sleeping and never received another wake-up signal.

### Fix Applied

Modified `resume_all_aps()` to send IPIs to any CPU that has tasks in its ready queue:

```rust
pub fn resume_all_aps() {
    core::sync::atomic::fence(Ordering::SeqCst);
    AP_PAUSED.store(false, Ordering::SeqCst);

    // Wake up any APs that have pending tasks
    let cpu_count = slopos_lib::get_cpu_count();
    for cpu_id in 1..cpu_count {
        if let Some(count) = with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()) {
            if count > 0 {
                if let Some(apic_id) = slopos_lib::apic_id_from_cpu_index(cpu_id) {
                    slopos_lib::send_ipi_to_cpu(apic_id, RESCHEDULE_IPI_VECTOR);
                }
            }
        }
    }
}
```

### Related Files

- `core/src/scheduler/per_cpu.rs` - `resume_all_aps()`
- `core/src/scheduler/task.rs` - `task_terminate()`, `release_task_dependents()`

---

## Fixed: Single-Core Userland Bottleneck

**Status**: Fixed (2026-01-27)  
**Severity**: Medium  
**Component**: `core/src/scheduler`

### Description

Due to the AP user-mode context switch failure, all userland tasks were forced to run on CPU 0, creating a single-core bottleneck for user applications.

### Fix Applied

The AP user-mode context switch issue is now fixed (see above). User-mode tasks can now run on any CPU. The scheduler automatically distributes tasks across CPUs using `find_least_loaded_cpu()`.

---

## Performance: Compositor Frame Rate During Task Termination

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

When a task terminates, `pause_all_aps()` is called which blocks all AP scheduler loops. While this is necessary for safe task cleanup, it can cause brief stalls in compositor frame rendering if the compositor happens to be scheduled on an AP.

### Current Behavior

1. Task calls `task_terminate()`
2. `pause_all_aps()` sets `AP_PAUSED = true` and waits for APs to stop executing
3. `release_task_dependents()` unblocks waiting tasks
4. `resume_all_aps()` sets `AP_PAUSED = false` and sends wake IPIs

During steps 2-3, any task on an AP (including compositor) is paused.

### Impact

- Brief frame drops (1-2 frames) during task termination
- More noticeable with frequent task spawning/termination
- Currently mitigated by user-mode tasks running on CPU 0 only

### Potential Optimizations

1. **Fine-grained locking**: Instead of pausing all APs, use per-task locks
2. **RCU-style cleanup**: Defer task cleanup to a dedicated kernel thread
3. **Lock-free dependent release**: Use atomic operations instead of global pause

### Related Files

- `core/src/scheduler/task.rs` - `task_terminate()`
- `core/src/scheduler/per_cpu.rs` - `pause_all_aps()`, `resume_all_aps()`

---

## Performance: Scheduler Lock Contention

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

The scheduler uses a global `SCHEDULER` mutex that can cause contention when multiple CPUs try to schedule tasks simultaneously.

### Current Architecture

```
SCHEDULER (global IrqMutex)
├── ready_queues[4]     // Priority-based queues
├── current_task
├── idle_task
└── various counters

CPU_SCHEDULERS[MAX_CPUS] (per-CPU)
├── ready_queues[4]     // Local priority queues
├── current_task_atomic
└── queue_lock (per-CPU mutex)
```

### Contention Points

1. `schedule()` calls `with_scheduler()` which locks global mutex
2. `schedule_task()` may fall back to global queue if per-CPU enqueue fails
3. `select_next_task()` checks both per-CPU and global queues

### Impact

- Minor latency spikes under high task churn
- Not significant with current workloads (compositor + shell)
- Would become more noticeable with many concurrent tasks

### Potential Optimizations

1. **Fully per-CPU scheduling**: Eliminate global ready queue entirely
2. **Lock-free queues**: Use compare-and-swap for enqueue/dequeue
3. **Batch operations**: Coalesce multiple schedule operations

### Related Files

- `core/src/scheduler/scheduler.rs` - `SCHEDULER`, `with_scheduler()`
- `core/src/scheduler/per_cpu.rs` - `CPU_SCHEDULERS`

---

## Notes for Future Development

### SMP Architecture

The kernel now uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `lib/src/pcr.rs` and `plans/UNIFIED_PCR_IMPLEMENTATION.md` for architecture details.
