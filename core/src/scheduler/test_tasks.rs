use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_lib::klog_info;
use slopos_lib::string::cstr_to_str;

use super::scheduler;
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, IdtEntry, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE,
    TASK_PRIORITY_NORMAL, Task, TaskContext, TaskIterateCb, task_create, task_get_info,
    task_get_total_yields, task_iterate_active, task_shutdown_all, task_state_to_string,
};
use crate::platform;

use super::ffi_boundary::simple_context_switch;

use slopos_mm::kernel_heap::kmalloc;

use slopos_abi::arch::{GDT_USER_CODE_SELECTOR, GDT_USER_DATA_SELECTOR, SYSCALL_VECTOR};

use spin::Once;

static IDT_GATE_HOOK: Once<fn(u8, *mut c_void) -> c_int> = Once::new();

pub fn register_idt_gate_hook(hook: fn(u8, *mut c_void) -> c_int) {
    IDT_GATE_HOOK.call_once(|| hook);
}

fn idt_get_gate(vector: u8, entry: *mut c_void) -> c_int {
    if let Some(hook) = IDT_GATE_HOOK.get() {
        hook(vector, entry)
    } else {
        -1
    }
}

/* ========================================================================
 * TEST TASK IMPLEMENTATIONS
 * ======================================================================== */
pub fn test_task_a(arg: *mut c_void) {
    let _ = arg;
    let mut counter: u32 = 0;

    klog_info!("Task A starting execution");

    while counter < 20 {
        klog_info!("Task A: iteration {}", counter);
        counter = counter.wrapping_add(1);

        if counter % 3 == 0 {
            klog_info!("Task A: yielding CPU");
            scheduler::r#yield();
        }
    }

    klog_info!("Task A completed");
}
pub fn test_task_b(arg: *mut c_void) {
    let _ = arg;

    let mut current_char: u8 = b'A';
    let mut iterations: u32 = 0;

    klog_info!("Task B starting execution");

    while iterations < 15 {
        klog_info!(
            "Task B: printing character '{}' ({}) (",
            current_char as char,
            current_char as c_int
        );
        platform::console_putc(current_char);
        klog_info!(")");

        current_char = current_char.wrapping_add(1);
        if current_char > b'Z' {
            current_char = b'A';
        }

        iterations = iterations.wrapping_add(1);
        if iterations % 2 == 0 {
            klog_info!("Task B: yielding CPU");
            scheduler::r#yield();
        }
    }

    klog_info!("Task B completed");
}

/* ========================================================================
 * SCHEDULER TEST FUNCTIONS
 * ======================================================================== */
pub fn run_scheduler_test() -> c_int {
    klog_info!("=== Starting SlopOS Cooperative Scheduler Test ===");

    if crate::task::init_task_manager() != 0 {
        klog_info!("Failed to initialize task manager");
        return -1;
    }

    if scheduler::init_scheduler() != 0 {
        klog_info!("Failed to initialize scheduler");
        return -1;
    }

    if scheduler::create_idle_task() != 0 {
        klog_info!("Failed to create idle task");
        return -1;
    }

    klog_info!("Creating test tasks...");

    let task_a_id = task_create(
        b"TestTaskA\0".as_ptr() as *const c_char,
        test_task_a,
        ptr::null_mut(),
        1,
        TASK_FLAG_KERNEL_MODE,
    );

    if task_a_id == INVALID_TASK_ID {
        klog_info!("Failed to create test task A");
        return -1;
    }

    klog_info!("Created Task A with ID {}", task_a_id);

    let task_b_id = task_create(
        b"TestTaskB\0".as_ptr() as *const c_char,
        test_task_b,
        ptr::null_mut(),
        1,
        TASK_FLAG_KERNEL_MODE,
    );

    if task_b_id == INVALID_TASK_ID {
        klog_info!("Failed to create test task B");
        return -1;
    }

    klog_info!("Created Task B with ID {}", task_b_id);

    let mut task_a_info: *mut Task = ptr::null_mut();
    let mut task_b_info: *mut Task = ptr::null_mut();

    if task_get_info(task_a_id, &mut task_a_info) != 0 {
        klog_info!("Failed to get task A info");
        return -1;
    }

    if task_get_info(task_b_id, &mut task_b_info) != 0 {
        klog_info!("Failed to get task B info");
        return -1;
    }

    if scheduler::schedule_task(task_a_info) != 0 {
        klog_info!("Failed to schedule task A");
        crate::task::task_terminate(task_a_id);
        crate::task::task_terminate(task_b_id);
        return -1;
    }

    if scheduler::schedule_task(task_b_info) != 0 {
        klog_info!("Failed to schedule task B");
        crate::task::task_terminate(task_a_id);
        crate::task::task_terminate(task_b_id);
        return -1;
    }

    klog_info!("Tasks scheduled, starting scheduler...");

    if scheduler::start_scheduler() != 0 {
        klog_info!("Failed to start scheduler");
        return -1;
    }

    klog_info!("Scheduler started successfully");

    0
}

/* ========================================================================
 * PRIVILEGE SEPARATION TEST
 * ======================================================================== */

#[unsafe(link_section = ".user_text")]
fn user_stub_task(arg: *mut c_void) {
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
pub fn run_privilege_separation_invariant_test() -> c_int {
    klog_info!("PRIVSEP_TEST: Checking privilege separation invariants");

    if crate::task::init_task_manager() != 0
        || scheduler::init_scheduler() != 0
        || scheduler::create_idle_task() != 0
    {
        klog_info!("PRIVSEP_TEST: init failed");
        return -1;
    }

    let user_task_id = task_create(
        b"UserStub\0".as_ptr() as *const c_char,
        user_stub_task,
        ptr::null_mut(),
        TASK_PRIORITY_NORMAL,
        TASK_FLAG_USER_MODE,
    );
    if user_task_id == INVALID_TASK_ID {
        klog_info!("PRIVSEP_TEST: user task creation failed");
        return -1;
    }

    let mut task_info: *mut Task = ptr::null_mut();
    if task_get_info(user_task_id, &mut task_info) != 0 || task_info.is_null() {
        klog_info!("PRIVSEP_TEST: task lookup failed");
        return -1;
    }

    let mut failed = 0;

    unsafe {
        if (*task_info).process_id == INVALID_PROCESS_ID {
            klog_info!("PRIVSEP_TEST: user task missing process VM");
            failed = 1;
        }
        if (*task_info).kernel_stack_top == 0 {
            klog_info!("PRIVSEP_TEST: user task missing kernel RSP0 stack");
            failed = 1;
        }
        let cs = (*task_info).context.cs;
        let ss = (*task_info).context.ss;
        if cs != GDT_USER_CODE_SELECTOR as u64 || ss != GDT_USER_DATA_SELECTOR as u64 {
            klog_info!(
                "PRIVSEP_TEST: user task selectors incorrect (cs=0x{:x} ss=0x{:x})",
                cs,
                ss
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

    let gate_ptr = &mut gate as *mut IdtEntry as *mut c_void;
    if idt_get_gate(SYSCALL_VECTOR, gate_ptr) != 0 {
        klog_info!("PRIVSEP_TEST: cannot read syscall gate");
        failed = 1;
    } else {
        let dpl = (gate.type_attr >> 5) & 0x3;
        if dpl != 3 {
            klog_info!("PRIVSEP_TEST: syscall gate DPL={} expected 3", dpl as u32);
            failed = 1;
        }
    }

    task_shutdown_all();
    scheduler::scheduler_shutdown();

    if failed != 0 {
        klog_info!("PRIVSEP_TEST: FAILED");
        return -1;
    }

    klog_info!("PRIVSEP_TEST: PASSED");
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
pub fn smoke_test_task_impl(ctx: *mut SmokeTestContext) {
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

    let name_str = unsafe { cstr_to_str(name) };
    klog_info!("{}: Starting (initial RSP=0x{:x})", name_str, stack_base);

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

        let stack_growth = ctx_ref
            .initial_stack_top
            .saturating_sub(ctx_ref.min_stack_pointer);
        if stack_growth > 0x1000 {
            klog_info!(
                "{}: ERROR - Stack growth exceeds 4KB: 0x{:x} bytes",
                name_str,
                stack_growth
            );
            ctx_ref.test_failed = 1;
            break;
        }

        iteration = iteration.wrapping_add(1);
        if iteration % 50 == 0 {
            klog_info!(
                "{}: Iteration {} (yields: {}, RSP=0x{:x})",
                name_str,
                iteration,
                ctx_ref.yield_count,
                current_rsp
            );
        }

        scheduler::r#yield();
        ctx_ref.yield_count = ctx_ref.yield_count.wrapping_add(1);
    }

    klog_info!("{}: Completed {} yields", name_str, ctx_ref.yield_count);
    klog_info!(
        "{}: Stack range: min=0x{:x} max=0x{:x} growth=0x{:x} bytes",
        name_str,
        ctx_ref.min_stack_pointer,
        ctx_ref.max_stack_pointer,
        ctx_ref
            .initial_stack_top
            .saturating_sub(ctx_ref.min_stack_pointer)
    );
    if ctx_ref.test_failed != 0 {
        klog_info!("{}: FAILED - Stack corruption detected", name_str);
    } else {
        klog_info!("{}: PASSED - No stack corruption", name_str);
    }
}
pub fn smoke_test_task_a(arg: *mut c_void) {
    let ctx = arg as *mut SmokeTestContext;
    if ctx.is_null() {
        return;
    }
    unsafe { (*ctx).task_name = b"SmokeTestA\0".as_ptr() as *const c_char };
    smoke_test_task_impl(ctx);
}
pub fn smoke_test_task_b(arg: *mut c_void) {
    let ctx = arg as *mut SmokeTestContext;
    if ctx.is_null() {
        return;
    }
    unsafe { (*ctx).task_name = b"SmokeTestB\0".as_ptr() as *const c_char };
    smoke_test_task_impl(ctx);
}
pub fn run_context_switch_smoke_test() -> c_int {
    klog_info!("=== Context Switch Stack Discipline Smoke Test ===");
    klog_info!("Testing basic context switch functionality");

    static mut TEST_COMPLETED: c_int = 0;
    unsafe {
        TEST_COMPLETED = 0;
        TEST_COMPLETED_PTR = &raw mut TEST_COMPLETED;
    }

    let mut test_ctx = TaskContext::default();
    test_ctx.rax = 0;
    test_ctx.rbx = 0;
    test_ctx.rcx = 0;
    test_ctx.rdx = 0;
    test_ctx.rsi = 0;
    test_ctx.rdi = unsafe { TEST_COMPLETED_PTR as u64 };
    test_ctx.rbp = 0;
    test_ctx.rip = test_task_function as *const () as usize as u64;
    test_ctx.rflags = 0x202;
    test_ctx.cs = 0x08;
    test_ctx.ds = 0x10;
    test_ctx.es = 0x10;
    test_ctx.fs = 0;
    test_ctx.gs = 0;
    test_ctx.ss = 0x10;
    test_ctx.cr3 = 0;

    let stack = kmalloc(4096) as *mut u64;
    if stack.is_null() {
        klog_info!("Failed to allocate stack for test task");
        return -1;
    }
    test_ctx.rsp = unsafe { stack.add(1024) } as u64;

    klog_info!("Switching to test context...");

    let mut current_rsp: u64 = 0;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) current_rsp);
        KERNEL_RETURN_CONTEXT.rip = context_switch_return_trampoline as *const () as usize as u64;
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
        simple_context_switch(&mut dummy_old, &test_ctx);
    }

    unsafe { core::hint::unreachable_unchecked() }
}
pub fn test_task_function(completed_flag: *mut c_int) {
    klog_info!("Test task function executed successfully");
    if !completed_flag.is_null() {
        unsafe {
            *completed_flag = 1;
        }
    }

    let mut dummy = TaskContext::default();
    unsafe {
        simple_context_switch(&mut dummy, &raw const KERNEL_RETURN_CONTEXT);
    }
}
pub fn context_switch_return_trampoline() -> c_int {
    klog_info!("Context switch returned successfully");

    let completed = unsafe {
        if TEST_COMPLETED_PTR.is_null() {
            0
        } else {
            *TEST_COMPLETED_PTR
        }
    };

    if completed != 0 {
        klog_info!("CONTEXT_SWITCH_TEST: Basic switch test PASSED");
        0
    } else {
        klog_info!("CONTEXT_SWITCH_TEST: Basic switch test FAILED");
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

fn print_task_stat_line(task: *mut Task, context: *mut c_void) {
    let ctx = unsafe { &mut *(context as *mut TaskStatPrintCtx) };
    ctx.index = ctx.index.wrapping_add(1);

    let name_str = unsafe { cstr_to_str((*task).name.as_ptr() as *const c_char) };
    let state_str = unsafe { cstr_to_str(task_state_to_string((*task).state)) };
    unsafe {
        klog_info!(
            "  #{} '{}' (ID {}) [{}] runtime={} ticks yields={}",
            ctx.index,
            name_str,
            (*task).task_id,
            state_str,
            (*task).total_runtime as u64,
            (*task).yield_count as u64
        );
    }
}
pub fn print_scheduler_stats() {
    use super::scheduler::get_scheduler_stats;
    use super::task::get_task_stats;

    let mut sched_switches: u64 = 0;
    let mut sched_yields: u64 = 0;
    let mut ready_tasks: u32 = 0;
    let mut schedule_calls: u32 = 0;
    let mut total_tasks: u32 = 0;
    let mut active_tasks: u32 = 0;
    let mut task_switches: u64 = 0;
    let task_yields = task_get_total_yields();

    get_scheduler_stats(
        &mut sched_switches,
        &mut sched_yields,
        &mut ready_tasks,
        &mut schedule_calls,
    );
    get_task_stats(&mut total_tasks, &mut active_tasks, &mut task_switches);

    klog_info!("\n=== Scheduler Statistics ===");
    klog_info!("Context switches: {}", sched_switches);
    klog_info!("Voluntary yields: {}", sched_yields);
    klog_info!("Schedule calls: {}", schedule_calls);
    klog_info!("Ready tasks: {}", ready_tasks);
    klog_info!("Total tasks created: {}", total_tasks);
    klog_info!("Active tasks: {}", active_tasks);
    klog_info!("Task yields (aggregate): {}", task_yields);
    klog_info!("Active task metrics:");

    let mut ctx = TaskStatPrintCtx { index: 0 };
    let callback: TaskIterateCb = Some(print_task_stat_line);
    task_iterate_active(callback, &mut ctx as *mut _ as *mut c_void);
    if ctx.index == 0 {
        klog_info!("  (no active tasks)");
    }
}
