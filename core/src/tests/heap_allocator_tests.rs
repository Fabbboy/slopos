use slopos_testing::{assert_test, TestResult};
use slopos_utils::klog_info;

use crate::exec::spawn_program_with_attrs;
use crate::task::{
    task_consume_zombie, task_peek_exit_info, TaskPriority, INVALID_PROCESS_ID, INVALID_TASK_ID,
    TASK_FLAG_USER_MODE,
};

const HEAP_TEST_BIN: &[u8] = b"/bin/heap_allocator_test";

pub fn test_heap_allocator_suite() -> TestResult {
    let task_id = match spawn_program_with_attrs(
        HEAP_TEST_BIN,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        INVALID_PROCESS_ID,
        INVALID_TASK_ID,
    ) {
        Ok(id) => id,
        Err(err) => {
            klog_info!("HEAP_ALLOC_TEST: spawn failed ({:?})", err);
            return TestResult::Fail;
        }
    };

    crate::sched::task_wait_for(task_id);

    // After `task_wait_for` returns the child has exited. It is either
    // a Zombie (orphaned spawn helper — no live parent) waiting to be
    // reaped, or already Terminated (auto-reaped). Either branch yields
    // the same `ExitInfo`.
    let info = task_consume_zombie(task_id)
        .or_else(|| task_peek_exit_info(task_id));
    let Some(info) = info else {
        klog_info!("HEAP_ALLOC_TEST: missing exit info for task {}", task_id);
        return TestResult::Fail;
    };

    assert_test!(
        info.exit_code == 0,
        "heap allocator test suite failed with exit code {}",
        info.exit_code
    );

    TestResult::Pass
}

slopos_testing::stest!(name = test_heap_allocator_suite, suite = heap_allocator);
