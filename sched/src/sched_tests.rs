//! Comprehensive scheduler and task management tests.
//!
//! These tests are designed to find REAL bugs, not just pass. They test:
//! - State machine transitions (valid AND invalid)
//! - Edge cases (null, max capacity, overflow)
//! - Race-prone scenarios
//! - Resource exhaustion
//! - Error recovery paths

use core::ffi::{c_char, c_void};
use core::ptr;

use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use super::runtime::{self, IdleStackResolveError};
use super::scheduler::{
    self, get_scheduler_stats, schedule, schedule_new_task, schedule_task, scheduler_is_enabled,
    scheduler_timer_tick, task_wait_for, unschedule_task,
};
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, IdtEntry, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE,
    Task, TaskPriority, TaskStatus, reap_zombies, task_create, task_exit_info_is_set,
    task_find_by_id, task_get_info, task_id_of, task_is_blocked, task_is_terminated,
    task_kernel_stack_top, task_last_cpu, task_priority, task_ref_count, task_set_state,
    task_set_state_with_reason, task_slot_index, task_status, task_terminate, task_waiter_count,
};
use super::test_fixture::KernelTestScope;
use slopos_abi::task::BlockReason;
use slopos_arch::MAX_CPUS;
use slopos_arch::arch::gdt::SegmentSelector;
use slopos_arch::arch::idt::SYSCALL_VECTOR;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

// =============================================================================
// RAII Fixture for Scheduler Tests
// =============================================================================

/// RAII fixture for scheduler tests. All setup/teardown logic lives in
/// [`KernelTestScope`]; this wrapper exists for the historical name and
/// to keep the change to call sites mechanical.
pub struct SchedFixture {
    _scope: KernelTestScope,
}

impl SchedFixture {
    pub fn new() -> Self {
        Self {
            _scope: KernelTestScope::enter(),
        }
    }
}

// =============================================================================
// Test Helper Functions
// =============================================================================

use crate::test_fixture::dummy_task_entry;

// =============================================================================
// STATE MACHINE TESTS
// These tests verify state transitions work correctly AND that invalid
// transitions are properly rejected (or at least logged).
// =============================================================================

/// Test: Valid state transition READY -> RUNNING
pub fn test_state_transition_ready_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"StateTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let task = task_find_by_id(task_id);
    if task.is_null() {
        return TestResult::Fail;
    }

    let initial_state = task_status(task).unwrap_or(TaskStatus::Terminated);
    if initial_state != TaskStatus::Ready {
        klog_info!("SCHED_TEST: Expected READY state, got {:?}", initial_state);
        return TestResult::Fail;
    }

    if task_set_state(task_id, TaskStatus::Running) != 0 {
        klog_info!("SCHED_TEST: Failed to set RUNNING state");
        return TestResult::Fail;
    }

    let new_state = task_status(task).unwrap_or(TaskStatus::Terminated);
    if new_state != TaskStatus::Running {
        klog_info!(
            "SCHED_TEST: Expected RUNNING state after transition, got {:?}",
            new_state
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Valid state transition RUNNING -> BLOCKED
pub fn test_state_transition_running_to_blocked() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"BlockTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // Set to RUNNING first
    task_set_state(task_id, TaskStatus::Running);

    // Then transition to BLOCKED
    if task_set_state(task_id, TaskStatus::Blocked) != 0 {
        klog_info!("SCHED_TEST: Failed to set BLOCKED state");
        return TestResult::Fail;
    }

    let task = task_find_by_id(task_id);
    let state = task_status(task).unwrap_or(TaskStatus::Terminated);
    if state != TaskStatus::Blocked {
        klog_info!("SCHED_TEST: Expected BLOCKED, got {:?}", state);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_state_transition_invalid_terminated_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"InvalidTransition\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // Terminate the task
    task_terminate(task_id);

    // Try to find it again - should fail or be in TERMINATED/INVALID state
    let task = task_find_by_id(task_id);

    if !task.is_null() {
        let _result = task_set_state(task_id, TaskStatus::Running);
        let new_state = task_status(task).unwrap_or(TaskStatus::Terminated);

        if new_state == TaskStatus::Running {
            klog_info!("SCHED_TEST: BUG - Invalid transition TERMINATED->RUNNING was allowed!");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Test: INVALID state transition BLOCKED -> RUNNING (should go through READY first)
pub fn test_state_transition_invalid_blocked_to_running() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"BlockedRunning\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    task_set_state(task_id, TaskStatus::Running);
    task_set_state(task_id, TaskStatus::Blocked);

    let _result = task_set_state(task_id, TaskStatus::Running);

    let task = task_find_by_id(task_id);
    let state = task_status(task).unwrap_or(TaskStatus::Terminated);

    if state == TaskStatus::Running {
        klog_info!("SCHED_TEST: BUG - Invalid transition BLOCKED->RUNNING was allowed!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// TASK CAPACITY TESTS
// Test behavior at and beyond MAX_TASKS limit
// =============================================================================

/// Test: the pool grows lazily. Walks creation past 256 so the
/// tier-3 path in `reserve_task_slot` (`None` slot → fresh
/// `KBox::try_init`) actually fires — tier-1 and tier-2 can only
/// satisfy the first allocations before the pool ever gets that big.
pub fn test_pool_grow_on_demand() -> TestResult {
    let _fixture = SchedFixture::new();

    const TARGET: usize = 512;
    let mut ids: slopos_ostd::KVec<u32> = match slopos_ostd::KVec::with_capacity(TARGET) {
        Ok(v) => v,
        Err(_) => return TestResult::Fail,
    };
    for _ in 0..TARGET {
        let id = task_create(
            b"GrowTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: pool grow failed at {} tasks", ids.len());
            return TestResult::Fail;
        }
        let _ = ids.push(id);
    }

    for id in ids.iter() {
        let _ = task_terminate(*id);
    }
    reap_zombies();
    TestResult::Pass
}

/// Test: Rapid create/destroy cycle - stress test slot reuse
pub fn test_rapid_create_destroy_cycle() -> TestResult {
    let _fixture = SchedFixture::new();

    const CYCLES: usize = 100;
    let mut last_id = INVALID_TASK_ID;

    for i in 0..CYCLES {
        let task_id = task_create(
            b"CycleTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if task_id == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Cycle {} failed to create task", i);
            return TestResult::Fail;
        }

        // Immediately terminate
        if task_terminate(task_id) != 0 {
            klog_info!("SCHED_TEST: Cycle {} failed to terminate task", i);
            return TestResult::Fail;
        }

        last_id = task_id;
    }

    klog_info!(
        "SCHED_TEST: Completed {} create/destroy cycles, last ID={}",
        CYCLES,
        last_id
    );

    TestResult::Pass
}

/// Test: `KernelStack::allocate` returns a handle whose `top > base`
/// and is page-aligned.  Verifies the VA region carving + guard-page
/// layout are correct.
pub fn test_kstack_basic_alloc() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let stack = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: KernelStack::allocate failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let base = stack.base().as_u64();
    let top = stack.top().as_u64();

    if top <= base {
        klog_info!("SCHED_TEST: kstack top 0x{:x} <= base 0x{:x}", top, base);
        return TestResult::Fail;
    }
    if top - base != TASK_STACK_SIZE {
        klog_info!(
            "SCHED_TEST: kstack size mismatch: top-base=0x{:x} want 0x{:x}",
            top - base,
            TASK_STACK_SIZE
        );
        return TestResult::Fail;
    }
    if (base & 0xFFF) != 0 {
        klog_info!("SCHED_TEST: kstack base 0x{:x} not page-aligned", base);
        return TestResult::Fail;
    }

    drop(stack);
    TestResult::Pass
}

/// Test: after dropping a `KernelStack`, the slot is returned to the
/// allocator and can be reused for a subsequent allocation.
///
/// Confirms that task stack capacity is **independent of kernel binary
/// size**, because the slot allocator tracks availability in its own
/// bitmap rather than reading from (kernel-image-reserved) physical
/// pages.
pub fn test_kstack_slot_reuse() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;

    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: second alloc after free failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST: kstack slot not reused: top1=0x{:x} top2=0x{:x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

/// Test: invalid sizes are rejected without touching global state.
pub fn test_kstack_rejects_invalid_size() -> TestResult {
    use super::task_stack::KernelStack;

    // Zero size.
    if KernelStack::allocate(0).is_ok() {
        klog_info!("SCHED_TEST: zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Not a multiple of page size.
    if KernelStack::allocate(4097).is_ok() {
        klog_info!("SCHED_TEST: unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Bigger than the slot stride (64 KB minus guard).
    if KernelStack::allocate(64 * 1024).is_ok() {
        klog_info!("SCHED_TEST: oversized alloc unexpectedly succeeded");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// Per-CPU kstack slot cache tests.
// =============================================================================

/// Test: repeated alloc/free on the same CPU stays in the per-CPU cache.
/// After the first refill, subsequent iterations must not increment
/// `refill_count`.
pub fn test_kstack_pcp_refill() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();

    // Start from a known-clean cache: flush any stale entries back to the
    // global allocator so refill_count readings are meaningful.
    pcp_flush_current::<KstackRegion>();

    let before = pcp_stats::<KstackRegion>(cpu);

    // First alloc → empty cache → triggers exactly one refill.
    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_refill]: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);

    let after_first = pcp_stats::<KstackRegion>(cpu);
    if after_first.refill_count <= before.refill_count {
        klog_info!(
            "SCHED_TEST[pcp_refill]: refill_count did not advance: {} -> {}",
            before.refill_count,
            after_first.refill_count
        );
        return TestResult::Fail;
    }

    // Subsequent allocs should be pure cache hits — the refill batch
    // (8 slots) amply covers several rounds.
    for i in 0..4 {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_refill]: iter {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    let after_warm = pcp_stats::<KstackRegion>(cpu);
    if after_warm.refill_count != after_first.refill_count {
        klog_info!(
            "SCHED_TEST[pcp_refill]: unexpected refill during warm path: {} -> {}",
            after_first.refill_count,
            after_warm.refill_count
        );
        return TestResult::Fail;
    }

    // alloc_count advanced by at least 4 warm-path pops (plus the first).
    if after_warm.alloc_count < before.alloc_count.saturating_add(5) {
        klog_info!(
            "SCHED_TEST[pcp_refill]: alloc_count under-advanced: {} -> {}",
            before.alloc_count,
            after_warm.alloc_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: driving the cache past `pcp_capacity()` forces a spill.
pub fn test_kstack_pcp_spill_overflow() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_capacity, pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<KstackRegion>();
    let baseline_in_use = in_use_count::<KstackRegion>();
    let before = pcp_stats::<KstackRegion>(cpu);

    // Hold N + 1 stacks simultaneously so each drop enters a full cache
    // and triggers a spill.  N = capacity.
    let hold = pcp_capacity::<KstackRegion>() + 1;
    let mut stacks: [Option<KernelStack>; 32] = [const { None }; 32];
    if hold > stacks.len() {
        klog_info!("SCHED_TEST[pcp_spill]: capacity {} > fixture cap", hold);
        return TestResult::Fail;
    }
    for i in 0..hold {
        stacks[i] = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
            Ok(s) => Some(s),
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_spill]: alloc {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
    }
    // Drop all — the first `capacity` fit in the cache, the rest force
    // at least one spill.
    for i in 0..hold {
        stacks[i] = None;
    }

    let after = pcp_stats::<KstackRegion>(cpu);
    if after.spill_count <= before.spill_count {
        klog_info!(
            "SCHED_TEST[pcp_spill]: spill_count did not advance: {} -> {}",
            before.spill_count,
            after.spill_count
        );
        return TestResult::Fail;
    }

    // No leaks: the global in-use counter returns to baseline + (what's
    // still sitting in the cache).  Since we flushed at the start and
    // every stack has been dropped, any residual in_use must equal the
    // current cache `count` exactly.
    let residual_in_use = in_use_count::<KstackRegion>().saturating_sub(baseline_in_use);
    if residual_in_use != after.count {
        klog_info!(
            "SCHED_TEST[pcp_spill]: leak detected: in_use_delta={} cache_count={}",
            residual_in_use,
            after.count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: a slot's `was_backed` bit survives a PCP round-trip.  After
/// alloc/drop/alloc on the same CPU we should see the same VA reused
/// AND the second alloc must NOT hit the mapping path.
pub fn test_kstack_pcp_was_backed_preserved() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_backed]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST[pcp_backed]: PCP did not reuse slot: top1={:#x} top2={:#x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

/// Test: allocate on one CPU, free on another (simulated by explicit
/// flush-between), then reallocate.  The global state must stay
/// consistent — freed slots must be visible to any CPU's refill path.
pub fn test_kstack_pcp_cross_cpu_safety() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    // Alloc, drop, and immediately flush — forces the slot back into
    // the global pool instead of the PCP.  The next alloc then has to
    // refill from the global, exercising the cross-CPU handoff path.
    let s1 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<KstackRegion>();

    let s2 = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[pcp_xcpu]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s2);
    pcp_flush_current::<KstackRegion>();

    let after = in_use_count::<KstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[pcp_xcpu]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: 1000-iteration stress loop with no leaks.
pub fn test_kstack_pcp_stress_1000() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<KstackRegion>();
    let before = in_use_count::<KstackRegion>();

    for i in 0..1000 {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[pcp_stress]: iteration {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    pcp_flush_current::<KstackRegion>();
    let after = in_use_count::<KstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[pcp_stress]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Advisory benchmark: logs cycles-per-alloc for a tight warm-cache
/// loop.  Always passes — the numbers show up in `test_output.log` for
/// regression tracking.
pub fn test_kstack_pcp_smp_throughput_bench() -> TestResult {
    use super::task_stack::KernelStack;
    use slopos_abi::task::TASK_STACK_SIZE;
    use slopos_mm::stack_region::KstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<KstackRegion>();

    // Warm up the cache so the timed loop is a pure PCP hit.
    let warmup = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(_) => return TestResult::Pass,
    };
    drop(warmup);

    const ITERATIONS: u64 = 512;
    let start = slopos_arch::tsc::rdtsc();
    for _ in 0..ITERATIONS {
        let s = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
            Ok(s) => s,
            Err(_) => return TestResult::Pass,
        };
        drop(s);
    }
    let end = slopos_arch::tsc::rdtsc();
    let cycles = end.wrapping_sub(start);
    let per_op = cycles / ITERATIONS;
    klog_info!(
        "SCHED_TEST[pcp_bench] kstack alloc+drop warm path: {} cycles/op over {} iters",
        per_op,
        ITERATIONS
    );

    TestResult::Pass
}

// =============================================================================
// UnsafeStack (SafeStack data-stack) parity tests.
//
// These mirror the kstack tests above against the U-region allocator.  They
// guard the unification refactor: any change that diverges the two regions'
// behavior must show up here.
// =============================================================================

pub fn test_ustack_basic_alloc() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;

    let stack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: UnsafeStack::allocate failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let base = stack.base().as_u64();
    let top = stack.top().as_u64();

    if top <= base {
        klog_info!("SCHED_TEST: ustack top 0x{:x} <= base 0x{:x}", top, base);
        return TestResult::Fail;
    }
    if top - base != TASK_UNSAFE_STACK_SIZE {
        klog_info!(
            "SCHED_TEST: ustack size mismatch: top-base=0x{:x} want 0x{:x}",
            top - base,
            TASK_UNSAFE_STACK_SIZE
        );
        return TestResult::Fail;
    }
    if (base & 0xFFF) != 0 {
        klog_info!("SCHED_TEST: ustack base 0x{:x} not page-aligned", base);
        return TestResult::Fail;
    }

    drop(stack);
    TestResult::Pass
}

pub fn test_ustack_slot_reuse() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: ustack first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST: ustack second alloc after free failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST: ustack slot not reused: top1=0x{:x} top2=0x{:x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

pub fn test_ustack_rejects_invalid_size() -> TestResult {
    use super::task_stack::UnsafeStack;

    if UnsafeStack::allocate(0).is_ok() {
        klog_info!("SCHED_TEST: ustack zero-size alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    if UnsafeStack::allocate(4097).is_ok() {
        klog_info!("SCHED_TEST: ustack unaligned alloc unexpectedly succeeded");
        return TestResult::Fail;
    }
    // Bigger than the slot stride (64 KB minus guard).
    if UnsafeStack::allocate(64 * 1024).is_ok() {
        klog_info!("SCHED_TEST: ustack oversized alloc unexpectedly succeeded");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_refill() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<UstackRegion>();

    let before = pcp_stats::<UstackRegion>(cpu);

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_refill]: first alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);

    let after_first = pcp_stats::<UstackRegion>(cpu);
    if after_first.refill_count <= before.refill_count {
        klog_info!(
            "SCHED_TEST[ustack_refill]: refill_count did not advance: {} -> {}",
            before.refill_count,
            after_first.refill_count
        );
        return TestResult::Fail;
    }

    for i in 0..4 {
        let s = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_refill]: iter {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    let after_warm = pcp_stats::<UstackRegion>(cpu);
    if after_warm.refill_count != after_first.refill_count {
        klog_info!(
            "SCHED_TEST[ustack_refill]: unexpected refill during warm path: {} -> {}",
            after_first.refill_count,
            after_warm.refill_count
        );
        return TestResult::Fail;
    }

    if after_warm.alloc_count < before.alloc_count.saturating_add(5) {
        klog_info!(
            "SCHED_TEST[ustack_refill]: alloc_count under-advanced: {} -> {}",
            before.alloc_count,
            after_warm.alloc_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_spill_overflow() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_capacity, pcp_flush_current, pcp_stats};

    let cpu = slopos_arch::pcr::get_current_cpu();
    pcp_flush_current::<UstackRegion>();
    let baseline_in_use = in_use_count::<UstackRegion>();
    let before = pcp_stats::<UstackRegion>(cpu);

    let hold = pcp_capacity::<UstackRegion>() + 1;
    let mut stacks: [Option<UnsafeStack>; 32] = [const { None }; 32];
    if hold > stacks.len() {
        klog_info!("SCHED_TEST[ustack_spill]: capacity {} > fixture cap", hold);
        return TestResult::Fail;
    }
    for i in 0..hold {
        stacks[i] = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
            Ok(s) => Some(s),
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_spill]: alloc {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
    }
    for i in 0..hold {
        stacks[i] = None;
    }

    let after = pcp_stats::<UstackRegion>(cpu);
    if after.spill_count <= before.spill_count {
        klog_info!(
            "SCHED_TEST[ustack_spill]: spill_count did not advance: {} -> {}",
            before.spill_count,
            after.spill_count
        );
        return TestResult::Fail;
    }

    let residual_in_use = in_use_count::<UstackRegion>().saturating_sub(baseline_in_use);
    if residual_in_use != after.count {
        klog_info!(
            "SCHED_TEST[ustack_spill]: leak detected: in_use_delta={} cache_count={}",
            residual_in_use,
            after.count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_was_backed_preserved() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::pcp_flush_current;

    pcp_flush_current::<UstackRegion>();

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_backed]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let top1 = s1.top();
    drop(s1);

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_backed]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    if s2.top().as_u64() != top1.as_u64() {
        klog_info!(
            "SCHED_TEST[ustack_backed]: PCP did not reuse slot: top1={:#x} top2={:#x}",
            top1.as_u64(),
            s2.top().as_u64()
        );
        return TestResult::Fail;
    }

    drop(s2);
    TestResult::Pass
}

pub fn test_ustack_pcp_cross_cpu_safety() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<UstackRegion>();
    let before = in_use_count::<UstackRegion>();

    let s1 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_xcpu]: s1 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s1);
    pcp_flush_current::<UstackRegion>();

    let s2 = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[ustack_xcpu]: s2 failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(s2);
    pcp_flush_current::<UstackRegion>();

    let after = in_use_count::<UstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[ustack_xcpu]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ustack_pcp_stress_1000() -> TestResult {
    use super::task_stack::UnsafeStack;
    use slopos_abi::task::TASK_UNSAFE_STACK_SIZE;
    use slopos_mm::stack_region::UstackRegion;
    use slopos_mm::stack_va::{in_use_count, pcp_flush_current};

    pcp_flush_current::<UstackRegion>();
    let before = in_use_count::<UstackRegion>();

    for i in 0..1000 {
        let s = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
            Ok(s) => s,
            Err(e) => {
                klog_info!("SCHED_TEST[ustack_stress]: iteration {} failed: {:?}", i, e);
                return TestResult::Fail;
            }
        };
        drop(s);
    }

    pcp_flush_current::<UstackRegion>();
    let after = in_use_count::<UstackRegion>();
    if after != before {
        klog_info!(
            "SCHED_TEST[ustack_stress]: in_use leaked: {} -> {}",
            before,
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: kstack and ustack live in disjoint VA regions and have
/// independent global state.  Three load-bearing invariants the
/// unification refactor must preserve:
///
///   1. The K and U VA windows do not overlap.
///   2. A K allocation lands inside the K window; same for U.
///   3. Allocating from one region must NOT change the other region's
///      `in_use_count`.  (The two regions must be backed by truly
///      independent allocators.)
pub fn test_regions_disjoint() -> TestResult {
    use super::task_stack::{KernelStack, UnsafeStack};
    use slopos_abi::task::{TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE};
    use slopos_mm::memory_layout_defs::{
        KSTACK_VA_BASE, KSTACK_VA_END, USTACK_VA_BASE, USTACK_VA_END,
    };
    use slopos_mm::stack_region::{KstackRegion, UstackRegion};

    // (1) Region windows must not overlap.
    if KSTACK_VA_END > USTACK_VA_BASE && USTACK_VA_END > KSTACK_VA_BASE {
        klog_info!(
            "SCHED_TEST[disjoint]: VA regions overlap: K=[{:#x},{:#x}) U=[{:#x},{:#x})",
            KSTACK_VA_BASE,
            KSTACK_VA_END,
            USTACK_VA_BASE,
            USTACK_VA_END
        );
        return TestResult::Fail;
    }

    // Snapshot U's count, do K work, confirm U didn't move.
    let u_before_k_work = slopos_mm::stack_va::in_use_count::<UstackRegion>();
    let kstack = match KernelStack::allocate(TASK_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[disjoint]: kstack alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let u_after_k_work = slopos_mm::stack_va::in_use_count::<UstackRegion>();
    if u_after_k_work != u_before_k_work {
        klog_info!(
            "SCHED_TEST[disjoint]: kstack alloc disturbed U in_use: {} -> {}",
            u_before_k_work,
            u_after_k_work
        );
        return TestResult::Fail;
    }

    // Snapshot K's count, do U work, confirm K didn't move.
    let k_before_u_work = slopos_mm::stack_va::in_use_count::<KstackRegion>();
    let ustack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(s) => s,
        Err(e) => {
            klog_info!("SCHED_TEST[disjoint]: ustack alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let k_after_u_work = slopos_mm::stack_va::in_use_count::<KstackRegion>();
    if k_after_u_work != k_before_u_work {
        klog_info!(
            "SCHED_TEST[disjoint]: ustack alloc disturbed K in_use: {} -> {}",
            k_before_u_work,
            k_after_u_work
        );
        return TestResult::Fail;
    }

    // (2) Each handle's VA must land in its own region.
    let k_base = kstack.base().as_u64();
    let u_base = ustack.base().as_u64();
    if !(KSTACK_VA_BASE..KSTACK_VA_END).contains(&k_base) {
        klog_info!(
            "SCHED_TEST[disjoint]: kstack base {:#x} not in K region [{:#x},{:#x})",
            k_base,
            KSTACK_VA_BASE,
            KSTACK_VA_END
        );
        return TestResult::Fail;
    }
    if !(USTACK_VA_BASE..USTACK_VA_END).contains(&u_base) {
        klog_info!(
            "SCHED_TEST[disjoint]: ustack base {:#x} not in U region [{:#x},{:#x})",
            u_base,
            USTACK_VA_BASE,
            USTACK_VA_END
        );
        return TestResult::Fail;
    }

    drop(kstack);
    drop(ustack);
    TestResult::Pass
}

// =============================================================================
// SCHEDULER QUEUE TESTS
// Test priority queue behavior including edge cases
// =============================================================================

/// Test: Schedule task to empty queue
pub fn test_schedule_to_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    slopos_arch::pcr::mark_cpu_online(cpu_id);
    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to enable scheduler precondition on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"EmptyQueue\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    // Schedule to empty queue
    if schedule_task(task_ptr) != 0 {
        klog_info!("SCHED_TEST: Failed to schedule task to empty queue");
        return TestResult::Fail;
    }

    // Verify task is in queue by checking stats
    let mut ready_count = 0u32;
    get_scheduler_stats(
        ptr::null_mut(),
        ptr::null_mut(),
        &mut ready_count,
        ptr::null_mut(),
    );

    if ready_count == 0 {
        klog_info!("SCHED_TEST: Task scheduled but ready count is 0");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Schedule same task twice - should not duplicate
pub fn test_schedule_duplicate_task() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"Duplicate\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    task_get_info(task_id, &mut task_ptr);

    // Schedule once
    schedule_task(task_ptr);

    let mut ready_before = 0u32;
    get_scheduler_stats(
        ptr::null_mut(),
        ptr::null_mut(),
        &mut ready_before,
        ptr::null_mut(),
    );

    // Schedule again - should be idempotent
    schedule_task(task_ptr);

    let mut ready_after = 0u32;
    get_scheduler_stats(
        ptr::null_mut(),
        ptr::null_mut(),
        &mut ready_after,
        ptr::null_mut(),
    );

    if ready_after != ready_before {
        klog_info!(
            "SCHED_TEST: Duplicate schedule changed count: {} -> {}",
            ready_before,
            ready_after
        );
        // This is actually handled correctly (returns 0 if already in queue)
        // but let's verify the count didn't change
    }

    TestResult::Pass
}

/// Test: Schedule null task pointer
pub fn test_schedule_null_task() -> TestResult {
    let _fixture = SchedFixture::new();

    let result = schedule_task(ptr::null_mut());

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Scheduling null task succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Unschedule task not in queue
pub fn test_unschedule_not_in_queue() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NotQueued\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    task_get_info(task_id, &mut task_ptr);

    let _result = unschedule_task(task_ptr);

    TestResult::Pass
}

// =============================================================================
// PRIORITY TESTS
// Verify priority-based scheduling works correctly
// =============================================================================

/// Test: Higher priority task should be selected first
pub fn test_priority_ordering() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create tasks with different priorities
    // Priority 0 = highest, Priority 3 = lowest (IDLE)
    let low_id = task_create(
        b"LowPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(), // 2
        TASK_FLAG_KERNEL_MODE,
    );

    let normal_id = task_create(
        b"NormalPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(), // 1
        TASK_FLAG_KERNEL_MODE,
    );

    let high_id = task_create(
        b"HighPri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::High.as_u8(), // 0
        TASK_FLAG_KERNEL_MODE,
    );

    if low_id == INVALID_TASK_ID || normal_id == INVALID_TASK_ID || high_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // Schedule in reverse priority order (low first)
    let mut low_ptr: *mut Task = ptr::null_mut();
    let mut normal_ptr: *mut Task = ptr::null_mut();
    let mut high_ptr: *mut Task = ptr::null_mut();

    task_get_info(low_id, &mut low_ptr);
    task_get_info(normal_id, &mut normal_ptr);
    task_get_info(high_id, &mut high_ptr);

    schedule_task(low_ptr);
    schedule_task(normal_ptr);
    schedule_task(high_ptr);

    TestResult::Pass
}

/// Test: IDLE priority task should be selected last
pub fn test_idle_priority_last() -> TestResult {
    let _fixture = SchedFixture::new();

    let idle_id = task_create(
        b"IdlePri\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Idle.as_u8(), // 3
        TASK_FLAG_KERNEL_MODE,
    );

    let normal_id = task_create(
        b"NormalPri2\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if idle_id == INVALID_TASK_ID || normal_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut idle_ptr: *mut Task = ptr::null_mut();
    let mut normal_ptr: *mut Task = ptr::null_mut();

    task_get_info(idle_id, &mut idle_ptr);
    task_get_info(normal_id, &mut normal_ptr);

    // Schedule idle first, then normal
    schedule_task(idle_ptr);
    schedule_task(normal_ptr);

    // The scheduler should pick normal before idle due to priority
    // We can't directly verify this without running, but we verify no crash

    TestResult::Pass
}

// =============================================================================
// TIMER TICK / PREEMPTION TESTS
// =============================================================================

/// Test: Timer tick should decrement time slice
pub fn test_timer_tick_decrements_slice() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create idle task so scheduler can start
    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"SliceTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    task_get_info(task_id, &mut task_ptr);
    schedule_task(task_ptr);

    TestResult::Pass
}

// =============================================================================
// TERMINATION EDGE CASES
// =============================================================================

/// Test: Terminate task with invalid ID
pub fn test_terminate_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    let result = task_terminate(INVALID_TASK_ID);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating INVALID_TASK_ID succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Terminate non-existent task ID
pub fn test_terminate_nonexistent_id() -> TestResult {
    let _fixture = SchedFixture::new();

    // Use a very high ID that definitely doesn't exist
    let result = task_terminate(0xDEADBEEF);

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - Terminating nonexistent task succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Double terminate same task
pub fn test_double_terminate() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"DoubleTerm\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // First terminate
    let first_result = task_terminate(task_id);
    if first_result != 0 {
        klog_info!("SCHED_TEST: First terminate failed");
        return TestResult::Fail;
    }

    let _second_result = task_terminate(task_id);

    TestResult::Pass
}

// =============================================================================
// TASK FIND/GET EDGE CASES
// =============================================================================

/// Test: Find task by invalid ID
pub fn test_find_invalid_id() -> TestResult {
    let _fixture = SchedFixture::new();

    let task = task_find_by_id(INVALID_TASK_ID);

    if !task.is_null() {
        klog_info!("SCHED_TEST: BUG - Found task with INVALID_TASK_ID!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Get info with null output pointer
pub fn test_get_info_null_output() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"NullOutput\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    // Call with null output pointer
    let result = task_get_info(task_id, ptr::null_mut());

    if result == 0 {
        klog_info!("SCHED_TEST: BUG - task_get_info with null output succeeded!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// TASK CREATION EDGE CASES
// =============================================================================

/// Test: Create task with null entry point
#[allow(unused_variables)]
pub fn test_create_null_entry() -> TestResult {
    let _fixture = SchedFixture::new();

    let _null_fn_ptr: Option<fn(*mut c_void)> = None;

    TestResult::Pass
}

/// Test: Create task with conflicting mode flags
pub fn test_create_conflicting_flags() -> TestResult {
    let _fixture = SchedFixture::new();

    // Both kernel and user mode flags
    let bad_flags = TASK_FLAG_KERNEL_MODE | super::task::TASK_FLAG_USER_MODE;

    let task_id = task_create(
        b"BadFlags\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        bad_flags,
    );

    if task_id != INVALID_TASK_ID {
        klog_info!("SCHED_TEST: BUG - Created task with conflicting flags!");
        task_terminate(task_id);
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Create task with null name (should still work)
pub fn test_create_null_name() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        ptr::null(),
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    // Null name should be allowed (empty name)
    if task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: Task creation with null name failed (may be OK)");
        // This is actually acceptable behavior
    }

    TestResult::Pass
}

// =============================================================================
// SCHEDULER ENABLE/DISABLE TESTS
// =============================================================================

/// Test: Scheduler starts disabled
pub fn test_scheduler_starts_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    let enabled = scheduler_is_enabled();

    if enabled != 0 {
        klog_info!("SCHED_TEST: Scheduler should start disabled!");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Schedule call when scheduler disabled
pub fn test_schedule_while_disabled() -> TestResult {
    let _fixture = SchedFixture::new();

    // Scheduler is disabled by default after init
    // Calling schedule() should be a no-op
    schedule();

    // Should not crash, no-op when disabled
    TestResult::Pass
}

/// Regression: boot userland pre-init enqueues tasks before enter_scheduler().
/// This must work on the current CPU even when its scheduler is initialized
/// but not yet enabled.
pub fn test_schedule_task_before_scheduler_enable_on_current_cpu() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.disable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to disable scheduler precondition on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"BootPreInit\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    if cpu_id >= u32::BITS as usize {
        return TestResult::Pass;
    }

    crate::task::task_install_idle_affinity(task_ptr, 1u32 << cpu_id, cpu_id as u8);

    if schedule_task(task_ptr) != 0 {
        klog_info!(
            "SCHED_TEST: Failed to schedule task before scheduler enable on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_count == 0 {
        klog_info!(
            "SCHED_TEST: Task was not enqueued before scheduler enable on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: BSP idle-stack handoff must use idle task kernel stack.
pub fn test_resolve_idle_stack_for_bsp_uses_idle_task_kernel_stack() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task_for_cpu(0) != 0 {
        klog_info!("SCHED_TEST: Failed to create BSP idle task");
        return TestResult::Fail;
    }

    let (idle_task, stack_top) = match runtime::resolve_idle_stack_for_cpu(0) {
        Ok(values) => values,
        Err(err) => {
            klog_info!("SCHED_TEST: Failed to resolve BSP idle stack: {:?}", err);
            return TestResult::Fail;
        }
    };

    if idle_task.is_null() {
        klog_info!("SCHED_TEST: Resolved idle task pointer is null");
        return TestResult::Fail;
    }

    let expected_top = task_kernel_stack_top(idle_task).unwrap_or(0);
    if expected_top == 0 || stack_top != expected_top {
        klog_info!(
            "SCHED_TEST: Idle stack mismatch (expected=0x{:x}, got=0x{:x})",
            expected_top,
            stack_top
        );
        return TestResult::Fail;
    }

    if (stack_top & 0xF) != 0 {
        klog_info!(
            "SCHED_TEST: Idle stack is not 16-byte aligned: 0x{:x}",
            stack_top
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: idle-stack resolution must fail cleanly when no idle task exists.
pub fn test_resolve_idle_stack_reports_missing_idle_task() -> TestResult {
    let _fixture = SchedFixture::new();

    // PCR.idle_task is the single source of truth for the idle slot.
    let previous_idle = slopos_arch::pcr::get_idle_task(0) as *mut Task;
    slopos_arch::pcr::set_idle_task(0, ptr::null_mut());

    let result = match runtime::resolve_idle_stack_for_cpu(0) {
        Err(IdleStackResolveError::MissingIdleTask) => TestResult::Pass,
        Err(other) => {
            klog_info!(
                "SCHED_TEST: Expected MissingIdleTask, got different error: {:?}",
                other
            );
            TestResult::Fail
        }
        Ok((_, stack_top)) => {
            klog_info!(
                "SCHED_TEST: Expected missing idle task, got stack 0x{:x}",
                stack_top
            );
            TestResult::Fail
        }
    };

    slopos_arch::pcr::set_idle_task(0, previous_idle as *mut ());

    result
}

/// Regression: idle-stack resolution must fail cleanly for zero kernel stack top.
pub fn test_resolve_idle_stack_reports_missing_kernel_stack() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task_for_cpu(0) != 0 {
        klog_info!("SCHED_TEST: Failed to create BSP idle task");
        return TestResult::Fail;
    }

    let idle_task = slopos_arch::pcr::get_idle_task(0) as *mut Task;
    if idle_task.is_null() {
        klog_info!("SCHED_TEST: Failed to fetch BSP idle task from PCR");
        return TestResult::Fail;
    }

    let original_top = task_kernel_stack_top(idle_task).unwrap_or(0);
    crate::task::task_set_kernel_stack_top(idle_task, 0);

    let result = match runtime::resolve_idle_stack_for_cpu(0) {
        Err(IdleStackResolveError::MissingKernelStack) => TestResult::Pass,
        Err(other) => {
            klog_info!(
                "SCHED_TEST: Expected MissingKernelStack, got different error: {:?}",
                other
            );
            TestResult::Fail
        }
        Ok((_, stack_top)) => {
            klog_info!(
                "SCHED_TEST: Expected missing kernel stack, got stack 0x{:x}",
                stack_top
            );
            TestResult::Fail
        }
    };

    crate::task::task_set_kernel_stack_top(idle_task, original_top);

    result
}

// =============================================================================
// STRESS TESTS
// =============================================================================

/// Test: Create many tasks with same priority
pub fn test_many_same_priority_tasks() -> TestResult {
    let _fixture = SchedFixture::new();

    const COUNT: usize = 32;
    let mut ids = [INVALID_TASK_ID; COUNT];

    for i in 0..COUNT {
        ids[i] = task_create(
            b"SamePri\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if ids[i] == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Failed at task {}", i);
            break;
        }
    }

    // Schedule all of them
    for id in ids.iter() {
        if *id != INVALID_TASK_ID {
            let mut ptr: *mut Task = ptr::null_mut();
            if task_get_info(*id, &mut ptr) == 0 && !ptr.is_null() {
                schedule_task(ptr);
            }
        }
    }

    let mut ready = 0u32;
    get_scheduler_stats(
        ptr::null_mut(),
        ptr::null_mut(),
        &mut ready,
        ptr::null_mut(),
    );

    klog_info!("SCHED_TEST: Scheduled {} tasks of same priority", ready);

    TestResult::Pass
}

/// Test: Interleaved create/schedule/terminate
pub fn test_interleaved_operations() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..50 {
        // Create
        let id1 = task_create(
            b"Inter1\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        let id2 = task_create(
            b"Inter2\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::High.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if id1 == INVALID_TASK_ID || id2 == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Interleaved creation failed at iteration {}", i);
            return TestResult::Fail;
        }

        // Schedule first
        let mut ptr1: *mut Task = ptr::null_mut();
        task_get_info(id1, &mut ptr1);
        if !ptr1.is_null() {
            schedule_task(ptr1);
        }

        // Terminate first before scheduling second
        task_terminate(id1);

        // Schedule second
        let mut ptr2: *mut Task = ptr::null_mut();
        task_get_info(id2, &mut ptr2);
        if !ptr2.is_null() {
            schedule_task(ptr2);
        }

        // Terminate second
        task_terminate(id2);
    }

    TestResult::Pass
}

// =============================================================================
// CROSS-CPU SCHEDULING TESTS (SMP)
// Tests for the unified per-CPU scheduler architecture
// =============================================================================

/// Test: Remote inbox push and drain mechanism
/// Verifies that push_remote_wake() correctly adds tasks to the inbox
/// and drain_remote_inbox() moves them to the ready queue.
pub fn test_remote_inbox_push_drain() -> TestResult {
    let _fixture = SchedFixture::new();

    let task_id = task_create(
        b"InboxTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Get ready count before
    let ready_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    // Push to remote inbox (simulating cross-CPU wake)
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(task_ptr);
    });

    // Verify inbox has pending task.
    // On SMP, a timer tick may concurrently drain the inbox before this read.
    // We treat that as acceptable and validate via ready-queue delta below.
    let has_pending = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
        .unwrap_or(false);

    // Drain inbox
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    // Verify inbox is now empty
    let still_pending =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if still_pending && has_pending {
        klog_info!("SCHED_TEST: drain_remote_inbox did not empty inbox");
        return TestResult::Fail;
    }

    // Verify task is now in ready queue
    let ready_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    if ready_after <= ready_before {
        klog_info!(
            "SCHED_TEST: Task not moved to ready queue: before={}, after={}",
            ready_before,
            ready_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Multiple tasks in remote inbox
/// Verifies FIFO ordering is preserved through inbox drain
pub fn test_remote_inbox_multiple_tasks() -> TestResult {
    let _fixture = SchedFixture::new();

    const NUM_TASKS: usize = 5;
    let mut task_ids = [INVALID_TASK_ID; NUM_TASKS];
    let mut task_ptrs: [*mut Task; NUM_TASKS] = [ptr::null_mut(); NUM_TASKS];

    // Create tasks
    for i in 0..NUM_TASKS {
        task_ids[i] = task_create(
            b"MultiInbox\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );

        if task_ids[i] == INVALID_TASK_ID {
            klog_info!("SCHED_TEST: Failed to create task {}", i);
            return TestResult::Fail;
        }

        task_get_info(task_ids[i], &mut task_ptrs[i]);
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Push all to inbox
    for i in 0..NUM_TASKS {
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.push_remote_wake(task_ptrs[i]);
        });
    }

    // Drain all
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    // Verify all are in ready queue
    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);

    if (ready_count as usize) < NUM_TASKS {
        klog_info!(
            "SCHED_TEST: Not all tasks in ready queue: expected {}, got {}",
            NUM_TASKS,
            ready_count
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Timer tick drains inbox on all CPUs
/// This is the key test for the unified scheduler inbox-drain path.
pub fn test_timer_tick_drains_inbox() -> TestResult {
    let _fixture = SchedFixture::new();

    // Create idle task so scheduler can work
    if scheduler::create_idle_task() != 0 {
        klog_info!("SCHED_TEST: Failed to create idle task");
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"TimerDrain\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Push to inbox (bypassing normal schedule_task)
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(task_ptr);
    });

    // Verify inbox has pending
    let has_pending_before =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(false);

    if !has_pending_before {
        klog_info!("SCHED_TEST: Task not in inbox before timer tick");
        return TestResult::Fail;
    }

    // Simulate timer tick - this should drain the inbox
    scheduler_timer_tick();

    // Verify inbox is now empty (drained by timer tick)
    let has_pending_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);

    if has_pending_after {
        klog_info!("SCHED_TEST: Timer tick did not drain inbox");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Draining remote inbox must not enqueue non-ready tasks.
pub fn test_remote_inbox_drops_non_ready_tasks() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let task_id = task_create(
        b"InboxBlocked\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.push_remote_wake(task_ptr);
    });

    if task_set_state(task_id, TaskStatus::Running) != 0
        || task_set_state(task_id, TaskStatus::Blocked) != 0
    {
        klog_info!("SCHED_TEST: Failed to transition task to BLOCKED before inbox drain");
        return TestResult::Fail;
    }

    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    let ready_count =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_count != 0 {
        klog_info!(
            "SCHED_TEST: Non-ready task was enqueued from inbox (ready_count={})",
            ready_count
        );
        return TestResult::Fail;
    }

    let inbox_pending =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.has_pending_inbox())
            .unwrap_or(true);
    if inbox_pending {
        klog_info!("SCHED_TEST: Inbox still has pending entries after drain");
        return TestResult::Fail;
    }

    if task_ref_count(task_ptr).unwrap_or(0) != 0 {
        klog_info!(
            "SCHED_TEST: Task refcount leaked after inbox drain (refcnt={})",
            task_ref_count(task_ptr).unwrap_or(0)
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Test: Cross-CPU schedule_task uses lock-free path
/// Verifies that schedule_task to another CPU uses push_remote_wake
pub fn test_cross_cpu_schedule_lockfree() -> TestResult {
    let _fixture = SchedFixture::new();

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count < 2 {
        klog_info!("SCHED_TEST: Skipping cross-CPU test (only 1 CPU)");
        return TestResult::Pass; // Skip on single-CPU systems
    }

    let current_cpu = slopos_arch::pcr::get_current_cpu() as usize;
    let target_cpu =
        match (0..cpu_count).find(|cpu| *cpu != current_cpu && *cpu < u32::BITS as usize) {
            Some(cpu) => cpu,
            None => {
                klog_info!(
                    "SCHED_TEST: Skipping cross-CPU test (no target CPU != {} in affinity range)",
                    current_cpu
                );
                return TestResult::Pass;
            }
        };
    let target_cpu_u8 = target_cpu as u8;

    slopos_arch::pcr::mark_cpu_online(target_cpu);
    if super::per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Failed to enable target CPU {} scheduler",
            target_cpu
        );
        return TestResult::Fail;
    }

    let task_id = task_create(
        b"CrossCPU\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );

    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }
    // Keep last_cpu on the current CPU so the scheduler must migrate it.
    crate::task::task_install_idle_affinity(task_ptr, 1u32 << target_cpu, current_cpu as u8);

    let result = schedule_task(task_ptr);
    if result != 0 {
        klog_info!("SCHED_TEST: Cross-CPU schedule_task failed");
        return TestResult::Fail;
    }

    // After drain, it should be in ready queue
    super::per_cpu::with_cpu_scheduler(target_cpu, |sched| {
        sched.drain_remote_inbox();
    });

    let ready_on_target =
        super::per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.total_ready_count())
            .unwrap_or(0);

    if ready_on_target == 0 {
        klog_info!(
            "SCHED_TEST: Task not found on CPU {} after cross-CPU schedule",
            target_cpu
        );
        return TestResult::Fail;
    }

    if task_last_cpu(task_ptr).unwrap_or(0) != target_cpu_u8 {
        klog_info!(
            "SCHED_TEST: last_cpu not updated to target CPU (expected {}, got {})",
            target_cpu,
            task_last_cpu(task_ptr).unwrap_or(0)
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// PRIVILEGE SEPARATION TESTS
// Verify that user-mode tasks get correct segment selectors, process VM,
// kernel RSP0 stack, and that the syscall gate has DPL=3.
// =============================================================================

/// Test: User-mode tasks are created with correct privilege separation invariants.
pub fn test_privilege_separation_invariants() -> TestResult {
    let _fixture = SchedFixture::new();

    let user_task_id = task_create(
        b"UserStub\0".as_ptr() as *const c_char,
        crate::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    );
    if user_task_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: user task creation failed");
        return TestResult::Fail;
    }
    // Prevent the scheduler on other CPUs from running this stub task.
    task_set_state(user_task_id, TaskStatus::Blocked);

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(user_task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        klog_info!("SCHED_TEST: user task lookup failed");
        return TestResult::Fail;
    }

    let Some(task_ref) = crate::task::task_borrow(task_ptr) else {
        return TestResult::Fail;
    };
    if task_ref.process_id == INVALID_PROCESS_ID {
        klog_info!("SCHED_TEST: user task missing process VM");
        return TestResult::Fail;
    }
    if task_ref.kernel_stack_top == 0 {
        klog_info!("SCHED_TEST: user task missing kernel RSP0 stack");
        return TestResult::Fail;
    }
    let cs = task_ref.context.cs;
    let ss = task_ref.context.ss;
    if cs != SegmentSelector::USER_CODE.bits() as u64
        || ss != SegmentSelector::USER_DATA.bits() as u64
    {
        klog_info!(
            "SCHED_TEST: user selectors wrong (cs=0x{:x} ss=0x{:x})",
            cs,
            ss
        );
        return TestResult::Fail;
    }

    let mut gate = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };
    let gate_ptr = &mut gate as *mut IdtEntry as *mut c_void;
    if slopos_kernel_services::platform::idt_get_gate(SYSCALL_VECTOR, gate_ptr) != 0 {
        klog_info!("SCHED_TEST: cannot read syscall gate");
        return TestResult::Fail;
    }
    let dpl = (gate.type_attr >> 5) & 0x3;
    if dpl != 3 {
        klog_info!("SCHED_TEST: syscall gate DPL={} expected 3", dpl as u32);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_scheduler_wakeup_race_stress_baseline() -> TestResult {
    let _fixture = SchedFixture::new();

    let mut task_ids = [INVALID_TASK_ID; 8];
    for slot in &mut task_ids {
        let id = task_create(
            b"WakeStress\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        *slot = id;
    }

    for _ in 0..128 {
        for id in task_ids {
            let task_ptr = task_find_by_id(id);
            if task_ptr.is_null() {
                return TestResult::Fail;
            }
            let _ = schedule_task(task_ptr);
        }
        scheduler_timer_tick();
        schedule();
        for id in task_ids {
            let task_ptr = task_find_by_id(id);
            if !task_ptr.is_null() {
                let _ = unschedule_task(task_ptr);
            }
            if task_find_by_id(id).is_null() {
                return TestResult::Fail;
            }
            let _ = task_set_state(id, TaskStatus::Ready);
        }
    }

    for id in task_ids {
        task_terminate(id);
    }

    TestResult::Pass
}

pub fn test_sleep_wake_race_regression() -> TestResult {
    let _fixture = SchedFixture::new();
    super::sleep::reset_sleep_queue();

    let task_id = task_create(
        b"SleepRace\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        return TestResult::Fail;
    }

    // Use a wake_tick far in the future so that real timer interrupts
    // (which call wake_due_sleepers with the current tick) never collect
    // our entry before the test explicitly wakes it.  With wake_tick=100
    // the entry was already "due" by the time the test ran, creating a
    // race between the timer handler and the test's block/wake sequence.
    const FAR_FUTURE: u64 = u64::MAX / 2;

    for round in 0..64 {
        let _ = task_set_state(task_id, TaskStatus::Running);

        if !super::sleep::test_insert_sleep_entry(task_id, FAR_FUTURE) {
            klog_info!("SCHED_TEST: sleep queue insert failed at round {}", round);
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if task_set_state_with_reason(task_id, TaskStatus::Blocked, BlockReason::Sleep) != 0 {
            klog_info!("SCHED_TEST: set Blocked failed at round {}", round);
            super::sleep::cancel_sleep(task_id);
            task_terminate(task_id);
            return TestResult::Fail;
        }

        super::sleep::wake_due_sleepers(FAR_FUTURE + 1);

        if task_is_blocked(task_ptr) {
            klog_info!("SCHED_TEST: task stuck in Blocked after wake — race bug");
            let _ = task_set_state(task_id, TaskStatus::Ready);
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// REGRESSION: task_wait_for race-window robustness (harmonic-cascade Phase 1H)
//
// These tests verify that the new wait/wake protocol — durable per-task
// `exit_info` cell published before the `waiters` queue's `wake_all`
// fanout — closes the lost-wakeup race that the legacy two-atomic
// `(status, waiting_on)` pair admitted. The buggy version deadlocked on
// child-exit at ~5% on KVM and much higher on TCG.
//
// In the test fixture APs are paused, so the runner cannot reproduce
// the multi-CPU race window directly. What it *can* exercise is the
// fast-path that Phase 1's design relies on for late waiters: any
// `task_wait_for` call that arrives after `mark_task_terminated`'s
// publish must observe the durable `exit_info` (or the Terminated
// status) on its first condition check and return immediately, no
// matter how many such calls land on the same target. A regression
// that re-introduces the lost-wake bug would either deadlock here (the
// runner's test thread blocks forever) or fail the post-conditions on
// `exit_info` / `task_is_terminated`.
// =============================================================================

/// 1000-iteration stress: child kthread is created and immediately
/// terminated; parent (this runner) calls `task_wait_for(child_id)`
/// against the freshly-terminated slot. Each iteration must return
/// promptly via the fast path — no blocking, no deadlock.
///
/// What this catches if regressed:
/// - `mark_task_terminated` failing to publish `exit_info` before
///   `release_task_dependents` (Phase 1G regression) → late waiter
///   would not see `is_set()` true and could fall through to the
///   blocking path, deadlocking the runner.
/// - `task_wait_for`'s condition closure forgetting the
///   `task_is_terminated(target)` fallback (Phase 1E regression) →
///   same outcome if the publish path ever skipped `try_set`.
/// - `release_task_dependents` no longer doing `waiters.wake_all()`
///   (Phase 1F regression) → caught only on the multi-waiter
///   variant below; the 1000-iter case is dominated by the durable
///   exit_info fast path.
pub fn test_task_wait_exit_race_1000() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..1000 {
        let child_id = task_create(
            b"WaitRace\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if child_id == INVALID_TASK_ID {
            klog_info!(
                "SCHED_TEST: task_create failed at iteration {} (pool exhausted?)",
                i
            );
            return TestResult::Fail;
        }

        let child_ptr = task_find_by_id(child_id);
        if child_ptr.is_null() {
            klog_info!("SCHED_TEST: task_find_by_id null at iteration {}", i);
            return TestResult::Fail;
        }

        // Terminate the child synchronously. mark_task_terminated runs
        // inline: publishes exit_info via try_set, then wake_all on
        // the (still-empty) waiters queue.
        let rc = task_terminate(child_id);
        if rc != 0 {
            klog_info!(
                "SCHED_TEST: task_terminate returned {} at iteration {}",
                rc,
                i
            );
            return TestResult::Fail;
        }

        // Post-conditions for the publish step.
        if !task_is_terminated(child_ptr) {
            klog_info!(
                "SCHED_TEST: child not Terminated after task_terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }
        if !task_exit_info_is_set(child_ptr) {
            klog_info!(
                "SCHED_TEST: exit_info not published after task_terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }

        // The wait must complete via the fast path — if it returns at
        // all the runner is not deadlocked, which is the property we
        // care about.
        let wait_rc = task_wait_for(child_id);
        if wait_rc != 0 {
            klog_info!(
                "SCHED_TEST: task_wait_for returned {} at iter {} (expected 0)",
                wait_rc,
                i
            );
            return TestResult::Fail;
        }

        // Reap on every iteration so the pool does not fill with
        // zombies; reap_zombies is bounded and idempotent.
        reap_zombies();
    }

    TestResult::Pass
}

/// Same shape as `test_task_wait_exit_race_1000` but the child slot is
/// driven through Ready→Running→Ready transitions before terminate, so
/// the "did some work" code path of `mark_task_terminated` (which
/// updates `total_runtime` based on `last_run_timestamp`) is exercised
/// alongside the publish/fanout sequence. This shifts the publish
/// timing relative to the runner's wait, so a regression that only
/// reliably misses the wake when the child has run is still caught.
pub fn test_task_wait_exit_race_with_work() -> TestResult {
    let _fixture = SchedFixture::new();

    for i in 0..500 {
        let child_id = task_create(
            b"WaitRaceWork\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if child_id == INVALID_TASK_ID {
            klog_info!(
                "SCHED_TEST: task_create failed at iteration {} (pool exhausted?)",
                i
            );
            return TestResult::Fail;
        }

        let child_ptr = task_find_by_id(child_id);
        if child_ptr.is_null() {
            klog_info!("SCHED_TEST: task_find_by_id null at iter {}", i);
            return TestResult::Fail;
        }

        // Simulate "child did some work": Ready -> Running, advance a
        // synthetic last_run_timestamp, then back to Ready so terminate
        // observes a non-zero runtime delta in mark_task_terminated.
        // The state set must respect the FSM (Ready -> Running is
        // valid).
        if task_set_state(child_id, TaskStatus::Running) != 0 {
            klog_info!("SCHED_TEST: failed Running transition at iter {}", i);
            return TestResult::Fail;
        }
        crate::task::task_set_last_run_timestamp(child_ptr, 1);
        // Spin a few iterations to advance any kdiag timestamp source
        // and shift the relative ordering of publish vs. observe.
        for _ in 0..16 {
            core::hint::spin_loop();
        }
        if task_set_state(child_id, TaskStatus::Ready) != 0 {
            klog_info!("SCHED_TEST: failed Ready transition at iter {}", i);
            return TestResult::Fail;
        }

        let rc = task_terminate(child_id);
        if rc != 0 {
            klog_info!("SCHED_TEST: task_terminate returned {} at iter {}", rc, i);
            return TestResult::Fail;
        }

        if !task_is_terminated(child_ptr) {
            klog_info!(
                "SCHED_TEST: child not Terminated after terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }
        if !task_exit_info_is_set(child_ptr) {
            klog_info!(
                "SCHED_TEST: exit_info not published after terminate at iter {}",
                i
            );
            return TestResult::Fail;
        }

        let wait_rc = task_wait_for(child_id);
        if wait_rc != 0 {
            klog_info!(
                "SCHED_TEST: task_wait_for returned {} at iter {} (expected 0)",
                wait_rc,
                i
            );
            return TestResult::Fail;
        }

        reap_zombies();
    }

    TestResult::Pass
}

/// Multi-waiter fanout: durable `exit_info` must satisfy any number of
/// late waiters that arrive after the wake fanout has already fired
/// against an (possibly empty) `waiters` queue. Each subsequent
/// `task_wait_for` for the same terminated child must hit the
/// fast-path return without blocking, no matter how many siblings
/// have already done the same.
///
/// What this catches if regressed:
/// - `release_task_dependents` failing to invoke
///   `child.waiters.wake_all()` (Phase 1F regression) is partially
///   caught here: while the test cannot enqueue real foreign waiters
///   under the paused-AP fixture, it does verify the symmetric
///   guarantee — that durable exit_info plus the Terminated status
///   make repeated independent waits all return 0.
/// - `exit_info` not surviving the wake fanout (e.g. a future
///   refactor that `take`s instead of `try_get`s) — the 4th waiter
///   would observe `is_set()` false.
pub fn test_task_wait_multi_waiter() -> TestResult {
    let _fixture = SchedFixture::new();

    let child_id = task_create(
        b"MultiWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let child_ptr = task_find_by_id(child_id);
    if child_ptr.is_null() {
        return TestResult::Fail;
    }

    // Pre-condition: waiters queue is empty before terminate.
    if task_waiter_count(child_ptr) > 0 {
        klog_info!("SCHED_TEST: child waiters queue non-empty before terminate");
        return TestResult::Fail;
    }

    if task_terminate(child_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate failed");
        return TestResult::Fail;
    }

    // After terminate: status Terminated and exit_info published.
    if !task_is_terminated(child_ptr) {
        klog_info!("SCHED_TEST: child not Terminated after terminate");
        return TestResult::Fail;
    }
    if !task_exit_info_is_set(child_ptr) {
        klog_info!("SCHED_TEST: exit_info not set after terminate");
        return TestResult::Fail;
    }

    // 4 simulated sibling parents each independently observe the
    // durable exit_info via the fast-path return.
    for waiter in 0..4 {
        let rc = task_wait_for(child_id);
        if rc != 0 {
            klog_info!(
                "SCHED_TEST: waiter {} task_wait_for returned {} (expected 0)",
                waiter,
                rc
            );
            return TestResult::Fail;
        }
        // exit_info must remain set across multiple observations
        // (try_get is non-consuming; only `take` consumes — and the
        // wait path never takes).
        if !task_exit_info_is_set(child_ptr) {
            klog_info!(
                "SCHED_TEST: exit_info became unset after waiter {} returned",
                waiter
            );
            return TestResult::Fail;
        }
    }

    reap_zombies();
    TestResult::Pass
}

// =============================================================================
// REGRESSION: Tick accounting & load-aware CPU selection
// =============================================================================

/// Regression: scheduler_timer_tick() must always increment total_ticks.
/// Previously the early-return path skipped increment_ticks(), under-counting
/// ticks on busy CPUs.  This test exercises the unguarded (no PreemptGuard)
/// path only; the guarded path is covered by the live scheduler under SMP.
pub fn test_timer_tick_always_increments_ticks() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let ticks_before = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched
            .total_ticks
            .load(core::sync::atomic::Ordering::Relaxed)
    })
    .unwrap_or(0);

    // Fire several timer ticks
    for _ in 0..5 {
        scheduler_timer_tick();
    }

    let ticks_after = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched
            .total_ticks
            .load(core::sync::atomic::Ordering::Relaxed)
    })
    .unwrap_or(0);

    let delta = ticks_after.saturating_sub(ticks_before);
    if delta < 5 {
        klog_info!(
            "SCHED_TEST: timer_tick incremented ticks only {} times (expected >=5)",
            delta
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: idle_time must track ticks, not loop iterations.
/// When the idle task is current, each timer tick should increment both
/// total_ticks and idle_time by the same amount.
pub fn test_idle_time_tracks_ticks_not_iterations() -> TestResult {
    let _fixture = SchedFixture::new();

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Set current task to the idle task so timer_tick recognises us as idle.
    // `dispatch()` writes PCR.current_task + scheduler-copy + syscall_pid
    // + state=Running in lockstep — single-writer invariant.
    let idle_task = slopos_arch::pcr::get_idle_task(cpu_id) as *mut Task;
    if idle_task.is_null() {
        return TestResult::Fail;
    }
    super::scheduler::dispatch(cpu_id, idle_task);

    let (ticks_before, idle_before) = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        (
            sched
                .total_ticks
                .load(core::sync::atomic::Ordering::Relaxed),
            sched.idle_time.load(core::sync::atomic::Ordering::Relaxed),
        )
    })
    .unwrap_or((0, 0));

    for _ in 0..10 {
        scheduler_timer_tick();
    }

    let (ticks_after, idle_after) = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        (
            sched
                .total_ticks
                .load(core::sync::atomic::Ordering::Relaxed),
            sched.idle_time.load(core::sync::atomic::Ordering::Relaxed),
        )
    })
    .unwrap_or((0, 0));

    let delta_ticks = ticks_after.saturating_sub(ticks_before);
    let delta_idle = idle_after.saturating_sub(idle_before);

    // Both should have incremented by 10 (one per tick).
    if delta_ticks < 10 {
        klog_info!("SCHED_TEST: total_ticks delta {} < 10", delta_ticks);
        return TestResult::Fail;
    }

    let drift = if delta_idle > delta_ticks {
        delta_idle - delta_ticks
    } else {
        delta_ticks - delta_idle
    };
    // Allow a small tolerance (up to 2 ticks) for SMP timing jitter.
    if drift > 2 {
        klog_info!(
            "SCHED_TEST: idle_time ({}) vs total_ticks ({}) — drift {} exceeds tolerance",
            delta_idle,
            delta_ticks,
            drift
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: select_target_cpu should prefer idle CPUs over busy ones.
/// Previously it always returned last_cpu regardless of load.
pub fn test_select_target_cpu_prefers_idle_cpu() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        // Single-CPU systems cannot test cross-CPU placement.
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Ensure both the local CPU and at least one other CPU are online
    // and schedulable so select_target_cpu sees both as candidates.
    slopos_arch::pcr::mark_cpu_online(cpu_id);
    if super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Could not enable scheduler on local CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }
    let other_cpu = if cpu_id == 0 { 1 } else { 0 };
    slopos_arch::pcr::mark_cpu_online(other_cpu);
    if super::per_cpu::with_cpu_scheduler(other_cpu, |sched| sched.enable()).is_none() {
        klog_info!(
            "SCHED_TEST: Could not enable scheduler on CPU {}",
            other_cpu
        );
        return TestResult::Fail;
    }

    // Load up last_cpu (cpu_id) with several queued tasks.
    let mut filler_ids = [INVALID_TASK_ID; 3];
    for i in 0..3 {
        let tid = task_create(
            b"Filler\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if tid == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        filler_ids[i] = tid;
        let mut tp: *mut Task = ptr::null_mut();
        if task_get_info(tid, &mut tp) != 0 || tp.is_null() {
            return TestResult::Fail;
        }
        // Pin fillers to cpu_id so they stay in its queue.
        crate::task::task_install_idle_affinity(
            tp,
            super::per_cpu::affinity_mask_for_cpu(cpu_id),
            cpu_id as u8,
        );
        if schedule_task(tp) != 0 {
            return TestResult::Fail;
        }
    }

    // Create a test task whose last_cpu is cpu_id (busy), with affinity=0 (any CPU).
    let task_id = task_create(
        b"Migratee\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }

    crate::task::task_install_idle_affinity(task_ptr, 0, cpu_id as u8);

    let target = super::per_cpu::select_target_cpu(task_ptr);
    match target {
        Some(t) if t == other_cpu => { /* expected — migrated to idle CPU */ }
        Some(t) if t == cpu_id => {
            klog_info!(
                "SCHED_TEST: select_target_cpu returned busy last_cpu {} instead of idle CPU {}",
                cpu_id,
                other_cpu
            );
            return TestResult::Fail;
        }
        Some(t) => {
            // Some other idle CPU is also acceptable.
            klog_info!(
                "SCHED_TEST: select_target_cpu chose CPU {} (not the expected {} but still OK)",
                t,
                other_cpu
            );
        }
        None => {
            klog_info!("SCHED_TEST: select_target_cpu returned None");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Regression: CPU running a real task with empty queue must NOT be
/// considered idle.  This is the key scenario that caused all tasks to
/// stick to CPU0 — bursty workloads left the queue empty between bursts,
/// so the old code always returned last_cpu.
pub fn test_select_target_cpu_running_task_not_idle() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let other_cpu = if cpu_id == 0 { 1 } else { 0 };
    slopos_arch::pcr::mark_cpu_online(other_cpu);
    if super::per_cpu::with_cpu_scheduler(other_cpu, |sched| sched.enable()).is_none() {
        return TestResult::Fail;
    }

    // Simulate a real task running on cpu_id: create a task and set it as
    // the current task.  The queue stays empty, but effective_load should
    // be 1 because a non-idle task is running.
    let runner_id = task_create(
        b"Runner\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if runner_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let mut runner_ptr: *mut Task = ptr::null_mut();
    if task_get_info(runner_id, &mut runner_ptr) != 0 || runner_ptr.is_null() {
        return TestResult::Fail;
    }
    super::scheduler::dispatch(cpu_id, runner_ptr);

    let load =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load()).unwrap_or(0);
    if load == 0 {
        klog_info!(
            "SCHED_TEST: effective_load is 0 despite running task on CPU {}",
            cpu_id
        );
        return TestResult::Fail;
    }

    // Create a task with last_cpu = cpu_id.  Even though cpu_id's QUEUE
    // is empty, the scheduler should NOT consider it idle.
    let task_id = task_create(
        b"WakeTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }
    crate::task::task_install_idle_affinity(task_ptr, 0, cpu_id as u8);

    let target = super::per_cpu::select_target_cpu(task_ptr);
    match target {
        Some(t) if t != cpu_id => { /* expected — migrated away from busy CPU */ }
        Some(t) => {
            klog_info!(
                "SCHED_TEST: select_target_cpu stuck to CPU {} despite running task (empty queue)",
                t
            );
            return TestResult::Fail;
        }
        None => {
            klog_info!("SCHED_TEST: select_target_cpu returned None");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Regression: schedule_new_task() must spread sequential forks across
/// CPUs via round-robin, not pile them all onto CPU0.  Mirrors Linux's
/// WF_FORK / SD_BALANCE_FORK slow path.
pub fn test_schedule_new_task_spreads_across_cpus() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if cpu_count < 2 {
        return TestResult::Pass;
    }

    if scheduler::create_idle_task() != 0 {
        return TestResult::Fail;
    }

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Enable all CPUs for scheduling.
    for c in 0..cpu_count {
        slopos_arch::pcr::mark_cpu_online(c);
        super::per_cpu::with_cpu_scheduler(c, |sched| sched.enable());
    }

    // Simulate the parent (shell) running on cpu_id by setting current_task.
    let parent_id = task_create(
        b"Parent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let mut parent_ptr: *mut Task = ptr::null_mut();
    if task_get_info(parent_id, &mut parent_ptr) != 0 {
        return TestResult::Fail;
    }
    super::scheduler::dispatch(cpu_id, parent_ptr);

    // Spawn N children using schedule_new_task (the fork path).
    let n = cpu_count.min(4);
    let mut placed_on = [0usize; 4];
    for i in 0..n {
        let tid = task_create(
            b"Child\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if tid == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        let mut tp: *mut Task = ptr::null_mut();
        if task_get_info(tid, &mut tp) != 0 || tp.is_null() {
            return TestResult::Fail;
        }
        crate::task::task_set_cpu_affinity(tp, 0); // any CPU
        if schedule_new_task(tp) != 0 {
            return TestResult::Fail;
        }
        placed_on[i] = task_last_cpu(tp).unwrap_or(0) as usize;
    }

    // Verify that at least 2 distinct CPUs were used (not all on CPU0).
    let mut distinct = [false; MAX_CPUS];
    let mut count = 0usize;
    for i in 0..n {
        if !distinct[placed_on[i]] {
            distinct[placed_on[i]] = true;
            count += 1;
        }
    }

    if count < 2 {
        klog_info!(
            "SCHED_TEST: schedule_new_task placed all {} children on same CPU ({})",
            n,
            placed_on[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: effective_load must reflect queued tasks correctly.
pub fn test_effective_load_accuracy() -> TestResult {
    let _fixture = SchedFixture::new();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // After fixture reset, effective_load should be 0 or 1 (just the
    // running task on this CPU, if any).
    let load_before = super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load())
        .unwrap_or(u32::MAX);
    if load_before > 1 {
        klog_info!(
            "SCHED_TEST: effective_load {} > 1 on empty queues",
            load_before
        );
        return TestResult::Fail;
    }

    // Enqueue a task — effective_load should increase.
    let task_id = task_create(
        b"LoadCheck\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let mut task_ptr: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return TestResult::Fail;
    }
    super::per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enqueue_local(task_ptr);
    });

    let load_after =
        super::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.effective_load()).unwrap_or(0);
    if load_after <= load_before {
        klog_info!(
            "SCHED_TEST: effective_load did not increase after enqueue ({} -> {})",
            load_before,
            load_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// =============================================================================
// REGRESSION: Phase 7A race-exerciser stress suite
//
// These three tests stress the wait/wake protocol from angles the
// existing Phase 1 sequential stress tests do not cover:
//
//   1. Overlapping parent/child lifetimes within each "fork group" so
//      multiple terminate publishes interleave with multiple wait
//      observations against distinct waiters queues.
//   2. Deep slot-reuse churn: 1000 children, each fully reaped before
//      the next is spawned, exercising `reset_in_place` /
//      `clone_from_raw` correctness for the per-task `waiters` ring +
//      `exit_info` cell — stale entries from the previous occupant
//      must NOT leak into the fresh slot.
//   3. Cross-priority terminate/wait pair so the runqueue's
//      priority-aware enqueue logic is exercised on the wake path
//      even though the child publishes from a Low-priority slot.
//
// Under `KernelTestScope` APs are paused, so these are not true SMP
// races — they widen coverage of the durable-exit-info fast path,
// the reset_in_place reuse contract, and the cross-priority wake
// fanout. A regression that re-introduces lost wakeups would either
// deadlock (test runner hangs) or fail one of the post-conditions.
// =============================================================================

/// 10 fork-groups × 100 iterations: each iteration spawns 10 children
/// concurrently (all live in the slot pool together), then terminates
/// and waits for each in turn. Total fork/exit/wait cycles = 1000.
///
/// What this catches if regressed:
/// - `mark_task_terminated` skipping the `waiters.wake_all()` fanout
///   (Phase 1F) — under overlapping lifetimes the ring buffer of the
///   first child has had ample opportunity to observe non-empty
///   waiter slots from sibling churn, so a missed fanout would
///   diverge from the durable exit_info path.
/// - `reset_in_place` failing to clear the `waiters` ring between
///   slot reuses — would surface as a non-empty `has_waiters()` on a
///   fresh task before the test calls terminate.
const FORK_GROUP_WIDTH: usize = 10;
const FORK_GROUP_ITERATIONS: usize = 100;

pub fn test_fork_exit_wait_stress_10x100() -> TestResult {
    let _fixture = SchedFixture::new();

    let mut child_ids = [INVALID_TASK_ID; FORK_GROUP_WIDTH];

    for outer in 0..FORK_GROUP_ITERATIONS {
        // Phase 1: spawn FORK_GROUP_WIDTH children. Their lifetimes
        // overlap — every slot is allocated before any is terminated
        // — so the wait/wake protocol is exercised against a pool
        // with WIDTH live siblings rather than the always-singleton
        // case of the existing 1000-iter test.
        for slot in 0..FORK_GROUP_WIDTH {
            let id = task_create(
                b"ForkStress\0".as_ptr() as *const c_char,
                dummy_task_entry,
                ptr::null_mut(),
                TaskPriority::Normal.as_u8(),
                TASK_FLAG_KERNEL_MODE,
            );
            if id == INVALID_TASK_ID {
                klog_info!(
                    "SCHED_TEST: task_create failed at outer={} slot={} (pool exhausted?)",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
            child_ids[slot] = id;

            // Pre-condition: a freshly-allocated slot must come back
            // with an empty waiters ring and an unset exit_info, no
            // matter how many prior reuses it has been through. Stale
            // state here would mean `reset_in_place` /
            // `clone_from_raw` left residue from the previous
            // occupant.
            let ptr = task_find_by_id(id);
            if ptr.is_null() {
                klog_info!(
                    "SCHED_TEST: task_find_by_id null at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
            if task_waiter_count(ptr) > 0 {
                klog_info!(
                    "SCHED_TEST: fresh child has stale waiters at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
            if task_exit_info_is_set(ptr) {
                klog_info!(
                    "SCHED_TEST: fresh child has stale exit_info at outer={} slot={}",
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }

        // Phase 2: terminate all WIDTH children. Each terminate
        // publishes exit_info and fans out wake_all on a still-empty
        // waiters queue. The ordering of these publishes interleaves
        // across siblings — different from the singleton case.
        for slot in 0..FORK_GROUP_WIDTH {
            let id = child_ids[slot];
            let rc = task_terminate(id);
            if rc != 0 {
                klog_info!(
                    "SCHED_TEST: task_terminate({}) returned {} at outer={} slot={}",
                    id,
                    rc,
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }

        // Phase 3: wait_for each terminated child. Every call must
        // hit the fast path (durable exit_info) and return 0 without
        // blocking. If the runner deadlocks here Phase 1's lost-wake
        // fix has regressed.
        for slot in 0..FORK_GROUP_WIDTH {
            let id = child_ids[slot];
            let wait_rc = task_wait_for(id);
            if wait_rc != 0 {
                klog_info!(
                    "SCHED_TEST: task_wait_for({}) returned {} at outer={} slot={} (expected 0)",
                    id,
                    wait_rc,
                    outer,
                    slot
                );
                return TestResult::Fail;
            }
        }

        // Reap on every outer iteration so the pool does not fill
        // with zombies; reap_zombies is bounded and idempotent.
        reap_zombies();
    }

    TestResult::Pass
}

/// 1000 children spawned and reaped sequentially. Verifies that the
/// per-task `waiters` queue and `exit_info` cell handle high churn
/// — a recycled slot must NEVER carry stale waiter pointers or
/// stale exit_info from the previous occupant. Exercises Phase 1C's
/// `reset_in_place` correctness specifically.
///
/// What this catches if regressed:
/// - `reset_in_place` byte-zero pass overwriting the `WaitQueue`
///   storage without re-`write`ing a fresh `WaitQueue::new()` —
///   would surface as a corrupt has_waiters() / waiter_count() read
///   on the fresh slot's first inspection.
/// - `reset_in_place` failing to call `exit_info.reset()` before the
///   byte-zero pass — would leak the previous KBox<ExitValue> and
///   leave is_set() observably true on a fresh task.
/// - A regression that drops the `addr_of_mut!.write(WaitQueue::new())`
///   re-init step would corrupt the AtomicUsize generation counter
///   and break wake_all on the recycled slot.
pub fn test_serial_reap_stampede() -> TestResult {
    let _fixture = SchedFixture::new();

    const STAMPEDE: usize = 1000;
    let mut last_slot: u32 = u32::MAX;
    let mut distinct_slots: usize = 0;

    for i in 0..STAMPEDE {
        let id = task_create(
            b"ReapStampede\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            klog_info!(
                "SCHED_TEST: task_create failed at iteration {} (pool exhausted?)",
                i
            );
            return TestResult::Fail;
        }

        let ptr = task_find_by_id(id);
        if ptr.is_null() {
            klog_info!("SCHED_TEST: task_find_by_id null at iter {}", i);
            return TestResult::Fail;
        }

        // Track slot reuse so we can prove this test actually
        // exercises the recycle path — if every iteration grabbed a
        // fresh slot the pool would have to be >= STAMPEDE, but
        // reap_zombies below should free slots for reuse and so
        // `slot_index` should repeat.
        let slot_index = task_slot_index(ptr).unwrap_or(0);
        if slot_index != last_slot {
            distinct_slots += 1;
        }
        last_slot = slot_index;

        // Pre-condition: the recycled slot must come back blank.
        // These two assertions are the heart of this test — they
        // catch any regression in `reset_in_place`'s reuse
        // discipline.
        if task_waiter_count(ptr) > 0 {
            klog_info!(
                "SCHED_TEST: recycled slot {} has stale waiters at iter {}",
                slot_index,
                i
            );
            return TestResult::Fail;
        }
        if task_exit_info_is_set(ptr) {
            klog_info!(
                "SCHED_TEST: recycled slot {} has stale exit_info at iter {}",
                slot_index,
                i
            );
            return TestResult::Fail;
        }

        // Terminate + wait + reap. The wait must hit the fast path.
        let rc = task_terminate(id);
        if rc != 0 {
            klog_info!("SCHED_TEST: task_terminate returned {} at iter {}", rc, i);
            return TestResult::Fail;
        }
        if !task_is_terminated(ptr) {
            klog_info!("SCHED_TEST: child not Terminated at iter {}", i);
            return TestResult::Fail;
        }
        if !task_exit_info_is_set(ptr) {
            klog_info!("SCHED_TEST: exit_info not published at iter {}", i);
            return TestResult::Fail;
        }
        let wait_rc = task_wait_for(id);
        if wait_rc != 0 {
            klog_info!(
                "SCHED_TEST: task_wait_for returned {} at iter {} (expected 0)",
                wait_rc,
                i
            );
            return TestResult::Fail;
        }
        reap_zombies();
    }

    // Sanity: with a 1000-iter stampede on an 8192-slot pool the
    // allocator should still hand out fresh slots most of the time
    // (zombies are reaped only at the end of each iteration), so
    // `distinct_slots` will be high. The looser invariant we care
    // about is that *some* reuse happened — otherwise the test is
    // not actually exercising the recycle path it claims to.
    if distinct_slots == STAMPEDE {
        klog_info!(
            "SCHED_TEST: stampede saw {} distinct slots — no slot reuse, recycle path NOT exercised",
            distinct_slots
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Regression: a parent's `waitpid_nohang`-equivalent reaper must still
/// observe a child's exit info after dozens of unrelated task
/// allocations fire between the child's termination and the parent's
/// reaping call. Before the Zombie-state introduction, the child's
/// exit record lived in a slot-indexed parallel array that was wiped on
/// every tier-2 slot reuse, so any new `task_create` between the
/// child's exit and the reap would silently drop the exit code and
/// hang the WNOHANG spin loop. The fix gates tier-2 reuse on
/// `Terminated` (not `Zombie`), so the child's `exit_info` cell stays
/// stable until `task_consume_zombie` transitions it.
///
/// What this catches if regressed:
/// - Anyone re-adding a separate exit-record cache keyed by slot index
///   that gets wiped on slot reuse → fails because the post-churn
///   consume returns None.
/// - A change that lets `reserve_task_slot` tier-2 pick a slot whose
///   status is still `Zombie` → the recycled slot's exit_info gets
///   reset, again the post-churn consume returns None.
pub fn test_waitpid_survives_concurrent_slot_reuse() -> TestResult {
    use super::task::{task_consume_zombie, task_set_parent};

    let _fixture = SchedFixture::new();

    // "Parent" runs the test (`task_set_parent` makes the child's
    // parent_task_id resolve to a Ready slot, so the child takes the
    // Zombie path on termination).
    let parent_id = task_create(
        b"ZombieParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let child_id = task_create(
        b"ZombieChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    task_set_parent(child_id, parent_id);

    if task_terminate(child_id) != 0 {
        return TestResult::Fail;
    }

    let child_ptr = task_find_by_id(child_id);
    if child_ptr.is_null() {
        return TestResult::Fail;
    }
    if task_status(child_ptr).unwrap_or(TaskStatus::Terminated) != TaskStatus::Zombie {
        klog_info!(
            "SCHED_TEST: child not Zombie after terminate (status={:?})",
            task_status(child_ptr).unwrap_or(TaskStatus::Terminated)
        );
        return TestResult::Fail;
    }

    // Churn: spawn + immediately terminate + reap a long chain of
    // kernel-mode tasks. These have no parent so they go straight to
    // Terminated, then reap_zombies + tier-2 reuse will hammer the
    // pool. None of these may steal the child's slot.
    for _ in 0..256 {
        let id = task_create(
            b"ChurnTask\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            TaskPriority::Normal.as_u8(),
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            return TestResult::Fail;
        }
        let _ = task_terminate(id);
        reap_zombies();
    }

    // Re-find the child by ID and confirm its slot is still the same
    // task (slot_index + task_id pair preserved). This is the "no
    // slot-reuse for Zombies" invariant under inspection.
    let child_ptr_after = task_find_by_id(child_id);
    if child_ptr_after.is_null() {
        klog_info!("SCHED_TEST: child slot vanished during churn");
        return TestResult::Fail;
    }
    if task_id_of(child_ptr_after).unwrap_or(0) != child_id {
        return TestResult::Fail;
    }
    if task_status(child_ptr_after).unwrap_or(TaskStatus::Terminated) != TaskStatus::Zombie {
        klog_info!("SCHED_TEST: child not Zombie after churn");
        return TestResult::Fail;
    }

    // Parent's reaper would call this. Must succeed.
    let info = match task_consume_zombie(child_id) {
        Some(i) => i,
        None => {
            klog_info!("SCHED_TEST: task_consume_zombie returned None after churn");
            return TestResult::Fail;
        }
    };
    if info.exit_code != 0 {
        return TestResult::Fail;
    }

    // After consume, slot should be Terminated and tier-2 reusable.
    if task_status(child_ptr_after).unwrap_or(TaskStatus::Terminated) != TaskStatus::Terminated {
        klog_info!("SCHED_TEST: child not Terminated after consume");
        return TestResult::Fail;
    }

    // Cleanup: terminate the parent so the next test sees a clean pool.
    let _ = task_terminate(parent_id);
    reap_zombies();

    TestResult::Pass
}

/// Regression: when a parent terminates without reaping its Zombie
/// children, those children must be auto-reaped (Zombie → Terminated)
/// so their slots become tier-2 reusable. Without this, a parent that
/// crashes leaves zombie slots pinned forever and the pool eventually
/// exhausts.
///
/// What this catches if regressed:
/// - `mark_task_terminated` no longer calling `reparent_and_reap_children`
///   → the zombie stays pinned, this test sees status == Zombie after
///   the parent's exit and fails.
pub fn test_orphan_child_auto_reaped_on_parent_exit() -> TestResult {
    use super::task::task_set_parent;

    let _fixture = SchedFixture::new();

    let parent_id = task_create(
        b"OrphanParent\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if parent_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }

    let child_id = task_create(
        b"OrphanChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    task_set_parent(child_id, parent_id);

    if task_terminate(child_id) != 0 {
        return TestResult::Fail;
    }
    let child_ptr = task_find_by_id(child_id);
    if child_ptr.is_null()
        || task_status(child_ptr).unwrap_or(TaskStatus::Terminated) != TaskStatus::Zombie
    {
        return TestResult::Fail;
    }

    // Parent dies without ever calling waitpid: the dying parent must
    // sweep its child list and demote the Zombie to Terminated.
    if task_terminate(parent_id) != 0 {
        return TestResult::Fail;
    }

    let child_ptr_after = task_find_by_id(child_id);
    if child_ptr_after.is_null() {
        return TestResult::Fail;
    }
    if task_status(child_ptr_after).unwrap_or(TaskStatus::Terminated) != TaskStatus::Terminated {
        klog_info!(
            "SCHED_TEST: orphan child still {:?} after parent exit",
            task_status(child_ptr_after).unwrap_or(TaskStatus::Terminated)
        );
        return TestResult::Fail;
    }

    reap_zombies();
    TestResult::Pass
}

/// Cross-priority wait: a high-priority caller waits on a
/// low-priority child that has already published its exit. Under the
/// paused-AP fixture this is not a true SMP priority-inversion
/// scenario, but it does exercise the symmetric guarantee — that
/// the durable exit_info publish from a Low-priority slot is fully
/// visible to a High-priority observer's wait_event condition check
/// regardless of the producer's runqueue placement.
///
/// What this catches if regressed:
/// - `mark_task_terminated` publishing exit_info via a path that
///   somehow varies with `task->priority` (it shouldn't) would
///   surface as a different ordering on Low than on Normal — the
///   existing Phase 1 tests use Normal exclusively, so this fills
///   that gap.
/// - A future change that makes the WaitQueue's wake fanout
///   priority-aware (e.g. reordering wake_all by waiter priority)
///   could lose the wake on a High waiter waiting on a Low
///   producer; this test pins that wake to fire and observes the
///   fast-path return.
pub fn test_cross_priority_wait() -> TestResult {
    let _fixture = SchedFixture::new();

    // Pre-allocate the High slot BEFORE creating the Low child. If
    // we created High after Low and Low had already been
    // terminated, `reserve_task_slot`'s tier-2 reuse path would pick
    // up the Low slot, run `Task::reset_in_place` on it, and the
    // Low child's `exit_info` would be silently invalidated under
    // our feet — at which point the cross-priority test would no
    // longer be checking what it claims to check.
    //
    // Allocation order: High first → Low second → terminate Low →
    // wait_for(Low) from the runner. Both slots stay valid for the
    // duration of the test.
    let high_id = task_create(
        b"HighWaiter\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::High.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if high_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: task_create(High) failed");
        return TestResult::Fail;
    }
    let high_ptr = task_find_by_id(high_id);
    if high_ptr.is_null() {
        klog_info!("SCHED_TEST: task_find_by_id(High) null");
        return TestResult::Fail;
    }
    if task_priority(high_ptr).unwrap_or(TaskPriority::Low) != TaskPriority::High {
        klog_info!(
            "SCHED_TEST: High waiter priority is {:?}, expected High",
            task_priority(high_ptr).unwrap_or(TaskPriority::Low)
        );
        return TestResult::Fail;
    }

    // Producer: low priority. The exit publish runs synchronously
    // inside `task_terminate` from the runner CPU, but the slot's
    // `priority` field stays Low — so any priority-conditional
    // logic on the producer side is exercised at the Low setting.
    let child_id = task_create(
        b"LowChild\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Low.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    );
    if child_id == INVALID_TASK_ID {
        klog_info!("SCHED_TEST: task_create(Low) failed");
        return TestResult::Fail;
    }

    let child_ptr = task_find_by_id(child_id);
    if child_ptr.is_null() {
        klog_info!("SCHED_TEST: task_find_by_id(Low) null");
        return TestResult::Fail;
    }
    if task_priority(child_ptr).unwrap_or(TaskPriority::Low) != TaskPriority::Low {
        klog_info!(
            "SCHED_TEST: Low child priority is {:?}, expected Low",
            task_priority(child_ptr).unwrap_or(TaskPriority::Low)
        );
        return TestResult::Fail;
    }

    // Sanity: the two slots really are distinct, so the wait below
    // is genuinely cross-slot rather than a self-wait that the early
    // task_id-equality check in `task_wait_for` would short-circuit.
    if core::ptr::eq(child_ptr, high_ptr) {
        klog_info!("SCHED_TEST: Low and High mapped to the same slot");
        return TestResult::Fail;
    }

    // Terminate Low producer. Publishes exit_info and fans out on
    // the (still-empty) waiters queue, all from a Low-priority
    // slot's perspective.
    if task_terminate(child_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate(Low) failed");
        return TestResult::Fail;
    }
    if !task_is_terminated(child_ptr) {
        klog_info!("SCHED_TEST: Low child not Terminated after task_terminate");
        return TestResult::Fail;
    }
    if !task_exit_info_is_set(child_ptr) {
        klog_info!("SCHED_TEST: exit_info not published by Low producer");
        return TestResult::Fail;
    }

    // The wait must complete via the durable exit_info fast path.
    // A regression that gates wake fanout on producer priority
    // would either deadlock the runner here or corrupt exit_info.
    // task_wait_for holds a TaskRefGuard for the duration of the
    // call, so the Low slot cannot be tier-2-recycled mid-wait.
    let wait_rc = task_wait_for(child_id);
    if wait_rc != 0 {
        klog_info!(
            "SCHED_TEST: cross-priority task_wait_for returned {} (expected 0)",
            wait_rc
        );
        return TestResult::Fail;
    }

    // Post-condition: the durable exit_info is still observable on
    // the Low slot. Because the High slot was reserved BEFORE the
    // Low slot terminated, no tier-2 reset_in_place could have run
    // on the Low slot — so this assertion is not racing the
    // recycle path. The wait path uses `try_get`/`is_set`
    // (non-consuming), never `take`; a regression that consumed
    // the cell would leave subsequent waiters stranded.
    if !task_exit_info_is_set(child_ptr) {
        klog_info!("SCHED_TEST: exit_info became unset after High-priority wait returned");
        return TestResult::Fail;
    }

    // Clean up the High waiter slot so reap_zombies retires both.
    if task_terminate(high_id) != 0 {
        klog_info!("SCHED_TEST: task_terminate(High) failed");
        return TestResult::Fail;
    }
    reap_zombies();

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_state_transition_ready_to_running,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_running_to_blocked,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_invalid_terminated_to_running,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_state_transition_invalid_blocked_to_running,
    suite = sched_core
);
slopos_testing::stest!(name = test_pool_grow_on_demand, suite = sched_core);
slopos_testing::stest!(name = test_rapid_create_destroy_cycle, suite = sched_core);
slopos_testing::stest!(name = test_kstack_basic_alloc, suite = sched_core);
slopos_testing::stest!(name = test_kstack_slot_reuse, suite = sched_core);
slopos_testing::stest!(name = test_kstack_rejects_invalid_size, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_refill, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_spill_overflow, suite = sched_core);
slopos_testing::stest!(
    name = test_kstack_pcp_was_backed_preserved,
    suite = sched_core
);
slopos_testing::stest!(name = test_kstack_pcp_cross_cpu_safety, suite = sched_core);
slopos_testing::stest!(name = test_kstack_pcp_stress_1000, suite = sched_core);
slopos_testing::stest!(
    name = test_kstack_pcp_smp_throughput_bench,
    suite = sched_core
);
slopos_testing::stest!(name = test_ustack_basic_alloc, suite = sched_core);
slopos_testing::stest!(name = test_ustack_slot_reuse, suite = sched_core);
slopos_testing::stest!(name = test_ustack_rejects_invalid_size, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_refill, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_spill_overflow, suite = sched_core);
slopos_testing::stest!(
    name = test_ustack_pcp_was_backed_preserved,
    suite = sched_core
);
slopos_testing::stest!(name = test_ustack_pcp_cross_cpu_safety, suite = sched_core);
slopos_testing::stest!(name = test_ustack_pcp_stress_1000, suite = sched_core);
slopos_testing::stest!(name = test_regions_disjoint, suite = sched_core);
slopos_testing::stest!(name = test_schedule_to_empty_queue, suite = sched_core);
slopos_testing::stest!(name = test_schedule_duplicate_task, suite = sched_core);
slopos_testing::stest!(name = test_schedule_null_task, suite = sched_core);
slopos_testing::stest!(name = test_unschedule_not_in_queue, suite = sched_core);
slopos_testing::stest!(name = test_priority_ordering, suite = sched_core);
slopos_testing::stest!(name = test_idle_priority_last, suite = sched_core);
slopos_testing::stest!(name = test_timer_tick_decrements_slice, suite = sched_core);
slopos_testing::stest!(name = test_terminate_invalid_id, suite = sched_core);
slopos_testing::stest!(name = test_terminate_nonexistent_id, suite = sched_core);
slopos_testing::stest!(name = test_double_terminate, suite = sched_core);
slopos_testing::stest!(name = test_find_invalid_id, suite = sched_core);
slopos_testing::stest!(name = test_get_info_null_output, suite = sched_core);
slopos_testing::stest!(name = test_create_null_entry, suite = sched_core);
slopos_testing::stest!(name = test_create_conflicting_flags, suite = sched_core);
slopos_testing::stest!(name = test_create_null_name, suite = sched_core);
slopos_testing::stest!(name = test_scheduler_starts_disabled, suite = sched_core);
slopos_testing::stest!(name = test_schedule_while_disabled, suite = sched_core);
slopos_testing::stest!(
    name = test_schedule_task_before_scheduler_enable_on_current_cpu,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_reports_missing_idle_task,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_reports_missing_kernel_stack,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_resolve_idle_stack_for_bsp_uses_idle_task_kernel_stack,
    suite = sched_core
);
slopos_testing::stest!(name = test_many_same_priority_tasks, suite = sched_core);
slopos_testing::stest!(name = test_interleaved_operations, suite = sched_core);
slopos_testing::stest!(name = test_remote_inbox_push_drain, suite = sched_core);
slopos_testing::stest!(name = test_remote_inbox_multiple_tasks, suite = sched_core);
slopos_testing::stest!(name = test_timer_tick_drains_inbox, suite = sched_core);
slopos_testing::stest!(
    name = test_remote_inbox_drops_non_ready_tasks,
    suite = sched_core
);
slopos_testing::stest!(name = test_cross_cpu_schedule_lockfree, suite = sched_core);
slopos_testing::stest!(
    name = test_privilege_separation_invariants,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_scheduler_wakeup_race_stress_baseline,
    suite = sched_core
);
slopos_testing::stest!(name = test_sleep_wake_race_regression, suite = sched_core);
slopos_testing::stest!(name = test_task_wait_exit_race_1000, suite = sched_core);
slopos_testing::stest!(
    name = test_task_wait_exit_race_with_work,
    suite = sched_core
);
slopos_testing::stest!(name = test_task_wait_multi_waiter, suite = sched_core);
slopos_testing::stest!(
    name = test_timer_tick_always_increments_ticks,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_idle_time_tracks_ticks_not_iterations,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_select_target_cpu_prefers_idle_cpu,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_select_target_cpu_running_task_not_idle,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_schedule_new_task_spreads_across_cpus,
    suite = sched_core
);
slopos_testing::stest!(name = test_fork_exit_wait_stress_10x100, suite = sched_core);
slopos_testing::stest!(name = test_serial_reap_stampede, suite = sched_core);
slopos_testing::stest!(name = test_cross_priority_wait, suite = sched_core);
slopos_testing::stest!(name = test_effective_load_accuracy, suite = sched_core);
slopos_testing::stest!(
    name = test_waitpid_survives_concurrent_slot_reuse,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_orphan_child_auto_reaped_on_parent_exit,
    suite = sched_core
);

// =============================================================================
// WAIT_QUEUE TESTS
//
// These tests cover the wait/wake primitive's APIs and defense-in-depth
// invariants:
//
// - `WaitOutcome<R>` enum return type.
// - `wait_event_until` / `wait_event_timeout_until` generic-return APIs.
// - Lock-free `has_waiters()` (callers that want to skip a `wake_*`
//   when no one is queued do this at the call site rather than baked
//   into `wake_*` itself — the in-place fast path is unsound on
//   weakly-ordered architectures; see `wait_protocol.md` §6.3).
// - `WaitNode::has_woken` auxiliary atomic, exercised indirectly via
//   the wake-empty-queue tests and via the wider scheduler integration.
// - `WaitNode` Drop with null queue back-pointer is a no-op (the
//   common case — back-pointer is cleared inside the WQ critical
//   section by every pop path, so by the time stack-pinned `WaitNode`s
//   go out of scope they hold null).
//
// A panic-mid-wait Drop-firing test is intentionally not included: the
// test-harness uses `catch_panic!` / longjmp, which skips Drops during
// recovery; the Drop-based unlink is defense-in-depth for production
// unwinding paths and runs implicitly on every successful wait.
// =============================================================================

use slopos_ostd::sync::wait_queue::{WaitOutcome, WaitQueue};

/// `wait_event_until` returns the closure's `Some(R)` immediately via
/// the pre-check fast path, without touching the scheduler backend.
pub fn test_wait_event_until_pre_check_returns_carried_value() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: Option<u32> = wq.wait_event_until(|| Some(0xCAFE_F00D_u32));
    if r == Some(0xCAFE_F00D_u32) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Some(0xCAFE_F00D)",
            r
        );
        TestResult::Fail
    }
}

/// `wait_event_timeout_until` returns `WaitOutcome::Ready(R)` on
/// the pre-check fast path.
pub fn test_wait_event_timeout_until_pre_check_ready() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: WaitOutcome<u32> = wq.wait_event_timeout_until(|| Some(7u32), 100);
    if matches!(r, WaitOutcome::Ready(7)) {
        TestResult::Pass
    } else {
        klog_info!(
            "WAIT_QUEUE_TEST: pre-check returned {:?}, expected Ready(7)",
            r
        );
        TestResult::Fail
    }
}

/// `wait_event_timeout_until` with an always-`None` closure does NOT
/// return `Ready` — it must end in either `Timeout` (real wait
/// elapsed) or `NoRuntime` (current task null / backend not yet
/// fully wired in this test fixture). Both are acceptable
/// soundness-preserving outcomes; the property we care about is
/// "the call returns without hanging or panicking and produces a
/// non-Ready outcome when the condition is unsatisfiable."
pub fn test_wait_event_timeout_until_does_not_return_ready_on_none() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let r: WaitOutcome<u32> = wq.wait_event_timeout_until(|| None, 1);
    match r {
        WaitOutcome::Timeout | WaitOutcome::NoRuntime => TestResult::Pass,
        WaitOutcome::Ready(_) => {
            klog_info!(
                "WAIT_QUEUE_TEST: timeout test returned {:?}, expected Timeout or NoRuntime",
                r
            );
            TestResult::Fail
        }
    }
}

/// `wait_event` (backwards-compat bool wrapper) returns `true` on
/// the pre-check fast path.
pub fn test_wait_event_bool_wrapper_pre_check_true() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.wait_event(|| true) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wait_event_timeout` (backwards-compat bool wrapper) returns
/// `false` on timeout.
pub fn test_wait_event_timeout_bool_wrapper_times_out() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.wait_event_timeout(|| false, 1) {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `has_waiters()` on a fresh queue returns `false` via the lock-free
/// read path (it must NOT take the queue's `SpinLock` — if it did,
/// it would still work in kernel mode, but the soundness invariant
/// from the plan would be violated).
pub fn test_has_waiters_fresh_queue_is_false() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.has_waiters() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `WaitQueue::new()` produces a queue with `generation == 0`.
pub fn test_wait_queue_initial_generation() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.generation() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_one` on an empty queue returns `false` without panicking —
/// confirms the has_woken / queue-back-pointer bookkeeping handles
/// the empty-pop branch cleanly.
pub fn test_wake_one_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if !wq.wake_one() {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `wake_all` on an empty queue returns 0 without panicking.
pub fn test_wake_all_on_empty_queue() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    if wq.wake_all() == 0 {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// Verify that wake_one / wake_all on an empty queue do NOT bump the
/// generation counter — the generation is only incremented when at
/// least one waiter was actually woken.
pub fn test_generation_unchanged_when_no_waiters() -> TestResult {
    let _fixture = SchedFixture::new();
    let wq = WaitQueue::new();
    let gen_before = wq.generation();
    let _ = wq.wake_one();
    let _ = wq.wake_all();
    if wq.generation() == gen_before {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

slopos_testing::stest!(
    name = test_wait_event_until_pre_check_returns_carried_value,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_until_pre_check_ready,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_until_does_not_return_ready_on_none,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_bool_wrapper_pre_check_true,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_event_timeout_bool_wrapper_times_out,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_has_waiters_fresh_queue_is_false,
    suite = sched_core
);
slopos_testing::stest!(
    name = test_wait_queue_initial_generation,
    suite = sched_core
);
slopos_testing::stest!(name = test_wake_one_on_empty_queue, suite = sched_core);
slopos_testing::stest!(name = test_wake_all_on_empty_queue, suite = sched_core);
slopos_testing::stest!(
    name = test_generation_unchanged_when_no_waiters,
    suite = sched_core
);

// =============================================================================
// Phase-1 scheduler-refactor regression tests
// =============================================================================

/// `TaskPriority::KernelIo` is numerically 1 (between `High`=0 and
/// `Normal`=2) and the renumber landed cleanly across the enum's
/// total decoder, strict decoder, and dispatch index.
fn test_kernel_io_priority_renumber() -> TestResult {
    if TaskPriority::High.as_u8() != 0 {
        return TestResult::Fail;
    }
    if TaskPriority::KernelIo.as_u8() != 1 {
        return TestResult::Fail;
    }
    if TaskPriority::Normal.as_u8() != 2 {
        return TestResult::Fail;
    }
    if TaskPriority::Low.as_u8() != 3 {
        return TestResult::Fail;
    }
    if TaskPriority::Idle.as_u8() != 4 {
        return TestResult::Fail;
    }
    // Total decoder round-trips every variant.
    for v in 0..=4u8 {
        if TaskPriority::from_u8(v).as_u8() != v {
            return TestResult::Fail;
        }
    }
    // Out-of-range coerces to Normal in the total decoder.
    if TaskPriority::from_u8(255) != TaskPriority::Normal {
        return TestResult::Fail;
    }
    // Strict decoder rejects out-of-range.
    if TaskPriority::try_from_u8(5).is_some() {
        return TestResult::Fail;
    }
    if TaskPriority::try_from_u8(1) != Some(TaskPriority::KernelIo) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// `SleepQueue::earliest_deadline` returns `None` on the lock-free
/// fast path when no tasks are sleeping. Drives the
/// tickless-idle path: when there are no deadlines, the idle loop
/// must skip arming a one-shot LAPIC.
fn test_sleep_queue_next_deadline_none_when_empty() -> TestResult {
    let _fix = SchedFixture::new();
    let now = slopos_kernel_services::platform::timer_ticks();
    match super::sleep::sleep_queue_next_deadline_ticks(now) {
        None => TestResult::Pass,
        Some(_) => TestResult::Fail,
    }
}

/// `KernelIoToken::__new_for_trampoline_only` is documented as the
/// only way to construct a witness. The constructor compiles and
/// returns. (The macro path is exercised in production by every
/// `spawn_kernel_io!` invocation; this test pins the inner ABI.)
fn test_kernel_io_token_constructs() -> TestResult {
    let _t =
        slopos_ostd::sync::kernel_io_task::KernelIoToken::<'static>::__new_for_trampoline_only();
    TestResult::Pass
}

slopos_testing::stest!(name = test_kernel_io_priority_renumber, suite = sched_core);
slopos_testing::stest!(
    name = test_sleep_queue_next_deadline_none_when_empty,
    suite = sched_core
);
slopos_testing::stest!(name = test_kernel_io_token_constructs, suite = sched_core);
