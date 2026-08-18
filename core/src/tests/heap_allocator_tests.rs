use slopos_ostd::klog_info;
use slopos_testing::{TestResult, assert_test};

use crate::exec::spawn_program_with_attrs;
use slopos_sched::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskPriority, task_consume_zombie,
    task_peek_exit_info,
};

const HEAP_TEST_BIN: &[u8] = b"/bin/heap_allocator_test";

pub fn test_heap_allocator_suite() -> TestResult {
    let task_id = match spawn_program_with_attrs(
        HEAP_TEST_BIN,
        None,
        None,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[],
        0,
        INVALID_PROCESS_ID,
        INVALID_TASK_ID,
    ) {
        Ok(id) => id,
        Err(err) => {
            klog_info!("HEAP_ALLOC_TEST: spawn failed ({:?})", err);
            return TestResult::Fail;
        }
    };

    slopos_sched::scheduler::task_wait_for(task_id);

    // The exited child is either an unreaped Zombie (orphaned spawn helper) or
    // already auto-reaped; both branches yield the same `ExitInfo`.
    let info = task_consume_zombie(task_id).or_else(|| task_peek_exit_info(task_id));
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
