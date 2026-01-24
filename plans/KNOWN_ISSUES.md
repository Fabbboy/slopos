# SlopOS Known Issues

Last updated: 2025-01-24

---

## Critical: AP (Application Processor) User-Mode Context Switch Failure

**Status**: Open - Workaround in place  
**Severity**: High  
**Component**: `core/src/scheduler`

### Description

CPU 1 (and other APs) cannot safely execute user-mode tasks. When `ap_execute_task()` attempts to context switch to a user-mode task, a page fault occurs.

### Symptoms

```
AP_LOOP: CPU 1 dequeued task 3 (queue_had=1)
EXCEPTION: Vector 14 (Page Fault)
Fault address: 0xffffffff8016f38a
Error code: 0x11 (Page present) (Read) (Supervisor)
```

The fault happens in kernel space (address 0xffffffff...) during the context switch to user mode.

### Current Workaround

All user-mode tasks are forced to run on CPU 0. See `core/src/scheduler/per_cpu.rs`:

```rust
pub fn select_target_cpu(task: *mut Task) -> usize {
    // WORKAROUND: Force all user-mode tasks to CPU 0 until AP user-mode context switch is fixed
    let is_user_mode = unsafe { (*task).flags & slopos_abi::task::TASK_FLAG_USER_MODE != 0 };
    if is_user_mode {
        return 0;
    }
    // ... rest of load balancing logic
}
```

### Impact

- SMP is effectively disabled for userland workloads
- All user tasks run on CPU 0, reducing parallelism
- Kernel tasks can still use multiple CPUs

### Investigation Notes

The issue occurs in `ap_execute_task()` at `core/src/scheduler/scheduler.rs:1107+`. Key differences between BSP (CPU 0) and AP context switching:

1. BSP uses `schedule()` → `prepare_switch()` → `do_context_switch()`
2. APs use `ap_scheduler_loop()` → `ap_execute_task()` with direct context switch

Potential causes to investigate:
- GDT/TSS not properly initialized for APs
- Page tables not synchronized for user-mode on APs
- Kernel stack setup differences between BSP and APs
- SYSCALL MSRs not configured on APs
- Missing CR3 switch or incorrect page directory

### Related Files

- `core/src/scheduler/scheduler.rs` - `ap_scheduler_loop()`, `ap_execute_task()`
- `core/src/scheduler/per_cpu.rs` - `select_target_cpu()` workaround
- `boot/src/smp.rs` - AP initialization
- `boot/src/gdt.rs` - GDT/TSS setup

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

## Performance: Synchronous VirtIO-GPU Frame Flush

**Status**: Open  
**Severity**: Medium  
**Component**: `drivers/src/virtio_gpu.rs`, `video/src/framebuffer.rs`

### Description

Every frame presented by the compositor requires synchronous spin-waiting on VirtIO-GPU commands, causing significant CPU overhead and limiting frame rate to approximately 1-5 FPS in some configurations.

### Root Cause

The frame presentation path is fully synchronous:

1. **Memory Copy** (~8MB per frame for 1920x1080):
   ```rust
   // video/src/framebuffer.rs:366
   ptr::copy_nonoverlapping(shm_virt as *const u8, fb.base_ptr(), copy_size);
   ```

2. **VirtIO-GPU Transfer** (synchronous spin-wait):
   ```rust
   // drivers/src/virtio_gpu.rs:1036-1045
   virtio_gpu_transfer_to_host_2d(...)  // Spins up to 1,000,000 times
   virtio_gpu_resource_flush(...)        // Spins up to 1,000,000 times
   ```

3. **Spin-wait implementation**:
   ```rust
   // drivers/src/virtio/queue.rs:157-172
   pub fn poll_used(&mut self, timeout_spins: u32) -> bool {
       loop {
           fence(Ordering::SeqCst);
           let used_idx = self.read_used_idx();
           if used_idx != self.last_used_idx {
               return true;
           }
           spins += 1;
           if spins > timeout_spins { return false; }
           core::hint::spin_loop();
       }
   }
   ```

### Performance Impact

- Each frame requires 2 synchronous GPU commands
- Each command can spin up to `GPU_CMD_TIMEOUT_SPINS = 1,000,000` iterations
- Full-screen buffer copy of ~8MB per frame
- No double-buffering or async presentation
- CPU is blocked during GPU operations

### Potential Optimizations

1. **Async VirtIO Commands**:
   - Use interrupt-driven completion instead of polling
   - Submit multiple commands before waiting
   - Implement command batching

2. **Double/Triple Buffering**:
   - Maintain multiple framebuffers
   - Flip between buffers instead of copying
   - Allow GPU to scan out while CPU renders next frame

3. **Dirty Rectangle Tracking**:
   - Only transfer changed regions to GPU
   - Compositor already has damage tracking (`DamageTracker`)
   - Could use `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D` with smaller rects

4. **DMA for Memory Copy**:
   - Use DMA engine instead of CPU memcpy
   - Free CPU while transfer happens

5. **Reduce Polling Overhead**:
   - Use `hlt` instruction between polls to save power
   - Implement exponential backoff
   - Lower `GPU_CMD_TIMEOUT_SPINS` with interrupt fallback

### Related Files

- `drivers/src/virtio_gpu.rs` - GPU command execution, `virtio_gpu_flush_full()`
- `drivers/src/virtio/queue.rs` - `poll_used()` spin-wait
- `video/src/framebuffer.rs` - `fb_flip_from_shm()`, `framebuffer_flush()`
- `userland/src/compositor.rs` - Frame presentation loop

### Workaround

None currently. Frame rate is limited by GPU command latency and synchronous execution.

---

## Performance: Single-Core Userland Bottleneck

**Status**: Open - Blocked by AP user-mode issue  
**Severity**: Medium  
**Component**: `core/src/scheduler`

### Description

Due to the AP user-mode context switch failure (see above), all userland tasks are forced to run on CPU 0. This creates a single-core bottleneck for user applications.

### Impact

- Compositor, shell, and all user applications share CPU 0
- CPU 1+ sit idle for user workloads (only handle kernel tasks)
- No parallel execution of user processes
- Context switch overhead increases as more user tasks compete for CPU 0

### Metrics

With the current workaround:
- CPU 0: 100% of user task execution
- CPU 1+: ~0% user task execution (idle or kernel-only)

### Resolution

Fix the AP user-mode context switch issue. Once fixed, remove the workaround in `select_target_cpu()` and the scheduler will automatically distribute tasks using `find_least_loaded_cpu()`.

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

### Testing AP User-Mode Tasks

To test when attempting to fix the AP user-mode issue:

1. Remove the workaround in `select_target_cpu()`
2. Boot with `make boot-log`
3. Observe the page fault details
4. Use `addr2line` or similar to map RIP addresses to source locations

### SMP Task Distribution

Once the AP user-mode issue is fixed, the scheduler will naturally distribute tasks across CPUs using `find_least_loaded_cpu()`. No additional changes needed.
