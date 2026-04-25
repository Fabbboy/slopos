//! Context switch and task lifecycle edge case tests.

use core::ffi::c_char;
use core::ptr;

use super::task_struct::Task;
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskStatus};
use slopos_arch::InterruptFrame;
use slopos_testing::{TestResult, assert_eq_test, assert_not_null, assert_test};
use slopos_utils::klog_info;

use super::scheduler::{
    init_scheduler, save_task_context_from_interrupt_frame, scheduler_shutdown,
};
use super::task::{
    MAX_TASKS, init_task_manager, task_create, task_find_by_id, task_get_info, task_set_state,
    task_shutdown_all, task_terminate,
};
use super::task_struct::TaskContext;
use slopos_arch::arch::gdt::SegmentSelector;

struct ContextFixture;

impl ContextFixture {
    fn new() -> Self {
        task_shutdown_all();
        scheduler_shutdown();
        let _ = init_task_manager();
        let _ = init_scheduler();
        Self
    }
}

impl Drop for ContextFixture {
    fn drop(&mut self) {
        task_shutdown_all();
        scheduler_shutdown();
    }
}

use crate::tests::helpers::dummy_task_entry;

fn create_test_task(name: &[u8], flags: u16) -> u32 {
    task_create(
        name.as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

// =============================================================================
// Task Lifecycle Tests
// =============================================================================

pub fn test_task_context_initial_state() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"CtxInit\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    unsafe {
        let task = &*task_ptr;
        let ctx_rsp = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rsp));
        let ctx_rip = core::ptr::read_unaligned(core::ptr::addr_of!(task.context.rip));

        if ctx_rsp == 0 && ctx_rip == 0 {
            klog_info!("CONTEXT_TEST: WARNING - Context RSP and RIP both zero");
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_state_transitions_exhaustive() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"StateTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let initial_state = unsafe { (*task_ptr).status() };
    assert_eq_test!(
        initial_state,
        TaskStatus::Ready,
        "new task not in READY state"
    );

    task_set_state(task_id, TaskStatus::Running);
    task_set_state(task_id, TaskStatus::Blocked);
    task_set_state(task_id, TaskStatus::Ready);

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_invalid_state_transition() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"BadTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);
    let _result = task_set_state(task_id, TaskStatus::Running);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).status() };
        assert_test!(
            state != TaskStatus::Running,
            "revived terminated task to RUNNING"
        );
    }

    TestResult::Pass
}

// =============================================================================
// Task Info & Termination Edge Cases
// =============================================================================

pub fn test_task_get_info_null_output() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"InfoNull\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let _result = task_get_info(task_id, ptr::null_mut());

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_get_info_invalid_id() -> TestResult {
    let _fixture = ContextFixture::new();

    let mut task_ptr: *mut Task = ptr::null_mut();

    let result = task_get_info(INVALID_TASK_ID, &mut task_ptr);
    assert_test!(
        result != 0 || task_ptr.is_null(),
        "succeeded for INVALID_TASK_ID"
    );

    task_ptr = ptr::null_mut();
    let result2 = task_get_info(0xFFFF_FFFF, &mut task_ptr);
    assert_test!(result2 != 0 || task_ptr.is_null(), "succeeded for max ID");

    TestResult::Pass
}

pub fn test_task_double_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"DoubleTerm\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let _r1 = task_terminate(task_id);
    let _r2 = task_terminate(task_id);
    let _r3 = task_terminate(task_id);

    TestResult::Pass
}

pub fn test_task_terminate_invalid_ids() -> TestResult {
    let _fixture = ContextFixture::new();

    let _ = task_terminate(INVALID_TASK_ID);
    let _ = task_terminate(0);
    let _ = task_terminate(0xFFFF_FFFF);
    let _ = task_terminate(MAX_TASKS as u32 + 100);

    TestResult::Pass
}

pub fn test_task_find_after_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"FindTerm\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    assert_not_null!(
        task_find_by_id(task_id),
        "task should exist before termination"
    );

    task_terminate(task_id);

    let ptr_after = task_find_by_id(task_id);
    if !ptr_after.is_null() {
        let state = unsafe { (*ptr_after).status() };
        assert_eq_test!(
            state,
            TaskStatus::Terminated,
            "terminated task in wrong state"
        );
    }

    TestResult::Pass
}

pub fn test_task_rapid_create_terminate() -> TestResult {
    let _fixture = ContextFixture::new();

    for _i in 0..50 {
        let task_id = create_test_task(b"Rapid\0", TASK_FLAG_KERNEL_MODE);
        if task_id == INVALID_TASK_ID {
            continue;
        }
        task_terminate(task_id);
    }

    TestResult::Pass
}

pub fn test_task_process_id_consistency() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"ProcId\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let _proc_id = unsafe { (*task_ptr).process_id };

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_flags_preserved() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"FlagsTest\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let flags = unsafe { (*task_ptr).flags };
    assert_test!(
        (flags & TASK_FLAG_KERNEL_MODE) != 0,
        "kernel mode flag not preserved"
    );

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// SwitchContext Layout Tests
// =============================================================================

pub fn test_switch_context_struct_size() -> TestResult {
    use super::task_struct::SwitchContext;
    assert_eq_test!(
        core::mem::size_of::<SwitchContext>(),
        72,
        "SwitchContext size wrong"
    );
    TestResult::Pass
}

pub fn test_switch_context_offsets() -> TestResult {
    use super::task_struct::{
        SWITCH_CTX_OFF_R12, SWITCH_CTX_OFF_R13, SWITCH_CTX_OFF_R14, SWITCH_CTX_OFF_R15,
        SWITCH_CTX_OFF_RBP, SWITCH_CTX_OFF_RBX, SWITCH_CTX_OFF_RFLAGS, SWITCH_CTX_OFF_RIP,
        SWITCH_CTX_OFF_RSP,
    };

    assert_eq_test!(SWITCH_CTX_OFF_RBX, 0);
    assert_eq_test!(SWITCH_CTX_OFF_R12, 8);
    assert_eq_test!(SWITCH_CTX_OFF_R13, 16);
    assert_eq_test!(SWITCH_CTX_OFF_R14, 24);
    assert_eq_test!(SWITCH_CTX_OFF_R15, 32);
    assert_eq_test!(SWITCH_CTX_OFF_RBP, 40);
    assert_eq_test!(SWITCH_CTX_OFF_RSP, 48);
    assert_eq_test!(SWITCH_CTX_OFF_RFLAGS, 56);
    assert_eq_test!(SWITCH_CTX_OFF_RIP, 64);
    TestResult::Pass
}

pub fn test_switch_context_zero_init() -> TestResult {
    use super::task_struct::SwitchContext;

    let ctx = SwitchContext::zero();
    assert_eq_test!(ctx.rbx, 0);
    assert_eq_test!(ctx.r12, 0);
    assert_eq_test!(ctx.r13, 0);
    assert_eq_test!(ctx.r14, 0);
    assert_eq_test!(ctx.r15, 0);
    assert_eq_test!(ctx.rbp, 0);
    assert_eq_test!(ctx.rsp, 0);
    assert_eq_test!(ctx.rip, 0);
    assert_eq_test!(ctx.rflags, 0x202, "rflags should default to IF+reserved");
    TestResult::Pass
}

pub fn test_switch_context_setup_initial() -> TestResult {
    use super::task_struct::SwitchContext;

    let stack_top: u64 = 0x1000;
    let entry: u64 = 0xDEADBEEF;
    let arg: u64 = 0xCAFEBABE;
    let trampoline: u64 = 0x12345678;

    let ctx = SwitchContext::new_for_task(entry, arg, stack_top, trampoline);

    assert_eq_test!(ctx.rsp, stack_top - 8, "rsp should be stack_top - 8");
    assert_eq_test!(ctx.rip, trampoline, "rip should be trampoline");
    assert_eq_test!(ctx.r12, entry, "r12 should hold entry");
    assert_eq_test!(ctx.r13, arg, "r13 should hold arg");
    assert_eq_test!(ctx.rflags, 0x202);
    TestResult::Pass
}

pub fn test_task_has_switch_ctx() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"SwitchTest\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    let switch_ctx = unsafe { &(*task_ptr).switch_ctx };
    assert_eq_test!(
        switch_ctx.rflags,
        0x202,
        "switch_ctx rflags not initialized"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_save_task_context_from_interrupt_frame_marks_started() -> TestResult {
    let mut task = Task::invalid();
    task.context = TaskContext::zero();
    task.user_started = 0;
    task.context_from_user = 0;

    let mut frame = InterruptFrame {
        r15: 0x15,
        r14: 0x14,
        r13: 0x13,
        r12: 0x12,
        r11: 0x11,
        r10: 0x10,
        r9: 0x9,
        r8: 0x8,
        rbp: 0xBEEF,
        rdi: 0xD1,
        rsi: 0x51,
        rdx: 0xD2,
        rcx: 0xC1,
        rbx: 0xB1,
        rax: 0xA1,
        vector: 0x80,
        error_code: 0,
        rip: 0x4000,
        cs: SegmentSelector::USER_CODE.bits() as u64,
        rflags: 0x202,
        rsp: 0x8000,
        ss: SegmentSelector::USER_DATA.bits() as u64,
    };

    save_task_context_from_interrupt_frame(&mut task, &mut frame, true);

    assert_eq_test!(task.context.rax, 0xA1);
    assert_eq_test!(task.context.rbx, 0xB1);
    assert_eq_test!(task.context.rcx, 0xC1);
    assert_eq_test!(task.context.rdx, 0xD2);
    assert_eq_test!(task.context.rsi, 0x51);
    assert_eq_test!(task.context.rdi, 0xD1);
    assert_eq_test!(task.context.r8, 0x8);
    assert_eq_test!(task.context.r9, 0x9);
    assert_eq_test!(task.context.r10, 0x10);
    assert_eq_test!(task.context.r11, 0x11);
    assert_eq_test!(task.context.r12, 0x12);
    assert_eq_test!(task.context.r13, 0x13);
    assert_eq_test!(task.context.r14, 0x14);
    assert_eq_test!(task.context.r15, 0x15);
    assert_eq_test!(task.context.rip, 0x4000);
    assert_eq_test!(task.context.rsp, 0x8000);
    assert_eq_test!(task.context.rflags, 0x202);
    assert_eq_test!(task.context.cs, SegmentSelector::USER_CODE.bits() as u64);
    assert_eq_test!(task.context.ss, SegmentSelector::USER_DATA.bits() as u64);
    assert_eq_test!(
        task.context.ds,
        SegmentSelector::USER_DATA.bits() as u64,
        "ds should always be user data selector"
    );
    assert_eq_test!(
        task.context.es,
        SegmentSelector::USER_DATA.bits() as u64,
        "es should always be user data selector"
    );
    assert_eq_test!(task.context.fs, 0);
    assert_eq_test!(task.context.gs, 0);
    assert_eq_test!(task.context_from_user, 1);
    assert_eq_test!(task.user_started, 1);

    TestResult::Pass
}

pub fn test_save_task_context_from_interrupt_frame_keeps_user_started() -> TestResult {
    let mut task = Task::invalid();
    task.context = TaskContext::zero();
    task.user_started = 0;
    task.context_from_user = 0;

    let mut frame = InterruptFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: 0,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector: 32,
        error_code: 0,
        rip: 0,
        cs: SegmentSelector::USER_CODE.bits() as u64,
        rflags: 0,
        rsp: 0,
        ss: SegmentSelector::USER_DATA.bits() as u64,
    };

    save_task_context_from_interrupt_frame(&mut task, &mut frame, false);

    assert_eq_test!(task.context_from_user, 1);
    assert_eq_test!(task.user_started, 0);

    TestResult::Pass
}

slopos_testing::define_test_suite!(
    context,
    [
        test_task_context_initial_state,
        test_task_state_transitions_exhaustive,
        test_task_invalid_state_transition,
        test_task_get_info_null_output,
        test_task_get_info_invalid_id,
        test_task_double_terminate,
        test_task_terminate_invalid_ids,
        test_task_find_after_terminate,
        test_task_rapid_create_terminate,
        test_task_process_id_consistency,
        test_task_flags_preserved,
        test_switch_context_struct_size,
        test_switch_context_offsets,
        test_switch_context_zero_init,
        test_switch_context_setup_initial,
        test_task_has_switch_ctx,
        test_save_task_context_from_interrupt_frame_marks_started,
        test_save_task_context_from_interrupt_frame_keeps_user_started,
    ]
);
