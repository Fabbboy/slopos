#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_lib::klog::{klog_printf, KlogLevel};

use crate::scheduler;
use crate::task::{
    task_create, task_get_info, task_get_total_yields, task_iterate_active, task_shutdown_all, task_state_to_string,
    Task, TaskContext, TaskIterateCb, INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE,
    TASK_PRIORITY_NORMAL,
};

unsafe extern "C" {
    fn serial_putc_com1(ch: u8);
    fn kmalloc(size: usize) -> *mut c_void;
    fn simple_context_switch(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn idt_get_gate(vector: u8, out_entry: *mut IdtEntry) -> c_int;
}

#[repr(C, packed)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

const SYSCALL_VECTOR: u8 = 0x80;

const GDT_USER_CODE_SELECTOR: u64 = 0x23;
const GDT_USER_DATA_SELECTOR: u64 = 0x1B;

/* ========================================================================
 * TEST TASK IMPLEMENTATIONS
 * ======================================================================== */

#[unsafe(no_mangle)]
pub extern "C" fn test_task_a(arg: *mut c_void) {
    let _ = arg;
    let mut counter: u32 = 0;

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Task A starting execution\n\0".as_ptr() as *const c_char,
        );
    }

    while counter < 20 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Task A: iteration %u\n\0".as_ptr() as *const c_char,
                counter,
            );
        }
        counter = counter.wrapping_add(1);

        if counter % 3 == 0 {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"Task A: yielding CPU\n\0".as_ptr() as *const c_char,
                );
            }
            scheduler::r#yield();
        }
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Task A completed\n\0".as_ptr() as *const c_char,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_task_b(arg: *mut c_void) {
    let _ = arg;

    let mut current_char: u8 = b'A';
    let mut iterations: u32 = 0;

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Task B starting execution\n\0".as_ptr() as *const c_char,
        );
    }

    while iterations < 15 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Task B: printing character '%c' (%u) (\0".as_ptr() as *const c_char,
                current_char as c_int,
                current_char as c_int,
            );
            serial_putc_com1(current_char);
            klog_printf(KlogLevel::Info, b")\n\0".as_ptr() as *const c_char);
        }

        current_char = current_char.wrapping_add(1);
        if current_char > b'Z' {
            current_char = b'A';
        }

        iterations = iterations.wrapping_add(1);
        if iterations % 2 == 0 {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"Task B: yielding CPU\n\0".as_ptr() as *const c_char,
                );
            }
            scheduler::r#yield();
        }
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Task B completed\n\0".as_ptr() as *const c_char,
        );
    }
}

/* ========================================================================
 * SCHEDULER TEST FUNCTIONS
 * ======================================================================== */

#[unsafe(no_mangle)]
pub extern "C" fn run_scheduler_test() -> c_int {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== Starting SlopOS Cooperative Scheduler Test ===\n\0".as_ptr() as *const c_char,
        );
    }

    if unsafe { crate::task::init_task_manager() } != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to initialize task manager\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    if scheduler::init_scheduler() != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to initialize scheduler\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    if scheduler::create_idle_task() != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to create idle task\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Creating test tasks...\n\0".as_ptr() as *const c_char,
        );
    }

    let task_a_id = unsafe {
        task_create(
            b"TestTaskA\0".as_ptr() as *const c_char,
            test_task_a,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        )
    };

    if task_a_id == INVALID_TASK_ID {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to create test task A\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Created Task A with ID %u\n\0".as_ptr() as *const c_char,
            task_a_id,
        );
    }

    let task_b_id = unsafe {
        task_create(
            b"TestTaskB\0".as_ptr() as *const c_char,
            test_task_b,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        )
    };

    if task_b_id == INVALID_TASK_ID {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to create test task B\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Created Task B with ID %u\n\0".as_ptr() as *const c_char,
            task_b_id,
        );
    }

    let mut task_a_info: *mut Task = ptr::null_mut();
    let mut task_b_info: *mut Task = ptr::null_mut();

    if task_get_info(task_a_id, &mut task_a_info) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to get task A info\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    if task_get_info(task_b_id, &mut task_b_info) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to get task B info\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    if scheduler::schedule_task(task_a_info) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to schedule task A\n\0".as_ptr() as *const c_char,
            );
        }
        unsafe {
            crate::task::task_terminate(task_a_id);
            crate::task::task_terminate(task_b_id);
        }
        return -1;
    }

    if scheduler::schedule_task(task_b_info) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to schedule task B\n\0".as_ptr() as *const c_char,
            );
        }
        unsafe {
            crate::task::task_terminate(task_a_id);
            crate::task::task_terminate(task_b_id);
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Tasks scheduled, starting scheduler...\n\0".as_ptr() as *const c_char,
        );
    }

    if scheduler::start_scheduler() != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to start scheduler\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Scheduler started successfully\n\0".as_ptr() as *const c_char,
        );
    }

    0
}

/* ========================================================================
 * PRIVILEGE SEPARATION TEST
 * ======================================================================== */

#[unsafe(link_section = ".user_text")]
extern "C" fn user_stub_task(arg: *mut c_void) {
    let _ = arg;
    unsafe {
        core::arch::asm!(
            "mov rax, 0",
            "int 0x80",
            "mov rax, 1",
            "int 0x80",
            options(noreturn)
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run_privilege_separation_invariant_test() -> c_int {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"PRIVSEP_TEST: Checking privilege separation invariants\n\0".as_ptr() as *const c_char,
        );
    }

    if unsafe { crate::task::init_task_manager() } != 0
        || scheduler::init_scheduler() != 0
        || scheduler::create_idle_task() != 0
    {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: init failed\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let user_task_id = unsafe {
        task_create(
            b"UserStub\0".as_ptr() as *const c_char,
            user_stub_task,
            ptr::null_mut(),
            TASK_PRIORITY_NORMAL,
            TASK_FLAG_USER_MODE,
        )
    };
    if user_task_id == INVALID_TASK_ID {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: user task creation failed\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(user_task_id, &mut task_info) != 0 || task_info.is_null() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: task lookup failed\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    let mut failed = 0;

    unsafe {
        if (*task_info).process_id == INVALID_PROCESS_ID {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: user task missing process VM\n\0".as_ptr() as *const c_char,
            );
            failed = 1;
        }
        if (*task_info).kernel_stack_top == 0 {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: user task missing kernel RSP0 stack\n\0".as_ptr() as *const c_char,
            );
            failed = 1;
        }
        if (*task_info).context.cs != GDT_USER_CODE_SELECTOR || (*task_info).context.ss != GDT_USER_DATA_SELECTOR {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: user task selectors incorrect (cs=0x%lx ss=0x%lx)\n\0".as_ptr()
                    as *const c_char,
                (*task_info).context.cs,
                (*task_info).context.ss,
            );
            failed = 1;
        }
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

    if unsafe { idt_get_gate(SYSCALL_VECTOR, &mut gate) } != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: cannot read syscall gate\n\0".as_ptr() as *const c_char,
            );
        }
        failed = 1;
    } else {
        let dpl = (gate.type_attr >> 5) & 0x3;
        if dpl != 3 {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"PRIVSEP_TEST: syscall gate DPL=%u expected 3\n\0".as_ptr() as *const c_char,
                    dpl as u32,
                );
            }
            failed = 1;
        }
    }

    unsafe {
        task_shutdown_all();
        scheduler::scheduler_shutdown();
    }

    if failed != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"PRIVSEP_TEST: FAILED\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"PRIVSEP_TEST: PASSED\n\0".as_ptr() as *const c_char,
        );
    }
    0
}

/* ========================================================================
 * CONTEXT SWITCH SMOKE TEST
 * ======================================================================== */

#[repr(C)]
pub struct SmokeTestContext {
    pub initial_stack_top: u64,
    pub min_stack_pointer: u64,
    pub max_stack_pointer: u64,
    pub yield_count: u32,
    pub test_failed: c_int,
    pub task_name: *const c_char,
}

static mut KERNEL_RETURN_CONTEXT: TaskContext = const { TaskContext::zero() };
static mut TEST_COMPLETED_PTR: *mut c_int = ptr::null_mut();

#[unsafe(no_mangle)]
pub extern "C" fn smoke_test_task_impl(ctx: *mut SmokeTestContext) {
    if ctx.is_null() {
        return;
    }
    let ctx_ref = unsafe { &mut *ctx };
    let mut stack_base: u64 = 0;

    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) stack_base);
    }
    ctx_ref.initial_stack_top = stack_base;
    ctx_ref.min_stack_pointer = stack_base;
    ctx_ref.max_stack_pointer = stack_base;
    ctx_ref.yield_count = 0;
    ctx_ref.test_failed = 0;

    let name = if ctx_ref.task_name.is_null() {
        b"SmokeTest\0".as_ptr() as *const c_char
    } else {
        ctx_ref.task_name
    };

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"%s: Starting (initial RSP=0x%lx)\n\0".as_ptr() as *const c_char,
            name,
            stack_base,
        );
    }

    let mut iteration: u32 = 0;
    let target_yields: u32 = 100;

    while ctx_ref.yield_count < target_yields {
        let mut current_rsp: u64 = 0;
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) current_rsp);
        }

        if current_rsp < ctx_ref.min_stack_pointer {
            ctx_ref.min_stack_pointer = current_rsp;
        }
        if current_rsp > ctx_ref.max_stack_pointer {
            ctx_ref.max_stack_pointer = current_rsp;
        }

        let stack_growth = ctx_ref.initial_stack_top.saturating_sub(ctx_ref.min_stack_pointer);
        if stack_growth > 0x1000 {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"%s: ERROR - Stack growth exceeds 4KB: 0x%lx bytes\n\0".as_ptr() as *const c_char,
                    name,
                    stack_growth,
                );
            }
            ctx_ref.test_failed = 1;
            break;
        }

        iteration = iteration.wrapping_add(1);
        if iteration % 50 == 0 {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"%s: Iteration %u (yields: %u, RSP=0x%lx)\n\0".as_ptr() as *const c_char,
                    name,
                    iteration,
                    ctx_ref.yield_count,
                    current_rsp,
                );
            }
        }

        scheduler::r#yield();
        ctx_ref.yield_count = ctx_ref.yield_count.wrapping_add(1);
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"%s: Completed %u yields\n\0".as_ptr() as *const c_char,
            name,
            ctx_ref.yield_count,
        );
        klog_printf(
            KlogLevel::Info,
            b"%s: Stack range: min=0x%lx max=0x%lx growth=0x%lx bytes\n\0".as_ptr() as *const c_char,
            name,
            ctx_ref.min_stack_pointer,
            ctx_ref.max_stack_pointer,
            ctx_ref.initial_stack_top.saturating_sub(ctx_ref.min_stack_pointer),
        );
        if ctx_ref.test_failed != 0 {
            klog_printf(
                KlogLevel::Info,
                b"%s: FAILED - Stack corruption detected\n\0".as_ptr() as *const c_char,
                name,
            );
        } else {
            klog_printf(
                KlogLevel::Info,
                b"%s: PASSED - No stack corruption\n\0".as_ptr() as *const c_char,
                name,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn smoke_test_task_a(arg: *mut c_void) {
    let ctx = arg as *mut SmokeTestContext;
    if ctx.is_null() {
        return;
    }
    unsafe { (*ctx).task_name = b"SmokeTestA\0".as_ptr() as *const c_char };
    smoke_test_task_impl(ctx);
}

#[unsafe(no_mangle)]
pub extern "C" fn smoke_test_task_b(arg: *mut c_void) {
    let ctx = arg as *mut SmokeTestContext;
    if ctx.is_null() {
        return;
    }
    unsafe { (*ctx).task_name = b"SmokeTestB\0".as_ptr() as *const c_char };
    smoke_test_task_impl(ctx);
}

#[unsafe(no_mangle)]
pub extern "C" fn run_context_switch_smoke_test() -> c_int {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== Context Switch Stack Discipline Smoke Test ===\n\0".as_ptr() as *const c_char,
        );
        klog_printf(
            KlogLevel::Info,
            b"Testing basic context switch functionality\n\0".as_ptr() as *const c_char,
        );
    }

    static mut TEST_COMPLETED: c_int = 0;
    unsafe {
        TEST_COMPLETED = 0;
        TEST_COMPLETED_PTR = &mut TEST_COMPLETED;
    }

    let mut test_ctx = TaskContext::default();
    test_ctx.rax = 0;
    test_ctx.rbx = 0;
    test_ctx.rcx = 0;
    test_ctx.rdx = 0;
    test_ctx.rsi = 0;
    test_ctx.rdi = unsafe { TEST_COMPLETED_PTR as u64 };
    test_ctx.rbp = 0;
    test_ctx.rip = test_task_function as u64;
    test_ctx.rflags = 0x202;
    test_ctx.cs = 0x08;
    test_ctx.ds = 0x10;
    test_ctx.es = 0x10;
    test_ctx.fs = 0;
    test_ctx.gs = 0;
    test_ctx.ss = 0x10;
    test_ctx.cr3 = 0;

    let stack = unsafe { kmalloc(4096) as *mut u64 };
    if stack.is_null() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"Failed to allocate stack for test task\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }
    test_ctx.rsp = unsafe { stack.add(1024) } as u64;

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Switching to test context...\n\0".as_ptr() as *const c_char,
        );
    }

    let mut current_rsp: u64 = 0;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) current_rsp);
        KERNEL_RETURN_CONTEXT.rip = context_switch_return_trampoline as u64;
        KERNEL_RETURN_CONTEXT.rsp = current_rsp;
        KERNEL_RETURN_CONTEXT.cs = 0x08;
        KERNEL_RETURN_CONTEXT.ss = 0x10;
        KERNEL_RETURN_CONTEXT.ds = 0x10;
        KERNEL_RETURN_CONTEXT.es = 0x10;
        KERNEL_RETURN_CONTEXT.fs = 0;
        KERNEL_RETURN_CONTEXT.gs = 0;
        KERNEL_RETURN_CONTEXT.rflags = 0x202;
    }

    let mut dummy_old = TaskContext::default();
    unsafe {
        unsafe {
            simple_context_switch(&mut dummy_old, &test_ctx);
        }
    }

    unsafe { core::hint::unreachable_unchecked() }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_task_function(completed_flag: *mut c_int) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Test task function executed successfully\n\0".as_ptr() as *const c_char,
        );
    }
    if !completed_flag.is_null() {
        unsafe {
            *completed_flag = 1;
        }
    }

    let mut dummy = TaskContext::default();
    unsafe {
        unsafe {
            simple_context_switch(&mut dummy, &KERNEL_RETURN_CONTEXT as *const TaskContext);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn context_switch_return_trampoline() -> c_int {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Context switch returned successfully\n\0".as_ptr() as *const c_char,
        );
    }

    let completed = unsafe {
        if TEST_COMPLETED_PTR.is_null() {
            0
        } else {
            *TEST_COMPLETED_PTR
        }
    };

    if completed != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"CONTEXT_SWITCH_TEST: Basic switch test PASSED\n\0".as_ptr() as *const c_char,
            );
        }
        0
    } else {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"CONTEXT_SWITCH_TEST: Basic switch test FAILED\n\0".as_ptr() as *const c_char,
            );
        }
        -1
    }
}

/* ========================================================================
 * SCHEDULER STATISTICS AND MONITORING
 * ======================================================================== */

#[repr(C)]
struct TaskStatPrintCtx {
    index: u32,
}

extern "C" fn print_task_stat_line(task: *mut Task, context: *mut c_void) {
    let ctx = unsafe { &mut *(context as *mut TaskStatPrintCtx) };
    ctx.index = ctx.index.wrapping_add(1);

    let name = unsafe { (*task).name.as_ptr() };
    let state_str = task_state_to_string(unsafe { (*task).state });
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"  #%u '%s' (ID %u) [%s] runtime=%llu ticks yields=%llu\n\0".as_ptr() as *const c_char,
            ctx.index,
            name,
            (*task).task_id,
            state_str,
            (*task).total_runtime as u64,
            (*task).yield_count as u64,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn print_scheduler_stats() {
    unsafe extern "C" {
        fn get_scheduler_stats(
            context_switches: *mut u64,
            yields: *mut u64,
            ready_tasks: *mut u32,
            schedule_calls: *mut u32,
        );
        fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64);
    }

    let mut sched_switches: u64 = 0;
    let mut sched_yields: u64 = 0;
    let mut ready_tasks: u32 = 0;
    let mut schedule_calls: u32 = 0;
    let mut total_tasks: u32 = 0;
    let mut active_tasks: u32 = 0;
    let mut task_switches: u64 = 0;
    let task_yields = task_get_total_yields();

    unsafe {
        get_scheduler_stats(
            &mut sched_switches,
            &mut sched_yields,
            &mut ready_tasks,
            &mut schedule_calls,
        );
        get_task_stats(&mut total_tasks, &mut active_tasks, &mut task_switches);
    }

    unsafe {
        klog_printf(KlogLevel::Info, b"\n=== Scheduler Statistics ===\n\0".as_ptr() as *const c_char);
        klog_printf(
            KlogLevel::Info,
            b"Context switches: %llu\n\0".as_ptr() as *const c_char,
            sched_switches,
        );
        klog_printf(
            KlogLevel::Info,
            b"Voluntary yields: %llu\n\0".as_ptr() as *const c_char,
            sched_yields,
        );
        klog_printf(
            KlogLevel::Info,
            b"Schedule calls: %u\n\0".as_ptr() as *const c_char,
            schedule_calls,
        );
        klog_printf(
            KlogLevel::Info,
            b"Ready tasks: %u\n\0".as_ptr() as *const c_char,
            ready_tasks,
        );
        klog_printf(
            KlogLevel::Info,
            b"Total tasks created: %u\n\0".as_ptr() as *const c_char,
            total_tasks,
        );
        klog_printf(
            KlogLevel::Info,
            b"Active tasks: %u\n\0".as_ptr() as *const c_char,
            active_tasks,
        );
        klog_printf(
            KlogLevel::Info,
            b"Task yields (aggregate): %llu\n\0".as_ptr() as *const c_char,
            task_yields,
        );

        klog_printf(
            KlogLevel::Info,
            b"Active task metrics:\n\0".as_ptr() as *const c_char,
        );
    }

    let mut ctx = TaskStatPrintCtx { index: 0 };
    let callback: TaskIterateCb = Some(print_task_stat_line);
    task_iterate_active(callback, &mut ctx as *mut _ as *mut c_void);
    if ctx.index == 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"  (no active tasks)\n\0".as_ptr() as *const c_char,
            );
        }
    }
}

