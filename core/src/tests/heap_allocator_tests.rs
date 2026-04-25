use slopos_testing::{assert_test, TestResult};
use slopos_utils::klog_info;

use crate::exec::spawn_program_with_attrs;
use crate::task::{
    task_get_exit_record, TaskExitRecord, TaskPriority, INVALID_PROCESS_ID, INVALID_TASK_ID,
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

    let mut record = TaskExitRecord::empty();
    if task_get_exit_record(task_id, &mut record) != 0 {
        klog_info!("HEAP_ALLOC_TEST: missing exit record for task {}", task_id);
        return TestResult::Fail;
    }

    assert_test!(
        record.exit_code == 0,
        "heap allocator test suite failed with exit code {}",
        record.exit_code
    );

    TestResult::Pass
}

slopos_testing::define_test_suite!(heap_allocator, [test_heap_allocator_suite,]);
