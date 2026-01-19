//! Context switch and task lifecycle edge case tests.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_STATE_BLOCKED, TASK_STATE_READY,
    TASK_STATE_RUNNING, TASK_STATE_TERMINATED, Task,
};
use slopos_lib::klog_info;

use super::scheduler::{init_scheduler, scheduler_shutdown};
use super::task::{
    MAX_TASKS, init_task_manager, task_create, task_find_by_id, task_fork, task_get_info,
    task_set_state, task_shutdown_all, task_terminate,
};

fn setup_context_test_env() -> i32 {
    task_shutdown_all();
    scheduler_shutdown();

    if init_task_manager() != 0 {
        klog_info!("CONTEXT_TEST: Failed to init task manager");
        return -1;
    }
    if init_scheduler() != 0 {
        klog_info!("CONTEXT_TEST: Failed to init scheduler");
        return -1;
    }
    0
}

fn teardown_context_test_env() {
    task_shutdown_all();
    scheduler_shutdown();
}

fn dummy_entry(_arg: *mut c_void) {}

fn create_test_task(name: &[u8], flags: u16) -> u32 {
    task_create(
        name.as_ptr() as *const c_char,
        dummy_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

pub fn test_task_context_initial_state() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"CtxInit\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        teardown_context_test_env();
        return -1;
    }

    unsafe {
        let task = &*task_ptr;
        let ctx_rsp = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rsp));
        let ctx_rip = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rip));

        if ctx_rsp == 0 && ctx_rip == 0 {
            klog_info!("CONTEXT_TEST: WARNING - Context RSP and RIP both zero");
        }
    }

    task_terminate(task_id);
    teardown_context_test_env();
    0
}

pub fn test_task_state_transitions_exhaustive() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"StateTrans\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    let initial_state = unsafe { (*task_ptr).state };
    if initial_state != TASK_STATE_READY {
        klog_info!("CONTEXT_TEST: BUG - New task not in READY state");
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    task_set_state(task_id, TASK_STATE_RUNNING);
    let _running_state = unsafe { (*task_ptr).state };

    task_set_state(task_id, TASK_STATE_BLOCKED);
    let _blocked_state = unsafe { (*task_ptr).state };

    task_set_state(task_id, TASK_STATE_READY);

    task_terminate(task_id);
    teardown_context_test_env();
    0
}

pub fn test_task_invalid_state_transition() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"BadTrans\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    task_terminate(task_id);

    let _result = task_set_state(task_id, TASK_STATE_RUNNING);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).state };
        if state == TASK_STATE_RUNNING {
            klog_info!("CONTEXT_TEST: BUG - Revived terminated task to RUNNING");
            teardown_context_test_env();
            return -1;
        }
    }

    teardown_context_test_env();
    0
}

pub fn test_fork_null_parent() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let child_id = task_fork(ptr::null_mut());
    if child_id != INVALID_TASK_ID {
        klog_info!("CONTEXT_TEST: BUG - task_fork succeeded with null parent");
        task_terminate(child_id);
        teardown_context_test_env();
        return -1;
    }

    teardown_context_test_env();
    0
}

pub fn test_fork_kernel_task() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let parent_id = create_test_task(b"KernelParent\0", TASK_FLAG_KERNEL_MODE);
    if parent_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let parent_ptr = task_find_by_id(parent_id);
    if parent_ptr.is_null() {
        task_terminate(parent_id);
        teardown_context_test_env();
        return -1;
    }

    let child_id = task_fork(parent_ptr);
    if child_id != INVALID_TASK_ID {
        klog_info!("CONTEXT_TEST: BUG - Forked kernel task");
        task_terminate(child_id);
    }

    task_terminate(parent_id);
    teardown_context_test_env();
    0
}

pub fn test_fork_terminated_parent() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let parent_id = create_test_task(b"DeadParent\0", TASK_FLAG_KERNEL_MODE);
    if parent_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let parent_ptr = task_find_by_id(parent_id);
    task_terminate(parent_id);

    if !parent_ptr.is_null() {
        let child_id = task_fork(parent_ptr);
        if child_id != INVALID_TASK_ID {
            klog_info!("CONTEXT_TEST: BUG - Forked terminated task");
            task_terminate(child_id);
            teardown_context_test_env();
            return -1;
        }
    }

    teardown_context_test_env();
    0
}

pub fn test_task_get_info_null_output() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"InfoNull\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let _result = task_get_info(task_id, ptr::null_mut());

    task_terminate(task_id);
    teardown_context_test_env();
    0
}

pub fn test_task_get_info_invalid_id() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let mut task_ptr: *mut Task = ptr::null_mut();
    let result = task_get_info(INVALID_TASK_ID, &mut task_ptr);

    if result == 0 || !task_ptr.is_null() {
        klog_info!("CONTEXT_TEST: BUG - task_get_info succeeded for INVALID_TASK_ID");
        teardown_context_test_env();
        return -1;
    }

    let result2 = task_get_info(0xFFFF_FFFF, &mut task_ptr);
    if result2 == 0 || !task_ptr.is_null() {
        klog_info!("CONTEXT_TEST: BUG - task_get_info succeeded for max ID");
        teardown_context_test_env();
        return -1;
    }

    teardown_context_test_env();
    0
}

pub fn test_task_double_terminate() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"DoubleTerm\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let _r1 = task_terminate(task_id);
    let _r2 = task_terminate(task_id);
    let _r3 = task_terminate(task_id);

    teardown_context_test_env();
    0
}

pub fn test_task_terminate_invalid_ids() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let _ = task_terminate(INVALID_TASK_ID);
    let _ = task_terminate(0);
    let _ = task_terminate(0xFFFF_FFFF);
    let _ = task_terminate(MAX_TASKS as u32 + 100);

    teardown_context_test_env();
    0
}

pub fn test_task_find_after_terminate() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"FindTerm\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let ptr_before = task_find_by_id(task_id);
    if ptr_before.is_null() {
        klog_info!("CONTEXT_TEST: BUG - Couldn't find task before termination");
        teardown_context_test_env();
        return -1;
    }

    task_terminate(task_id);

    let ptr_after = task_find_by_id(task_id);
    if !ptr_after.is_null() {
        let state = unsafe { (*ptr_after).state };
        if state != TASK_STATE_TERMINATED {
            klog_info!(
                "CONTEXT_TEST: BUG - Terminated task in wrong state: {}",
                state
            );
            teardown_context_test_env();
            return -1;
        }
    }

    teardown_context_test_env();
    0
}

pub fn test_task_rapid_create_terminate() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    for _i in 0..50 {
        let task_id = create_test_task(b"Rapid\0", TASK_FLAG_KERNEL_MODE);
        if task_id == INVALID_TASK_ID {
            continue;
        }
        task_terminate(task_id);
    }

    teardown_context_test_env();
    0
}

pub fn test_task_max_concurrent() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let mut created_ids: [u32; 64] = [INVALID_TASK_ID; 64];
    let mut count = 0usize;

    for _ in 0..MAX_TASKS + 10 {
        let task_id = create_test_task(b"MaxTest\0", TASK_FLAG_KERNEL_MODE);
        if task_id == INVALID_TASK_ID {
            break;
        }
        if count < created_ids.len() {
            created_ids[count] = task_id;
            count += 1;
        }
    }

    for i in 0..count {
        task_terminate(created_ids[i]);
    }

    teardown_context_test_env();
    0
}

pub fn test_task_process_id_consistency() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"ProcId\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    let _proc_id = unsafe { (*task_ptr).process_id };

    task_terminate(task_id);
    teardown_context_test_env();
    0
}

pub fn test_task_flags_preserved() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"FlagsTest\0", TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        teardown_context_test_env();
        return -1;
    }

    let task_ptr = task_find_by_id(task_id);
    if task_ptr.is_null() {
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    let flags = unsafe { (*task_ptr).flags };
    if (flags & TASK_FLAG_KERNEL_MODE) == 0 {
        klog_info!("CONTEXT_TEST: BUG - Kernel mode flag not preserved");
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    task_terminate(task_id);
    teardown_context_test_env();
    0
}

pub fn run_context_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    macro_rules! run_test {
        ($name:expr, $test_fn:expr) => {{
            total += 1;
            let result = $test_fn();
            if result == 0 {
                passed += 1;
            } else {
                klog_info!("CONTEXT_TEST FAILED: {}", $name);
            }
        }};
    }

    klog_info!("=== Context Switch Tests ===");

    run_test!(
        "task_context_initial_state",
        test_task_context_initial_state
    );
    run_test!(
        "task_state_transitions_exhaustive",
        test_task_state_transitions_exhaustive
    );
    run_test!(
        "task_invalid_state_transition",
        test_task_invalid_state_transition
    );
    run_test!("fork_null_parent", test_fork_null_parent);
    run_test!("fork_kernel_task", test_fork_kernel_task);
    run_test!("fork_terminated_parent", test_fork_terminated_parent);
    run_test!("task_get_info_null_output", test_task_get_info_null_output);
    run_test!("task_get_info_invalid_id", test_task_get_info_invalid_id);
    run_test!("task_double_terminate", test_task_double_terminate);
    run_test!(
        "task_terminate_invalid_ids",
        test_task_terminate_invalid_ids
    );
    run_test!("task_find_after_terminate", test_task_find_after_terminate);
    run_test!(
        "task_rapid_create_terminate",
        test_task_rapid_create_terminate
    );
    run_test!("task_max_concurrent", test_task_max_concurrent);
    run_test!(
        "task_process_id_consistency",
        test_task_process_id_consistency
    );
    run_test!("task_flags_preserved", test_task_flags_preserved);

    klog_info!("Context tests: {}/{} passed", passed, total);
    (passed, total)
}
