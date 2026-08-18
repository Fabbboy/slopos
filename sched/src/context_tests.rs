//! Context switch and task lifecycle edge case tests.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_ostd::KBox;

use super::task_struct::Task;
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TaskStatus};
use slopos_arch::InterruptFrame;
use slopos_ostd::klog_info;
use slopos_testing::{TestResult, assert_eq_test, assert_some, assert_test};

use super::task::{MAX_TASKS, task_create, task_find_by_id, task_set_state, task_terminate};
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

use crate::test_fixture::dummy_task_entry;

fn create_test_task(name: &[u8], flags: u16) -> u32 {
    task_create(
        name.as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

pub fn test_task_context_initial_state() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"CtxInit\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task = assert_some!(task_find_by_id(task_id));

    // Diagnostic reads of a live task, so the racy accessors off a shared guard
    // rather than a witness.
    let ctx_rsp = task.context_rsp();
    let ctx_rip = task.context_rip();
    if ctx_rsp == 0 && ctx_rip == 0 {
        klog_info!("CONTEXT_TEST: WARNING - Context RSP and RIP both zero");
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_state_transitions_exhaustive() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"StateTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task = assert_some!(task_find_by_id(task_id));
    let task_ptr: &Task = &task;

    assert_eq_test!(
        task.status(),
        TaskStatus::Blocked,
        "new task not in BLOCKED/non-runnable state"
    );

    task_set_state(task_id, TaskStatus::Ready);
    assert_eq_test!(task_ptr.status(), TaskStatus::Ready);
    task_set_state(task_id, TaskStatus::Running);
    assert_eq_test!(task_ptr.status(), TaskStatus::Running);
    task_set_state(task_id, TaskStatus::Blocked);
    assert_eq_test!(task_ptr.status(), TaskStatus::Blocked);

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_invalid_state_transition() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"BadTrans\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);
    let _result = task_set_state(task_id, TaskStatus::Running);

    if let Some(task) = task_find_by_id(task_id) {
        let state = task.status();
        assert_test!(
            state != TaskStatus::Running,
            "revived terminated task to RUNNING"
        );
    }

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

    assert_test!(
        task_find_by_id(task_id).is_some(),
        "task should exist before termination"
    );

    task_terminate(task_id);

    if let Some(task) = task_find_by_id(task_id) {
        let state = task.status();
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

    let task = assert_some!(task_find_by_id(task_id));
    let _proc_id = task.process_id;

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_task_flags_preserved() -> TestResult {
    let _fixture = ContextFixture::new();

    let task_id = create_test_task(b"FlagsTest\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(task_id != INVALID_TASK_ID);

    let task = assert_some!(task_find_by_id(task_id));
    let flags = task.flags;
    assert_test!(
        (flags & TASK_FLAG_KERNEL_MODE) != 0,
        "kernel mode flag not preserved"
    );

    task_terminate(task_id);
    TestResult::Pass
}

// No layout (size/offset) stests here on purpose: `SwitchContext` aliases
// `slopos_ostd::task::TaskContext`, whose size and every field offset are
// pinned by `const _` asserts beside the struct definition in all build
// configurations. A runtime duplicate can only agree or drift stale.

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

    let task = assert_some!(task_find_by_id(task_id));
    let rflags = task.switch_ctx_rflags();
    assert_eq_test!(rflags, 0x202, "switch_ctx rflags not initialized");

    task_terminate(task_id);
    TestResult::Pass
}

/// Compare u64 fields by index against a parallel label array. Dozens of inline
/// `assert_eq_test!` sites would each materialise a `format_args!` slot in the
/// caller's frame; folding them here keeps the test fn under the 2 KiB gate.
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
    *task.context.get_mut() = TaskContext::zero();
    *task.user_started.get_mut() = 0;
    *task.context_from_user.get_mut() = 0;

    let frame = InterruptFrame {
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

    task.save_from_interrupt_frame_mut(&frame, true);

    let user_data = SegmentSelector::USER_DATA.bits() as u64;
    let user_code = SegmentSelector::USER_CODE.bits() as u64;
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
    // Heap-allocated so two 25 * 8 byte arrays never sit on this frame.
    let mut expected: KBox<[u64; 25]> = KBox::zeroed().expect("alloc");
    let mut actual: KBox<[u64; 25]> = KBox::zeroed().expect("alloc");
    expected[0] = 0xA1;
    actual[0] = task.context.get_mut().rax;
    expected[1] = 0xB1;
    actual[1] = task.context.get_mut().rbx;
    expected[2] = 0xC1;
    actual[2] = task.context.get_mut().rcx;
    expected[3] = 0xD2;
    actual[3] = task.context.get_mut().rdx;
    expected[4] = 0x51;
    actual[4] = task.context.get_mut().rsi;
    expected[5] = 0xD1;
    actual[5] = task.context.get_mut().rdi;
    expected[6] = 0x8;
    actual[6] = task.context.get_mut().r8;
    expected[7] = 0x9;
    actual[7] = task.context.get_mut().r9;
    expected[8] = 0x10;
    actual[8] = task.context.get_mut().r10;
    expected[9] = 0x11;
    actual[9] = task.context.get_mut().r11;
    expected[10] = 0x12;
    actual[10] = task.context.get_mut().r12;
    expected[11] = 0x13;
    actual[11] = task.context.get_mut().r13;
    expected[12] = 0x14;
    actual[12] = task.context.get_mut().r14;
    expected[13] = 0x15;
    actual[13] = task.context.get_mut().r15;
    expected[14] = 0x4000;
    actual[14] = task.context.get_mut().rip;
    expected[15] = 0x8000;
    actual[15] = task.context.get_mut().rsp;
    expected[16] = 0x202;
    actual[16] = task.context.get_mut().rflags;
    expected[17] = user_code;
    actual[17] = task.context.get_mut().cs;
    expected[18] = user_data;
    actual[18] = task.context.get_mut().ss;
    expected[19] = user_data;
    actual[19] = task.context.get_mut().ds;
    expected[20] = user_data;
    actual[20] = task.context.get_mut().es;
    expected[21] = 0;
    actual[21] = task.context.get_mut().fs;
    expected[22] = 0;
    actual[22] = task.context.get_mut().gs;
    expected[23] = 1;
    actual[23] = task.context_from_user.load(Ordering::Relaxed) as u64;
    expected[24] = 1;
    actual[24] = task.user_started.load(Ordering::Relaxed) as u64;
    if check_u64_fields(&LABELS, &*expected, &*actual).is_err() {
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_save_task_context_from_interrupt_frame_keeps_user_started() -> TestResult {
    let mut task: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
    *task.context.get_mut() = TaskContext::zero();
    *task.user_started.get_mut() = 0;
    *task.context_from_user.get_mut() = 0;

    let frame = InterruptFrame {
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

    task.save_from_interrupt_frame_mut(&frame, false);

    assert_eq_test!(task.context_from_user.load(Ordering::Relaxed), 1);
    assert_eq_test!(task.user_started.load(Ordering::Relaxed), 0);

    TestResult::Pass
}

slopos_testing::stest!(name = test_task_context_initial_state, suite = context);
slopos_testing::stest!(
    name = test_task_state_transitions_exhaustive,
    suite = context
);
slopos_testing::stest!(name = test_task_invalid_state_transition, suite = context);
slopos_testing::stest!(name = test_task_double_terminate, suite = context);
slopos_testing::stest!(name = test_task_terminate_invalid_ids, suite = context);
slopos_testing::stest!(name = test_task_find_after_terminate, suite = context);
slopos_testing::stest!(name = test_task_rapid_create_terminate, suite = context);
slopos_testing::stest!(name = test_task_process_id_consistency, suite = context);

// The FPU tests below drive the buffer the context switch routes through — each
// task's own `fpu_state` cell — rather than the XSAVE/XRSTOR instructions,
// which `xsave_tests.rs` covers.
//
// Each begins by restoring a scratch task, which is not redundant:
// `fpu_owner_assert_may_take` exempts a task whose `fpu_last_cpu` is still
// `FPU_CPU_NONE`, which is exactly the state of a fresh `KBox<Task>`, so a test
// that saved straight into one would exercise the exempt path. The restore is
// what gives the task an ownership claim so the saves below run checked.
//
// Interrupts stay off because the sequences leave a pattern in the live register
// file across several calls, and a context switch in that window would save and
// restore the whole file underneath them. Each test also snapshots the live
// state on entry and puts it back on exit.

/// Scratch XSAVE image parking the running task's live vector state for the
/// duration of a test. Heap-allocated: `FpuState` is 2.6 KiB against a 2 KiB
/// stack-frame gate.
fn snapshot_live_fpu(xcr0: u64) -> KBox<slopos_ostd::task::FpuState> {
    let mut area: KBox<slopos_ostd::task::FpuState> =
        KBox::try_init(slopos_ostd::task::FpuState::init_zero()).expect("alloc");
    area.save_current(xcr0);
    area
}

/// Four distinguishable 128-bit patterns.
fn patterns_a() -> [slopos_ostd::test_support::cpu_state::Xmm128; 4] {
    [
        [0xA0A0_0000_0000_0001, 0xA0A0_0000_0000_0002],
        [0xA0A0_0000_0000_0003, 0xA0A0_0000_0000_0004],
        [0xA0A0_0000_0000_0005, 0xA0A0_0000_0000_0006],
        [0xA0A0_0000_0000_0007, 0xA0A0_0000_0000_0008],
    ]
}

/// Four patterns sharing no byte with [`patterns_a`], so a bleed is visible
/// rather than plausible.
fn patterns_b() -> [slopos_ostd::test_support::cpu_state::Xmm128; 4] {
    [
        [0xB1B1_FFFF_FFFF_1001, 0xB1B1_FFFF_FFFF_1002],
        [0xB1B1_FFFF_FFFF_1003, 0xB1B1_FFFF_FFFF_1004],
        [0xB1B1_FFFF_FFFF_1005, 0xB1B1_FFFF_FFFF_1006],
        [0xB1B1_FFFF_FFFF_1007, 0xB1B1_FFFF_FFFF_1008],
    ]
}

/// Two tasks' save areas are independent: writing B's does not disturb A's.
/// Fails if a save or restore is ever routed through the wrong task's cell.
pub fn test_fpu_per_task_slot_isolation() -> TestResult {
    use slopos_ostd::test_support::cpu_state as cpu;

    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
    let mut a: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
    let mut b: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
    let pat_a = patterns_a();
    let pat_b = patterns_b();

    let (outcome, accepted) = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let saved = snapshot_live_fpu(xcr0);

        // Every image restored below is one this test authored, so a rejection
        // is a defect in the save path rather than a hostile image.
        let mut accepted = true;

        accepted &= a.fpu_restore_to_cpu_mut(xcr0);
        cpu::xmm_load_4(&pat_a);
        a.fpu_save_in_place_mut(xcr0);

        // If the two slots aliased, this is the write that would destroy A's.
        accepted &= b.fpu_restore_to_cpu_mut(xcr0);
        cpu::xmm_load_4(&pat_b);
        b.fpu_save_in_place_mut(xcr0);

        // Zero first, so a pass proves the restore did the work rather than the
        // pattern never having left the registers.
        cpu::xmm_zero_4();
        accepted &= a.fpu_restore_to_cpu_mut(xcr0);
        let readback = cpu::xmm_read_4();

        // Leave no CPU naming a task that is about to be freed.
        slopos_ostd::task::fpu_owner_forget(&*a);
        slopos_ostd::task::fpu_owner_forget(&*b);
        accepted &= saved.restore_to_cpu(xcr0);
        (readback, accepted)
    });

    assert_test!(
        accepted,
        "a task-authored FPU image was rejected by XRSTOR64"
    );

    for i in 0..4 {
        if outcome[i] != pat_a[i] {
            klog_info!(
                "FPU_SLOT: register {} came back {:#x?}, expected A's {:#x?} \
                 (B's is {:#x?}) — task slots are not isolated",
                i,
                outcome[i],
                pat_a[i],
                pat_b[i],
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// The dispatcher's save-prev / restore-next pair, run through `run_switch`
/// with the incoming task published inside the window.
///
/// That ordering is the whole reason `SwitchWindow` exists: the incoming task
/// reaches the PCR *before* the outgoing task's registers are saved, so
/// `CurrentTask` no longer names the task whose vector state is about to be
/// written. Publication sits in `run_switch`'s `publish` step, as in production,
/// because it also swaps the SafeStack data stack and so needs the window open.
pub fn test_fpu_switch_saves_prev_and_restores_next() -> TestResult {
    use slopos_ostd::task::SchedPlacement;
    use slopos_ostd::task::fpu_current_cpu;
    use slopos_ostd::test_support::cpu_state as cpu;

    let _fixture = ContextFixture::new();

    // The incoming end has to be a task the PCR genuinely names, and in the
    // kernel test phase the BSP is parked on a bootstrap stub where
    // `Current::get()` is `None`, so a real registered task is dispatched
    // through the sanctioned test hook. Forging `PCR.current_task` would leave
    // the genuinely running task looking un-pinned to `task_is_dispatch_pinned`,
    // and a concurrent reap could free it underneath this test.
    let next_id = create_test_task(b"FpuSwitch\0", TASK_FLAG_KERNEL_MODE);
    assert_test!(next_id != INVALID_TASK_ID, "failed to create incoming task");
    let next_guard = assert_some!(task_find_by_id(next_id));
    if next_guard.status() == TaskStatus::Blocked {
        assert_eq_test!(
            task_set_state(next_id, TaskStatus::Ready),
            0,
            "could not make the incoming task Ready"
        );
    }
    next_guard.set_sched_placement(SchedPlacement::OnCpu);
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
    let mut prev: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
    let pat_prev = patterns_a();
    let pat_next = patterns_b();

    let (live_after_switch, prev_slot, published, accepted) =
        slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
            let saved = snapshot_live_fpu(xcr0);
            let prev_ref: &Task = &prev;
            let mut published = false;
            let mut accepted = true;

            slopos_ostd::task::run_switch(
                Some(prev_ref),
                &next_guard,
                || {
                    published = crate::scheduler::dispatch_task_for_test(cpu_id, next_id);

                    if let Some(current) = crate::task_struct::Current::get() {
                        cpu::xmm_load_4(&pat_next);
                        current.task().fpu_save_in_place(&current, xcr0);
                    }

                    // The owner tag is taken directly rather than as a
                    // side-effect of a restore: an `XRSTOR` here would load a
                    // slot the next line immediately overwrites, so only the tag
                    // would survive it anyway. Without the tag `prev` stays
                    // `FPU_CPU_NONE` and the `fpu_save_current` below takes
                    // `fpu_owner_assert_may_take`'s never-restored exemption,
                    // skipping the assertion this test exists for.
                    slopos_ostd::task::fpu_owner_take(prev_ref, fpu_current_cpu());
                    cpu::xmm_load_4(&pat_prev);
                },
                |prev_window, next_window| {
                    if let Some(prev_window) = prev_window {
                        prev_window.task().fpu_save_current(prev_window, xcr0);
                    }
                    accepted &= next_window.task().fpu_restore_to_cpu(next_window, xcr0);
                },
            );

            let live_after_switch = cpu::xmm_read_4();
            cpu::xmm_zero_4();
            accepted &= prev.fpu_restore_to_cpu_mut(xcr0);
            let prev_slot = cpu::xmm_read_4();

            slopos_ostd::task::fpu_owner_forget(&*prev);
            accepted &= saved.restore_to_cpu(xcr0);
            (live_after_switch, prev_slot, published, accepted)
        });

    drop(next_guard);
    task_terminate(next_id);

    assert_test!(published, "incoming task vanished before dispatch");
    assert_test!(
        accepted,
        "a task-authored FPU image was rejected by XRSTOR64"
    );

    for i in 0..4 {
        if live_after_switch[i] != pat_next[i] {
            klog_info!(
                "FPU_SWITCH: after the switch register {} is {:#x?}, expected the \
                 incoming task's {:#x?} — restore-next did not take effect",
                i,
                live_after_switch[i],
                pat_next[i],
            );
            return TestResult::Fail;
        }
        if prev_slot[i] != pat_prev[i] {
            klog_info!(
                "FPU_SWITCH: outgoing task's slot register {} is {:#x?}, expected \
                 the state that was live at switch-out {:#x?} — save-prev lost it",
                i,
                prev_slot[i],
                pat_prev[i],
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// The AVX upper halves survive a round trip through a task's slot. `fxsave`
/// cannot represent the upper 128 bits of YMM, so a save path that regressed to
/// it would return correct lower halves and zeroed upper ones.
pub fn test_fpu_avx_upper_halves_survive_task_slot() -> TestResult {
    use slopos_arch::cpu::control_regs::Xcr0Flags;
    use slopos_ostd::test_support::cpu_state as cpu;

    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
    if (xcr0 & Xcr0Flags::AVX.bits()) == 0 {
        return TestResult::Skipped;
    }

    let mut task: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");
    let patterns: [cpu::Xmm128; 4] = [
        [0xDEAD_BEEF_CAFE_BABE, 0x1111_2222_3333_4444],
        [0xAAAA_BBBB_CCCC_DDDD, 0x5555_6666_7777_8888],
        [0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210],
        [0xF0F0_E0E0_D0D0_C0C0, 0xA0A0_B0B0_9090_8080],
    ];

    let (readback, accepted) = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let saved = snapshot_live_fpu(xcr0);
        let mut accepted = true;

        accepted &= task.fpu_restore_to_cpu_mut(xcr0);
        cpu::ymm_load_2(&patterns);
        task.fpu_save_in_place_mut(xcr0);

        cpu::ymm_zero_2();
        accepted &= task.fpu_restore_to_cpu_mut(xcr0);
        let readback = cpu::ymm_read_2();

        slopos_ostd::task::fpu_owner_forget(&*task);
        accepted &= saved.restore_to_cpu(xcr0);
        (readback, accepted)
    });

    assert_test!(
        accepted,
        "a task-authored FPU image was rejected by XRSTOR64"
    );

    let labels = ["YMM0 lower", "YMM0 UPPER", "YMM1 lower", "YMM1 UPPER"];
    for i in 0..4 {
        if readback[i] != patterns[i] {
            klog_info!(
                "FPU_AVX: {} came back {:#x?}, expected {:#x?} — a task-slot save \
                 that drops the upper halves is an fxsave regression",
                labels[i],
                readback[i],
                patterns[i],
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// A save area the hardware refuses is repaired underneath the restore.
/// `prepare_switch_to` XRSTORs the incoming task's slot with interrupts off,
/// mid-switch, with nowhere to report a failure, so the repair has to happen
/// below it or the switch is what stops the CPU. The slot here is poisoned the
/// way a signal frame from user memory could poison one.
pub fn test_fpu_poisoned_slot_is_repaired() -> TestResult {
    use slopos_ostd::task::XSTATE_RESERVED_OFFSET;

    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
    let mut task: KBox<Task> = KBox::try_init(Task::init_invalid()).expect("alloc");

    // A non-zero byte in the XSTATE header's reserved tail faults in both the
    // standard and the compacted form, so the fault does not depend on which
    // XSAVE features this CPU implements.
    task.fpu_state.get_mut().data[XSTATE_RESERVED_OFFSET] = 1;

    let (accepted, poison_after, accepted_again, restored) =
        slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
            let saved = snapshot_live_fpu(xcr0);

            let accepted = task.fpu_restore_to_cpu_mut(xcr0);
            let poison_after = task.fpu_state.get_mut().data[XSTATE_RESERVED_OFFSET];
            let accepted_again = task.fpu_restore_to_cpu_mut(xcr0);

            slopos_ostd::task::fpu_owner_forget(&*task);
            let restored = saved.restore_to_cpu(xcr0);
            (accepted, poison_after, accepted_again, restored)
        });

    assert_test!(
        restored,
        "could not put the running task's own FPU state back"
    );
    assert_test!(
        !accepted,
        "xrstor64 accepted an image with a dirty reserved header byte"
    );
    assert_eq_test!(
        poison_after,
        0,
        "the rejected save area was left poisoned for the next context switch"
    );
    assert_test!(
        accepted_again,
        "the repaired save area was rejected a second time"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_task_flags_preserved, suite = context);
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
slopos_testing::stest!(name = test_fpu_per_task_slot_isolation, suite = context);
slopos_testing::stest!(
    name = test_fpu_switch_saves_prev_and_restores_next,
    suite = context
);
slopos_testing::stest!(
    name = test_fpu_avx_upper_halves_survive_task_slot,
    suite = context
);
slopos_testing::stest!(name = test_fpu_poisoned_slot_is_repaired, suite = context);
