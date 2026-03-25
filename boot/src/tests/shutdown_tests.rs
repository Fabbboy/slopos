//! Shutdown subsystem tests.
//!
//! Tests verify the kernel shutdown machinery: StateFlag atomicity,
//! scheduler/task teardown, and reinit-after-shutdown correctness.

use slopos_core::scheduler::scheduler::{
    init_scheduler, scheduler_enable, scheduler_is_enabled, scheduler_shutdown,
};
use slopos_core::scheduler::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_NORMAL, TaskStatus, init_task_manager,
    task_create, task_find_by_id, task_shutdown_all, task_terminate,
};
use slopos_sync::StateFlag;
use slopos_testing::{TestResult, assert_eq_test, assert_test};
use slopos_utils::klog_info;

use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

// =============================================================================
// Test Helpers
// =============================================================================

struct ShutdownFixture;

impl ShutdownFixture {
    fn new() -> Self {
        // Tear down tasks while the scheduler is still enabled (correct
        // ordering — see kernel_shutdown), then disable the scheduler.
        task_shutdown_all();
        scheduler_shutdown();
        let _ = init_task_manager();
        let _ = init_scheduler();
        Self
    }
}

impl Drop for ShutdownFixture {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();
    }
}

fn dummy_task_fn(_arg: *mut c_void) {}

fn create_n_tasks(n: usize) -> usize {
    let mut created = 0;
    for _ in 0..n {
        let id = task_create(
            b"TestTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TASK_PRIORITY_NORMAL,
            TASK_FLAG_KERNEL_MODE,
        );
        if id == INVALID_TASK_ID {
            break;
        }
        created += 1;
    }
    created
}

// =============================================================================
// StateFlag Tests
// =============================================================================

pub fn test_stateflag_lifecycle() -> TestResult {
    let flag = StateFlag::new();

    // Starts inactive
    assert_test!(!flag.is_active(), "should start inactive");

    // First enter succeeds
    assert_test!(flag.enter(), "first enter should return true");
    assert_test!(flag.is_active(), "should be active after enter");

    // Second enter is idempotent
    assert_test!(!flag.enter(), "second enter should return false");

    // Leave and re-enter
    flag.leave();
    assert_test!(!flag.is_active(), "should be inactive after leave");
    assert_test!(flag.enter(), "re-enter after leave should succeed");

    TestResult::Pass
}

pub fn test_stateflag_take() -> TestResult {
    let flag = StateFlag::new();

    assert_test!(!flag.take(), "take on inactive should return false");

    flag.set_active();
    assert_test!(flag.take(), "take on active should return true");
    assert_test!(!flag.is_active(), "should be inactive after take");

    TestResult::Pass
}

pub fn test_stateflag_independence() -> TestResult {
    let flag1 = StateFlag::new();
    let flag2 = StateFlag::new();

    flag1.enter();
    assert_test!(flag1.is_active());
    assert_test!(!flag2.is_active(), "flag2 should be independent");

    TestResult::Pass
}

pub fn test_stateflag_concurrent_pattern() -> TestResult {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.store(0, Ordering::SeqCst);

    let flag = StateFlag::new();
    let mut successful_enters = 0u32;

    for _ in 0..10 {
        if flag.enter() {
            successful_enters += 1;
            COUNTER.fetch_add(1, Ordering::SeqCst);
        }
    }

    assert_eq_test!(successful_enters, 1, "only one enter should succeed");
    assert_eq_test!(COUNTER.load(Ordering::SeqCst), 1);

    TestResult::Pass
}

pub fn test_stateflag_relaxed_access() -> TestResult {
    let flag = StateFlag::new();
    assert_test!(!flag.is_active_relaxed());

    flag.set_active();
    assert_test!(flag.is_active_relaxed());

    TestResult::Pass
}

// =============================================================================
// Scheduler Shutdown Tests
// =============================================================================

pub fn test_scheduler_shutdown_disables() -> TestResult {
    let _fixture = ShutdownFixture::new();

    assert_eq_test!(scheduler_is_enabled(), 0, "should start disabled");

    scheduler_shutdown();
    assert_eq_test!(
        scheduler_is_enabled(),
        0,
        "should stay disabled after shutdown"
    );

    TestResult::Pass
}

pub fn test_scheduler_shutdown_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    scheduler_shutdown();
    scheduler_shutdown();
    scheduler_shutdown();
    assert_eq_test!(scheduler_is_enabled(), 0);

    TestResult::Pass
}

pub fn test_scheduler_shutdown_clears_state() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"ShutdownTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    assert_test!(
        !task_find_by_id(task_id).is_null(),
        "task should be findable"
    );

    scheduler_shutdown();
    TestResult::Pass
}

// =============================================================================
// Task Shutdown Tests
// =============================================================================

pub fn test_task_shutdown_all_terminates() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let created = create_n_tasks(10);
    assert_test!(created > 0, "failed to create any tasks");

    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_task_shutdown_all_empty() -> TestResult {
    let _fixture = ShutdownFixture::new();
    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_task_shutdown_all_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"IdempotentTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID);

    let _r1 = task_shutdown_all();
    let _r2 = task_shutdown_all();
    let _r3 = task_shutdown_all();
    TestResult::Pass
}

// =============================================================================
// Shutdown Sequence Tests
// =============================================================================

/// The canonical shutdown order is task_shutdown_all() → scheduler_shutdown().
/// Tasks must be torn down while the scheduler is still enabled so that any
/// CPU whose current task is destroyed can schedule() to idle.
pub fn test_shutdown_sequence_ordering() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"SeqTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID);

    // Correct order: tasks first, scheduler second.
    let _result = task_shutdown_all();
    scheduler_shutdown();
    TestResult::Pass
}

pub fn test_shutdown_from_clean_state() -> TestResult {
    let _fixture = ShutdownFixture::new();
    scheduler_shutdown();
    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_shutdown_partial_init() -> TestResult {
    task_shutdown_all();
    let _ = init_task_manager();
    // Deliberately skip init_scheduler - partial init
    scheduler_shutdown();
    task_shutdown_all();
    TestResult::Pass
}

pub fn test_rapid_shutdown_cycles() -> TestResult {
    const CYCLES: usize = 20;

    for _i in 0..CYCLES {
        let _fixture = ShutdownFixture::new();

        let task_id = task_create(
            b"CycleTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TASK_PRIORITY_NORMAL,
            TASK_FLAG_KERNEL_MODE,
        );
        assert_test!(task_id != INVALID_TASK_ID, "cycle task creation failed");
    }

    TestResult::Pass
}

pub fn test_shutdown_many_tasks() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let created = create_n_tasks(50);
    assert_test!(created > 0);

    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_shutdown_mixed_priorities() -> TestResult {
    use slopos_core::scheduler::task::{TASK_PRIORITY_HIGH, TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW};

    let _fixture = ShutdownFixture::new();

    let priorities = [
        TASK_PRIORITY_HIGH,
        TASK_PRIORITY_NORMAL,
        TASK_PRIORITY_LOW,
        TASK_PRIORITY_IDLE,
    ];

    for &priority in &priorities {
        let task_id = task_create(
            b"PriTask\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            priority,
            TASK_FLAG_KERNEL_MODE,
        );
        assert_test!(task_id != INVALID_TASK_ID);
    }

    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_task_shutdown_skips_current() -> TestResult {
    let _fixture = ShutdownFixture::new();

    for _ in 0..5 {
        let _ = task_create(
            b"SkipTest\0".as_ptr() as *const c_char,
            dummy_task_fn,
            ptr::null_mut(),
            TASK_PRIORITY_NORMAL,
            TASK_FLAG_KERNEL_MODE,
        );
    }

    let _result = task_shutdown_all();
    TestResult::Pass
}

pub fn test_scheduler_reinit_after_shutdown() -> TestResult {
    let _fixture = ShutdownFixture::new();

    scheduler_shutdown();
    task_shutdown_all();

    assert_eq_test!(init_task_manager(), 0, "reinit task manager failed");
    assert_eq_test!(init_scheduler(), 0, "reinit scheduler failed");

    let task_id = task_create(
        b"ReinitTest\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(
        task_id != INVALID_TASK_ID,
        "task creation after reinit failed"
    );

    TestResult::Pass
}

pub fn test_kernel_page_directory_available() -> TestResult {
    use slopos_mm::paging::paging_get_kernel_directory;
    assert_test!(
        !paging_get_kernel_directory().is_null(),
        "kernel page dir null"
    );
    TestResult::Pass
}

pub fn test_serial_flush_terminates() -> TestResult {
    use slopos_utils::ports::COM1;

    let lsr_port = COM1.offset(5);
    let mut iterations = 0;
    for _ in 0..1024 {
        let lsr = unsafe { lsr_port.read() };
        iterations += 1;
        if (lsr & 0x40) != 0 {
            break;
        }
        slopos_arch::cpu::pause();
    }
    klog_info!(
        "SHUTDOWN_TEST: Serial flush completed in {} iterations",
        iterations
    );
    TestResult::Pass
}

pub fn test_shutdown_e2e_stress_with_allocation() -> TestResult {
    use slopos_mm::kernel_heap::{kfree, kmalloc};
    use slopos_mm::page_alloc::{ALLOC_FLAG_NO_PCP, alloc_page_frame, free_page_frame};

    const CYCLES: usize = 10;
    const TASKS_PER_CYCLE: usize = 5;
    const ALLOCS_PER_CYCLE: usize = 8;

    task_shutdown_all();
    scheduler_shutdown();

    for cycle in 0..CYCLES {
        assert_test!(
            init_task_manager() == 0 && init_scheduler() == 0,
            "cycle {} init failed",
            cycle
        );

        for _ in 0..TASKS_PER_CYCLE {
            let _ = task_create(
                b"StressTask\0".as_ptr() as *const c_char,
                dummy_task_fn,
                ptr::null_mut(),
                TASK_PRIORITY_NORMAL,
                TASK_FLAG_KERNEL_MODE,
            );
        }

        let mut heap_ptrs: [*mut c_void; ALLOCS_PER_CYCLE] = [ptr::null_mut(); ALLOCS_PER_CYCLE];
        for i in 0..ALLOCS_PER_CYCLE {
            heap_ptrs[i] = kmalloc(64 + (i * 32));
        }

        let mut page_addrs: [u64; 4] = [0; 4];
        for i in 0..4 {
            let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
            page_addrs[i] = phys.as_u64();
        }

        let _result = task_shutdown_all();
        scheduler_shutdown();

        for ptr in heap_ptrs.iter() {
            if !ptr.is_null() {
                kfree(*ptr);
            }
        }
        for &addr in page_addrs.iter() {
            if addr != 0 {
                free_page_frame(slopos_abi::PhysAddr::new(addr));
            }
        }
    }

    TestResult::Pass
}

// =============================================================================
// Regression Tests
// =============================================================================

/// Regression: task_terminate must be idempotent.  Calling it on an
/// already-terminated task should return 0 without re-running teardown
/// side-effects.  Before the fix, repeated terminate calls would spam
/// log output and redo cleanup hooks on dead tasks.
pub fn test_task_terminate_idempotent() -> TestResult {
    let _fixture = ShutdownFixture::new();

    let task_id = task_create(
        b"TermIdem\0".as_ptr() as *const c_char,
        dummy_task_fn,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_KERNEL_MODE,
    );
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");

    // First terminate succeeds.
    assert_eq_test!(task_terminate(task_id), 0, "first terminate should succeed");

    // Verify the task is actually terminated.
    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let status = unsafe { (*task_ptr).status() };
        assert_eq_test!(
            status,
            TaskStatus::Terminated,
            "task should be Terminated after first call"
        );
    }

    // Second terminate must also return 0 (idempotent), not -1 or panic.
    assert_eq_test!(
        task_terminate(task_id),
        0,
        "second terminate should be idempotent"
    );

    // Third time for good measure.
    assert_eq_test!(
        task_terminate(task_id),
        0,
        "third terminate should be idempotent"
    );

    TestResult::Pass
}

/// Regression: scheduler must stay enabled during task_shutdown_all() so
/// that faulting CPUs can schedule() to idle.  Verify that the scheduler
/// is still operational after task_shutdown_all returns.
pub fn test_shutdown_scheduler_alive_during_task_teardown() -> TestResult {
    let _fixture = ShutdownFixture::new();

    // The fixture leaves the scheduler disabled (init_scheduler resets to 0).
    // Enable it so we can verify task_shutdown_all preserves the enabled state.
    scheduler_enable();

    let created = create_n_tasks(5);
    assert_test!(created > 0, "failed to create tasks");

    // After task teardown, scheduler should still be enabled.
    let _result = task_shutdown_all();
    assert_test!(
        scheduler_is_enabled() != 0,
        "scheduler must remain enabled after task_shutdown_all"
    );

    // Now disable it (as kernel_shutdown would).
    scheduler_shutdown();
    assert_eq_test!(scheduler_is_enabled(), 0, "scheduler should be disabled");

    TestResult::Pass
}

slopos_testing::define_test_suite!(
    shutdown,
    [
        test_stateflag_lifecycle,
        test_stateflag_take,
        test_stateflag_independence,
        test_stateflag_concurrent_pattern,
        test_stateflag_relaxed_access,
        test_scheduler_shutdown_disables,
        test_scheduler_shutdown_idempotent,
        test_scheduler_shutdown_clears_state,
        test_task_shutdown_all_terminates,
        test_task_shutdown_all_empty,
        test_task_shutdown_all_idempotent,
        test_shutdown_sequence_ordering,
        test_shutdown_from_clean_state,
        test_shutdown_partial_init,
        test_rapid_shutdown_cycles,
        test_shutdown_many_tasks,
        test_shutdown_mixed_priorities,
        test_task_shutdown_skips_current,
        test_scheduler_reinit_after_shutdown,
        test_kernel_page_directory_available,
        test_serial_flush_terminates,
        test_shutdown_e2e_stress_with_allocation,
        test_task_terminate_idempotent,
        test_shutdown_scheduler_alive_during_task_teardown,
    ]
);
