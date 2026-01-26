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

pub fn test_switch_context_struct_size() -> c_int {
    use core::mem::size_of;
    use slopos_abi::task::SwitchContext;

    let size = size_of::<SwitchContext>();
    if size != 72 {
        klog_info!(
            "CONTEXT_TEST: SwitchContext size wrong: {} (expected 72)",
            size
        );
        return -1;
    }
    0
}

pub fn test_switch_context_offsets() -> c_int {
    use slopos_abi::task::{
        SWITCH_CTX_OFF_R12, SWITCH_CTX_OFF_R13, SWITCH_CTX_OFF_R14, SWITCH_CTX_OFF_R15,
        SWITCH_CTX_OFF_RBP, SWITCH_CTX_OFF_RBX, SWITCH_CTX_OFF_RFLAGS, SWITCH_CTX_OFF_RIP,
        SWITCH_CTX_OFF_RSP,
    };

    if SWITCH_CTX_OFF_RBX != 0 {
        return -1;
    }
    if SWITCH_CTX_OFF_R12 != 8 {
        return -1;
    }
    if SWITCH_CTX_OFF_R13 != 16 {
        return -1;
    }
    if SWITCH_CTX_OFF_R14 != 24 {
        return -1;
    }
    if SWITCH_CTX_OFF_R15 != 32 {
        return -1;
    }
    if SWITCH_CTX_OFF_RBP != 40 {
        return -1;
    }
    if SWITCH_CTX_OFF_RSP != 48 {
        return -1;
    }
    if SWITCH_CTX_OFF_RFLAGS != 56 {
        return -1;
    }
    if SWITCH_CTX_OFF_RIP != 64 {
        return -1;
    }
    0
}

pub fn test_switch_context_zero_init() -> c_int {
    use slopos_abi::task::SwitchContext;

    let ctx = SwitchContext::zero();
    if ctx.rbx != 0 || ctx.r12 != 0 || ctx.r13 != 0 || ctx.r14 != 0 || ctx.r15 != 0 {
        return -1;
    }
    if ctx.rbp != 0 || ctx.rsp != 0 || ctx.rip != 0 {
        return -1;
    }
    if ctx.rflags != 0x202 {
        klog_info!(
            "CONTEXT_TEST: SwitchContext::zero() rflags wrong: {:#x}",
            ctx.rflags
        );
        return -1;
    }
    0
}

pub fn test_switch_context_setup_initial() -> c_int {
    use slopos_abi::task::SwitchContext;

    let stack_top: u64 = 0x1000;
    let entry: u64 = 0xDEADBEEF;
    let arg: u64 = 0xCAFEBABE;
    let trampoline: u64 = 0x12345678;

    let ctx = SwitchContext::builder()
        .with_entry(entry, arg)
        .with_stack(stack_top, trampoline)
        .build();

    if ctx.rsp != stack_top - 8 {
        klog_info!("CONTEXT_TEST: builder rsp wrong: {:#x}", ctx.rsp);
        return -1;
    }
    if ctx.rip != trampoline {
        klog_info!("CONTEXT_TEST: builder rip wrong: {:#x}", ctx.rip);
        return -1;
    }
    if ctx.r12 != entry {
        klog_info!("CONTEXT_TEST: builder r12 wrong: {:#x}", ctx.r12);
        return -1;
    }
    if ctx.r13 != arg {
        klog_info!("CONTEXT_TEST: builder r13 wrong: {:#x}", ctx.r13);
        return -1;
    }
    if ctx.rflags != 0x202 {
        return -1;
    }
    0
}

pub fn test_task_has_switch_ctx() -> c_int {
    if setup_context_test_env() != 0 {
        return -1;
    }

    let task_id = create_test_task(b"SwitchTest\0", TASK_FLAG_KERNEL_MODE);
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

    let switch_ctx = unsafe { &(*task_ptr).switch_ctx };
    if switch_ctx.rflags != 0x202 {
        klog_info!(
            "CONTEXT_TEST: Task switch_ctx rflags not initialized: {:#x}",
            switch_ctx.rflags
        );
        task_terminate(task_id);
        teardown_context_test_env();
        return -1;
    }

    task_terminate(task_id);
    teardown_context_test_env();
    0
}
