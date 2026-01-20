# Multi-CPU (SMP) Support Implementation Plan

## Executive Summary

SlopOS currently has **partial SMP support**: CPUs are discovered and started via Limine's MP protocol, LAPIC/IOAPIC interrupts work, and TLB shootdown infrastructure exists. However, **Application Processors (APs) idle after boot** because the scheduler is single-CPU only. This plan details the path to production-grade multi-CPU support.

---

## Current State Analysis

### What Works ✅

| Component | Status | Location |
|-----------|--------|----------|
| CPU Discovery | Limine MP protocol | `boot/src/smp.rs` |
| AP Startup | GDT/IDT/APIC init per-AP | `boot/src/smp.rs:ap_entry()` |
| LAPIC Driver | Detection, enable, EOI, IPI | `drivers/src/apic.rs` |
| IOAPIC Driver | MADT parsing, IRQ routing | `drivers/src/ioapic.rs` |
| TLB Shootdown | Per-CPU state, IPI broadcast | `mm/src/tlb.rs` |
| Per-CPU Page Caches | Lock-free order-0 alloc | `mm/src/page_alloc.rs` |
| Synchronization | IrqMutex, Spinlock, RwLock | `lib/src/spinlock.rs` |
| CPU Index Tracking | APIC ID → CPU index mapping | `mm/src/tlb.rs` |

### What's Missing ❌

| Component | Gap | Impact |
|-----------|-----|--------|
| **Per-CPU Scheduler** | Single global `SchedulerInner` | All CPUs fight for one lock |
| **Per-CPU Run Queues** | One global queue set | No locality, no parallelism |
| **get_current_cpu()** | Returns hardcoded `0` | Per-CPU data broken |
| **Per-CPU GDT/TSS** | Single static TSS | Can't have per-CPU RSP0 |
| **Load Balancing** | None | Uneven CPU utilization |
| **CPU Affinity** | None | Can't pin tasks to CPUs |
| **Per-CPU Current Task** | Single `current_task` pointer | Race conditions |
| **AP Work Loop** | APs just `hlt` in loop | Wasted CPU cores |

### Critical Code Paths

```
boot/src/smp.rs:ap_entry()
  └── Currently: gdt_init() → idt_load() → apic::enable() → loop { pause() }
  └── Should:    gdt_init_ap() → idt_load() → apic::enable() → scheduler_run_ap()

core/src/scheduler/scheduler.rs:schedule()
  └── Currently: Acquires single SCHEDULER mutex, picks from global queues
  └── Should:    Access per-CPU scheduler, pick from local queue, steal if empty

mm/src/page_alloc.rs:get_current_cpu()
  └── Currently: return 0
  └── Should:    Read from GS-based per-CPU data or LAPIC ID lookup
```

---

## Implementation Phases

### Phase 1: Per-CPU Infrastructure Foundation (Priority: CRITICAL)

**Goal**: Establish reliable per-CPU data access before any scheduler changes.

#### 1.1 Implement `get_current_cpu()` Properly

**File**: `lib/src/cpu.rs` (new or extend existing)

```rust
// Option A: Via LAPIC ID (works immediately)
pub fn get_current_cpu() -> usize {
    let apic_id = apic::get_id();
    tlb::cpu_index_from_apic_id(apic_id).unwrap_or(0)
}

// Option B: Via GS-base per-CPU data (faster, requires setup)
pub fn get_current_cpu() -> usize {
    unsafe {
        let cpu_id: usize;
        core::arch::asm!(
            "mov {}, gs:[0]",  // Per-CPU data at GS:0
            out(reg) cpu_id,
            options(nostack, readonly)
        );
        cpu_id
    }
}
```

**Recommendation**: Start with Option A (LAPIC-based), migrate to Option B later for performance.

#### 1.2 Per-CPU Data Structure

**File**: `lib/src/percpu.rs` (new)

```rust
/// Per-CPU data structure - one instance per CPU, accessed via GS segment
#[repr(C, align(64))]  // Cache line aligned
pub struct PerCpuData {
    pub cpu_id: usize,
    pub apic_id: u32,
    pub current_task: *mut Task,
    pub kernel_stack_top: u64,
    pub preempt_count: u32,
    pub in_interrupt: bool,
    pub scheduler: *mut PerCpuScheduler,
    _pad: [u8; 16],  // Pad to 64 bytes
}

static mut PER_CPU_DATA: [PerCpuData; MAX_CPUS] = [...];

pub fn init_percpu_for_cpu(cpu_id: usize, apic_id: u32) {
    unsafe {
        PER_CPU_DATA[cpu_id].cpu_id = cpu_id;
        PER_CPU_DATA[cpu_id].apic_id = apic_id;
        // Set GS base to point to this CPU's data
        let addr = &PER_CPU_DATA[cpu_id] as *const _ as u64;
        cpu::write_msr(MSR_GS_BASE, addr);
    }
}
```

#### 1.3 Per-CPU GDT/TSS

**Current Issue**: Single static `KERNEL_TSS` and `GDT_TABLE` in `boot/src/gdt.rs`.

**File**: `boot/src/gdt.rs` modifications

```rust
// Change from single static to per-CPU array
static mut PER_CPU_TSS: [Tss64; MAX_CPUS] = [...];
static mut PER_CPU_GDT: [GdtLayout; MAX_CPUS] = [...];

pub fn gdt_init_for_cpu(cpu_id: usize) {
    // Initialize this CPU's GDT with its own TSS
    let tss_addr = unsafe { &PER_CPU_TSS[cpu_id] as *const _ as u64 };
    // ... set up GDT entries pointing to per-CPU TSS
}

pub fn gdt_set_kernel_rsp0_for_cpu(cpu_id: usize, rsp0: u64) {
    unsafe { PER_CPU_TSS[cpu_id].rsp0 = rsp0; }
}
```

**Deliverables Phase 1**:
- [ ] `get_current_cpu()` returns correct CPU index
- [ ] Per-CPU data structure initialized for all CPUs
- [ ] Per-CPU GDT/TSS for each CPU
- [ ] BSP and APs use their own GDT/TSS

---

### Phase 2: Per-CPU Scheduler Architecture (Priority: HIGH)

**Goal**: Each CPU has its own scheduler instance with local run queues.

#### 2.1 Per-CPU Scheduler Structure

**File**: `core/src/scheduler/per_cpu.rs` (new)

```rust
/// Per-CPU scheduler state
#[repr(C, align(64))]
pub struct PerCpuScheduler {
    pub cpu_id: usize,
    pub ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    pub current_task: *mut Task,
    pub idle_task: *mut Task,
    pub time_slice: u16,
    pub total_switches: u64,
    pub total_preemptions: u64,
    lock: AtomicBool,  // Per-CPU lock (mostly uncontended)
}

static mut CPU_SCHEDULERS: [PerCpuScheduler; MAX_CPUS] = [...];

impl PerCpuScheduler {
    pub fn schedule(&mut self) {
        // Only handles THIS CPU's tasks
        // No global lock needed for local operations
    }
    
    pub fn enqueue_local(&mut self, task: *mut Task) {
        // Add to this CPU's queue
    }
}
```

#### 2.2 Modify Task Structure for SMP

**File**: `abi/src/task.rs` additions

```rust
pub struct Task {
    // ... existing fields ...
    
    // SMP additions
    pub cpu_affinity: u32,      // Bitmask of allowed CPUs (0 = any)
    pub last_cpu: u8,           // Last CPU this task ran on (for locality)
    pub migration_count: u32,   // Stats: how often migrated
}
```

#### 2.3 Task Scheduling Flow (SMP)

```
schedule() [called on CPU N]
    │
    ├─► Lock per_cpu_scheduler[N]
    │
    ├─► If current_task exists and is RUNNING:
    │       Mark READY, enqueue to local queue
    │
    ├─► next = dequeue_highest_priority() from local queues
    │
    ├─► If next is NULL:
    │       next = work_steal()  // Try other CPUs
    │
    ├─► If next is still NULL:
    │       next = idle_task[N]
    │
    ├─► If next != current_task:
    │       context_switch(current, next)
    │
    └─► Unlock
```

#### 2.4 Global Task Creation/Assignment

**File**: `core/src/scheduler/scheduler.rs` modifications

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    let target_cpu = select_target_cpu(task);
    
    // If target is current CPU, enqueue locally
    let current_cpu = get_current_cpu();
    if target_cpu == current_cpu {
        with_local_scheduler(|sched| sched.enqueue_local(task))
    } else {
        // Cross-CPU enqueue requires the target's lock
        with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task))
    }
}

fn select_target_cpu(task: *mut Task) -> usize {
    let affinity = unsafe { (*task).cpu_affinity };
    let last_cpu = unsafe { (*task).last_cpu as usize };
    
    // 1. Honor affinity
    if affinity != 0 && (affinity & (1 << last_cpu)) != 0 {
        return last_cpu;  // Prefer last CPU if allowed
    }
    
    // 2. Find least loaded allowed CPU
    find_least_loaded_cpu(affinity)
}
```

**Deliverables Phase 2**:
- [ ] PerCpuScheduler structure implemented
- [ ] Task struct extended with SMP fields
- [ ] schedule() uses per-CPU scheduler
- [ ] Task creation assigns to appropriate CPU
- [ ] Basic CPU selection (least loaded)

---

### Phase 3: AP Activation & Work Loop (Priority: HIGH)

**Goal**: APs participate in scheduling instead of idling.

#### 3.1 AP Entry Modification

**File**: `boot/src/smp.rs`

```rust
unsafe extern "C" fn ap_entry(cpu_info: &MpCpu) -> ! {
    cpu::disable_interrupts();
    
    let cpu_id = /* assigned sequentially */;
    let apic_id = cpu_info.lapic_id;
    
    // Per-CPU setup
    gdt_init_for_cpu(cpu_id);
    idt_load();
    init_percpu_for_cpu(cpu_id, apic_id);
    apic::enable();
    
    // Initialize this CPU's scheduler
    init_scheduler_for_cpu(cpu_id);
    create_idle_task_for_cpu(cpu_id);
    
    cpu_info.extra.store(AP_STARTED_MAGIC, Ordering::Release);
    klog_info!("MP: CPU {} online, entering scheduler", cpu_id);
    
    cpu::enable_interrupts();
    
    // Enter the scheduler loop - never returns
    scheduler_run_ap(cpu_id);
}
```

#### 3.2 AP Scheduler Loop

**File**: `core/src/scheduler/scheduler.rs`

```rust
pub fn scheduler_run_ap(cpu_id: usize) -> ! {
    // Mark this CPU as ready to receive tasks
    mark_cpu_online(cpu_id);
    
    loop {
        // Try to run a task
        let ran_task = with_local_scheduler(|sched| {
            if let Some(task) = sched.dequeue_highest_priority() {
                sched.run_task(task);
                true
            } else {
                false
            }
        });
        
        if !ran_task {
            // No work - try work stealing
            if !try_work_steal(cpu_id) {
                // Still no work - halt until interrupt
                cpu::enable_interrupts();
                cpu::hlt();
                cpu::disable_interrupts();
            }
        }
    }
}
```

**Deliverables Phase 3**:
- [ ] AP entry initializes per-CPU scheduler
- [ ] AP enters scheduler loop after init
- [ ] Per-CPU idle tasks created
- [ ] APs halt efficiently when no work

---

### Phase 4: Load Balancing & Work Stealing (Priority: MEDIUM)

**Goal**: Distribute work evenly across CPUs.

#### 4.1 Work Stealing Algorithm

**File**: `core/src/scheduler/work_steal.rs` (new)

```rust
/// Attempt to steal work from another CPU
/// Returns true if a task was stolen and is now on the local queue
pub fn try_work_steal(cpu_id: usize) -> bool {
    let cpu_count = get_active_cpu_count();
    
    // Round-robin starting from a random offset to avoid thundering herd
    let start = (cpu_id + 1) % cpu_count;
    
    for i in 0..cpu_count {
        let victim = (start + i) % cpu_count;
        if victim == cpu_id {
            continue;
        }
        
        // Try to steal from victim's lowest priority queue (least urgent)
        if let Some(task) = try_steal_from_cpu(victim) {
            // Check affinity before stealing
            let affinity = unsafe { (*task).cpu_affinity };
            if affinity == 0 || (affinity & (1 << cpu_id)) != 0 {
                with_local_scheduler(|sched| sched.enqueue_local(task));
                return true;
            }
        }
    }
    false
}

fn try_steal_from_cpu(victim: usize) -> Option<*mut Task> {
    // Try lock - don't spin, just fail if busy
    with_cpu_scheduler_try(victim, |sched| {
        // Steal from the back of the lowest priority queue
        // (least likely to be needed soon)
        for queue in sched.ready_queues.iter_mut().rev() {
            if let Some(task) = queue.steal_from_tail() {
                return Some(task);
            }
        }
        None
    }).flatten()
}
```

#### 4.2 Periodic Load Balancing

**File**: `core/src/scheduler/load_balance.rs` (new)

```rust
/// Called periodically (e.g., every 100ms) from timer interrupt
pub fn periodic_load_balance() {
    let cpu_count = get_active_cpu_count();
    if cpu_count <= 1 {
        return;
    }
    
    // Calculate load imbalance
    let loads: Vec<u32> = (0..cpu_count)
        .map(|cpu| get_cpu_load(cpu))
        .collect();
    
    let avg_load = loads.iter().sum::<u32>() / cpu_count as u32;
    
    // Find most and least loaded
    let (max_cpu, max_load) = loads.iter().enumerate()
        .max_by_key(|(_, &l)| l).unwrap();
    let (min_cpu, min_load) = loads.iter().enumerate()
        .min_by_key(|(_, &l)| l).unwrap();
    
    // Migrate if imbalance > threshold (e.g., 25%)
    if max_load > avg_load * 5 / 4 && min_load < avg_load * 3 / 4 {
        migrate_task_between_cpus(max_cpu, min_cpu);
    }
}

fn get_cpu_load(cpu: usize) -> u32 {
    with_cpu_scheduler(cpu, |sched| {
        sched.ready_queues.iter().map(|q| q.count).sum()
    })
}
```

**Deliverables Phase 4**:
- [ ] Work stealing implemented
- [ ] Periodic load balancer
- [ ] Load imbalance detection
- [ ] Cross-CPU task migration

---

### Phase 5: CPU Affinity API (Priority: MEDIUM)

**Goal**: Allow tasks to specify CPU preferences.

#### 5.1 Affinity System Calls

**File**: `core/src/syscall/handlers.rs` additions

```rust
/// Set CPU affinity mask for a task
/// affinity_mask: bitmask of allowed CPUs (0 = any CPU)
pub fn sys_set_cpu_affinity(task_id: u32, affinity_mask: u32) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -ESRCH;
    }
    
    // Validate: at least one allowed CPU must be online
    let online_mask = get_online_cpu_mask();
    if affinity_mask != 0 && (affinity_mask & online_mask) == 0 {
        return -EINVAL;
    }
    
    unsafe {
        (*task).cpu_affinity = affinity_mask;
    }
    
    // If task is running on disallowed CPU, trigger migration
    let current_cpu = unsafe { (*task).last_cpu as usize };
    if affinity_mask != 0 && (affinity_mask & (1 << current_cpu)) == 0 {
        trigger_migration(task);
    }
    
    0
}

pub fn sys_get_cpu_affinity(task_id: u32) -> i32 {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -ESRCH;
    }
    unsafe { (*task).cpu_affinity as i32 }
}
```

**Deliverables Phase 5**:
- [ ] set_cpu_affinity() syscall
- [ ] get_cpu_affinity() syscall
- [ ] Migration on affinity violation
- [ ] Userland wrapper functions

---

### Phase 6: IPI-Based Cross-CPU Coordination (Priority: MEDIUM)

**Goal**: Efficient cross-CPU communication for scheduling events.

#### 6.1 Reschedule IPI

**File**: `drivers/src/apic.rs` additions

```rust
pub const IPI_RESCHEDULE_VECTOR: u8 = 0xFC;

pub fn send_reschedule_ipi(target_cpu: usize) {
    let target_apic_id = cpu_to_apic_id(target_cpu);
    send_ipi_to_cpu(target_apic_id, IPI_RESCHEDULE_VECTOR);
}
```

**File**: `boot/src/idt.rs` additions

```rust
// Handler for reschedule IPI
extern "x86-interrupt" fn reschedule_ipi_handler(_frame: InterruptStackFrame) {
    apic::send_eoi();
    // Just sets the reschedule pending flag
    PreemptGuard::set_reschedule_pending();
}
```

#### 6.2 Cross-CPU Task Wake

```rust
pub fn wake_task_on_cpu(task: *mut Task, target_cpu: usize) {
    // Enqueue to target CPU's queue
    with_cpu_scheduler(target_cpu, |sched| {
        sched.enqueue_local(task);
    });
    
    // Send IPI to wake target CPU if it's halted
    if is_cpu_idle(target_cpu) {
        send_reschedule_ipi(target_cpu);
    }
}
```

**Deliverables Phase 6**:
- [ ] Reschedule IPI vector and handler
- [ ] Cross-CPU task wake with IPI
- [ ] Idle CPU detection

---

## Testing Strategy

### Phase 1 Tests: Per-CPU Infrastructure

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_get_current_cpu_bsp` | BSP returns 0 | Pass |
| `test_get_current_cpu_consistency` | Same CPU returns same ID | Pass |
| `test_percpu_data_isolation` | Each CPU has separate data | Pass |
| `test_percpu_gdt_tss` | Each CPU loads own GDT/TSS | Pass |

**Test File**: `tests/src/smp_tests.rs` (new)

```rust
pub fn test_get_current_cpu_bsp() -> c_int {
    let cpu = get_current_cpu();
    if cpu != 0 {
        klog_info!("FAIL: BSP should be CPU 0, got {}", cpu);
        return 1;
    }
    0
}

pub fn test_get_current_cpu_consistency() -> c_int {
    let cpu1 = get_current_cpu();
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
    let cpu2 = get_current_cpu();
    if cpu1 != cpu2 {
        klog_info!("FAIL: CPU ID changed from {} to {}", cpu1, cpu2);
        return 1;
    }
    0
}
```

### Phase 2 Tests: Per-CPU Scheduler

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_percpu_scheduler_init` | Each CPU has scheduler | Pass |
| `test_local_enqueue_dequeue` | Tasks stay on correct CPU | Pass |
| `test_task_runs_on_assigned_cpu` | Task's last_cpu matches | Pass |
| `test_concurrent_scheduling` | No races with 2+ CPUs | Pass |

```rust
pub fn test_percpu_scheduler_init() -> c_int {
    let cpu_count = get_active_cpu_count();
    for cpu in 0..cpu_count {
        if !is_scheduler_initialized_for_cpu(cpu as usize) {
            klog_info!("FAIL: CPU {} scheduler not initialized", cpu);
            return 1;
        }
    }
    0
}

pub fn test_task_runs_on_assigned_cpu() -> c_int {
    // Create task pinned to CPU 1
    let task_id = task_create_on_cpu(1, "test_task", test_fn, ...);
    
    // Wait for it to run
    task_wait_for(task_id);
    
    // Check it ran on CPU 1
    let task = task_find_by_id(task_id);
    if unsafe { (*task).last_cpu } != 1 {
        klog_info!("FAIL: Task ran on wrong CPU");
        return 1;
    }
    0
}
```

### Phase 3 Tests: AP Activation

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_all_cpus_online` | All discovered CPUs running | Pass |
| `test_ap_runs_tasks` | Tasks execute on APs | Pass |
| `test_ap_idle_when_no_work` | APs halt correctly | Pass |

```rust
pub fn test_all_cpus_online() -> c_int {
    let discovered = get_discovered_cpu_count();
    let online = get_online_cpu_count();
    if discovered != online {
        klog_info!("FAIL: {} discovered, {} online", discovered, online);
        return 1;
    }
    0
}

pub fn test_ap_runs_tasks() -> c_int {
    // Create tasks for each AP
    let cpu_count = get_active_cpu_count();
    let mut completed = [false; MAX_CPUS];
    
    for cpu in 1..cpu_count {
        let task_id = task_create_pinned(cpu, move || {
            completed[cpu] = true;
        });
    }
    
    // Wait and verify all completed
    sleep_ms(100);
    for cpu in 1..cpu_count {
        if !completed[cpu] {
            klog_info!("FAIL: CPU {} didn't run task", cpu);
            return 1;
        }
    }
    0
}
```

### Phase 4 Tests: Load Balancing

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_work_stealing_basic` | Idle CPU steals work | Pass |
| `test_load_balance_distribution` | Tasks spread evenly | Pass |
| `test_no_steal_affinity_violation` | Affinity respected | Pass |

```rust
pub fn test_work_stealing_basic() -> c_int {
    // Create many tasks on CPU 0
    for _ in 0..10 {
        task_create_on_cpu(0, "work", busy_work, ...);
    }
    
    // Wait for work stealing
    sleep_ms(50);
    
    // Check tasks distributed
    let loads: Vec<u32> = (0..get_active_cpu_count())
        .map(|cpu| get_cpu_queue_length(cpu))
        .collect();
    
    // CPU 0 should no longer have all tasks
    if loads[0] >= 8 {
        klog_info!("FAIL: Work not stolen, CPU 0 has {} tasks", loads[0]);
        return 1;
    }
    0
}
```

### Phase 5 Tests: CPU Affinity

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_affinity_pin_to_cpu` | Task stays on pinned CPU | Pass |
| `test_affinity_multiple_cpus` | Task uses allowed CPUs only | Pass |
| `test_affinity_migration` | Task migrates on affinity change | Pass |

### Phase 6 Tests: IPI Coordination

| Test | Description | Expected Result |
|------|-------------|-----------------|
| `test_reschedule_ipi` | IPI wakes halted CPU | Pass |
| `test_cross_cpu_wake` | Task woken runs on target CPU | Pass |

---

## Integration Test Suite

**File**: `tests/src/smp_integration.rs` (new)

```rust
/// Full SMP stress test
pub fn test_smp_stress() -> c_int {
    let cpu_count = get_active_cpu_count();
    let task_count = cpu_count * 4;  // 4 tasks per CPU
    
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    
    // Spawn many tasks that increment counter
    for _ in 0..task_count {
        task_create("stress", || {
            for _ in 0..10000 {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
        });
    }
    
    // Wait for completion
    sleep_ms(5000);
    
    let expected = task_count * 10000;
    let actual = COUNTER.load(Ordering::SeqCst);
    if actual != expected {
        klog_info!("FAIL: Counter {} != expected {}", actual, expected);
        return 1;
    }
    0
}

/// Test that measures actual parallelism
pub fn test_parallel_speedup() -> c_int {
    let cpu_count = get_active_cpu_count();
    if cpu_count < 2 {
        klog_info!("SKIP: Need 2+ CPUs for speedup test");
        return 0;
    }
    
    // Sequential baseline
    let start = timestamp();
    for _ in 0..cpu_count {
        compute_work();
    }
    let sequential_time = timestamp() - start;
    
    // Parallel execution
    let start = timestamp();
    for cpu in 0..cpu_count {
        task_create_pinned(cpu, compute_work);
    }
    wait_all_tasks();
    let parallel_time = timestamp() - start;
    
    // Expect at least 50% speedup with 2+ CPUs
    let speedup = sequential_time as f32 / parallel_time as f32;
    if speedup < 1.5 {
        klog_info!("FAIL: Speedup {} < 1.5x", speedup);
        return 1;
    }
    klog_info!("PASS: Speedup {}x with {} CPUs", speedup, cpu_count);
    0
}
```

---

## Milestone Checkpoints

### Milestone 1: Foundation (Phases 1-2)
**Criteria**:
- [ ] `get_current_cpu()` works on all CPUs
- [ ] Per-CPU GDT/TSS loaded
- [ ] Per-CPU scheduler structures exist
- [ ] All Phase 1-2 tests pass

### Milestone 2: Multi-CPU Scheduling (Phase 3)
**Criteria**:
- [ ] APs enter scheduler loop
- [ ] Tasks run on APs
- [ ] Per-CPU idle tasks work
- [ ] All Phase 3 tests pass

### Milestone 3: Production Ready (Phases 4-6)
**Criteria**:
- [ ] Work stealing functional
- [ ] Load balancing active
- [ ] CPU affinity API complete
- [ ] IPI-based coordination working
- [ ] Stress tests pass
- [ ] No deadlocks under load

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Deadlocks from lock ordering | Document lock hierarchy, use try_lock where possible |
| Cache line bouncing | Align per-CPU data to 64 bytes, minimize sharing |
| Starvation on unbalanced loads | Periodic load balancer, aggressive work stealing |
| ABI breakage | Keep Task struct backward compatible, add fields at end |
| Race conditions | Extensive stress testing, use atomic operations |

---

## References

- Linux kernel `kernel/sched/` - Reference for load balancing
- Redox OS scheduler - Rust-based SMP scheduler
- OSDev Wiki SMP documentation
- Intel SDM Vol 3A - APIC and MP specification

---

## Appendix: Lock Hierarchy

```
Level 0 (lowest - acquired first):
  - Page allocator lock (PAGE_ALLOCATOR)
  - Per-CPU page cache (lock-free)

Level 1:
  - Per-CPU scheduler locks (CPU_SCHEDULERS[n])
  - TLB shootdown state

Level 2:
  - Task manager (TASK_MANAGER)
  - Global scheduler state

Level 3 (highest - acquired last):
  - Console/logging locks
```

**Rule**: Always acquire lower-level locks before higher-level locks to prevent deadlock.
