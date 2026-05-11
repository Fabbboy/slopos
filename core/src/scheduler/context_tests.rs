//! Context switch and task lifecycle edge case tests.

use core::ffi::c_char;
use core::ptr;

use slopos_ostd::KBox;

use super::task_struct::Task;
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskStatus};
use slopos_arch::InterruptFrame;
use slopos_testing::{TestResult, assert_eq_test, assert_not_null, assert_test};
use slopos_utils::klog_info;

use super::scheduler::save_task_context_from_interrupt_frame;
use super::task::{
    MAX_TASKS, task_create, task_find_by_id, task_get_info, task_set_state, task_terminate,
};
use super::task_struct::TaskContext;
use super::test_fixture::KernelTestScope;
use slopos_arch::arch::gdt::SegmentSelector;

struct ContextFixture {
    _scope: KernelTestScope,
}

impl ContextFixture {
    fn new() -> Self {
        Self {
            _scope: KernelTestScope::enter(),
        }
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

/// Compare two arrays of u64 fields by index, with labels supplied as a
/// separate parallel array. Folds dozens of inline `assert_eq_test!`
/// call sites — each of which materialises its own `format_args!` slot
/// in the caller's stack frame — into one materialisation site here,
/// keeping the test fn's frame under the 2 KiB stack-size gate.
#[inline(never)]
fn check_u64_fields(labels: &[&'static str], expected: &[u64], actual: &[u64]) -> Result<(), ()> {
    for ((label, &exp), &act) in labels.iter().zip(expected.iter()).zip(actual.iter()) {
        if exp != act {
            klog_info!("ASSERT_EQ: {} - expected {:#x}, got {:#x}", label, exp, act);
            return Err(());
        }
    }
    Ok(())
}

pub fn test_save_task_context_from_interrupt_frame_marks_started() -> TestResult {
    let mut task: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
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

    save_task_context_from_interrupt_frame(&mut *task, &mut frame, true);

    let user_data = SegmentSelector::USER_DATA.bits() as u64;
    let user_code = SegmentSelector::USER_CODE.bits() as u64;
    // Labels are static — no per-call materialisation cost.
    static LABELS: [&'static str; 25] = [
        "rax",
        "rbx",
        "rcx",
        "rdx",
        "rsi",
        "rdi",
        "r8",
        "r9",
        "r10",
        "r11",
        "r12",
        "r13",
        "r14",
        "r15",
        "rip",
        "rsp",
        "rflags",
        "cs",
        "ss",
        "ds",
        "es",
        "fs",
        "gs",
        "context_from_user",
        "user_started",
    ];
    // Heap-allocate expected/actual u64 arrays so 25 * 8 byte arrays
    // never sit on the test fn's stack frame.
    let mut expected: KBox<[u64; 25]> = KBox::zeroed().expect("alloc");
    let mut actual: KBox<[u64; 25]> = KBox::zeroed().expect("alloc");
    expected[0] = 0xA1;
    actual[0] = task.context.rax;
    expected[1] = 0xB1;
    actual[1] = task.context.rbx;
    expected[2] = 0xC1;
    actual[2] = task.context.rcx;
    expected[3] = 0xD2;
    actual[3] = task.context.rdx;
    expected[4] = 0x51;
    actual[4] = task.context.rsi;
    expected[5] = 0xD1;
    actual[5] = task.context.rdi;
    expected[6] = 0x8;
    actual[6] = task.context.r8;
    expected[7] = 0x9;
    actual[7] = task.context.r9;
    expected[8] = 0x10;
    actual[8] = task.context.r10;
    expected[9] = 0x11;
    actual[9] = task.context.r11;
    expected[10] = 0x12;
    actual[10] = task.context.r12;
    expected[11] = 0x13;
    actual[11] = task.context.r13;
    expected[12] = 0x14;
    actual[12] = task.context.r14;
    expected[13] = 0x15;
    actual[13] = task.context.r15;
    expected[14] = 0x4000;
    actual[14] = task.context.rip;
    expected[15] = 0x8000;
    actual[15] = task.context.rsp;
    expected[16] = 0x202;
    actual[16] = task.context.rflags;
    expected[17] = user_code;
    actual[17] = task.context.cs;
    expected[18] = user_data;
    actual[18] = task.context.ss;
    expected[19] = user_data;
    actual[19] = task.context.ds;
    expected[20] = user_data;
    actual[20] = task.context.es;
    expected[21] = 0;
    actual[21] = task.context.fs;
    expected[22] = 0;
    actual[22] = task.context.gs;
    expected[23] = 1;
    actual[23] = task.context_from_user as u64;
    expected[24] = 1;
    actual[24] = task.user_started as u64;
    if check_u64_fields(&LABELS, &*expected, &*actual).is_err() {
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_save_task_context_from_interrupt_frame_keeps_user_started() -> TestResult {
    let mut task: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
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

    save_task_context_from_interrupt_frame(&mut *task, &mut frame, false);

    assert_eq_test!(task.context_from_user, 1);
    assert_eq_test!(task.user_started, 0);

    TestResult::Pass
}

slopos_testing::stest!(name = test_task_context_initial_state, suite = context);
slopos_testing::stest!(
    name = test_task_state_transitions_exhaustive,
    suite = context
);
slopos_testing::stest!(name = test_task_invalid_state_transition, suite = context);
slopos_testing::stest!(name = test_task_get_info_null_output, suite = context);
slopos_testing::stest!(name = test_task_get_info_invalid_id, suite = context);
slopos_testing::stest!(name = test_task_double_terminate, suite = context);
slopos_testing::stest!(name = test_task_terminate_invalid_ids, suite = context);
slopos_testing::stest!(name = test_task_find_after_terminate, suite = context);
slopos_testing::stest!(name = test_task_rapid_create_terminate, suite = context);
slopos_testing::stest!(name = test_task_process_id_consistency, suite = context);
slopos_testing::stest!(name = test_task_flags_preserved, suite = context);
slopos_testing::stest!(name = test_switch_context_struct_size, suite = context);
slopos_testing::stest!(name = test_switch_context_offsets, suite = context);
slopos_testing::stest!(name = test_switch_context_zero_init, suite = context);
slopos_testing::stest!(name = test_switch_context_setup_initial, suite = context);
slopos_testing::stest!(name = test_task_has_switch_ctx, suite = context);
slopos_testing::stest!(
    name = test_save_task_context_from_interrupt_frame_marks_started,
    suite = context
);
slopos_testing::stest!(
    name = test_save_task_context_from_interrupt_frame_keeps_user_started,
    suite = context
);
