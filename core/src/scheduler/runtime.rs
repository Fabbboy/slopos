use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_sync::{IrqMutex, OnceLock};
use slopos_utils::klog_info;

use super::per_cpu;
use super::scheduler::{run_ready_task_from_idle, schedule_task, set_scheduler_enabled, r#yield};
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_IDLE, Task, TaskEntry, reap_zombies,
    task_create, task_get_info, task_set_current,
};
use super::work_steal::try_work_steal;

const MAX_IDLE_CALLBACKS: usize = 4;
const MAX_BOTTOM_HALVES: usize = 4;

struct IdleCallbacks {
    slots: [Option<fn() -> c_int>; MAX_IDLE_CALLBACKS],
    count: usize,
}

struct BottomHalves {
    slots: [Option<fn()>; MAX_BOTTOM_HALVES],
    count: usize,
}

impl IdleCallbacks {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_IDLE_CALLBACKS],
            count: 0,
        }
    }
}

impl BottomHalves {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_BOTTOM_HALVES],
            count: 0,
        }
    }
}

static IDLE_CBS: OnceLock<IrqMutex<IdleCallbacks>> = OnceLock::new();
static BOTTOM_HALVES: OnceLock<IrqMutex<BottomHalves>> = OnceLock::new();

pub fn scheduler_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    IDLE_CBS.call_once(|| IrqMutex::new(IdleCallbacks::new()));
    let Some(cb) = callback else { return };
    if let Some(mutex) = IDLE_CBS.get() {
        let mut cbs = mutex.lock();
        let idx = cbs.count;
        if idx < MAX_IDLE_CALLBACKS {
            cbs.slots[idx] = Some(cb);
            cbs.count = idx + 1;
        }
    }
}

pub fn scheduler_register_bottom_half(callback: fn()) {
    BOTTOM_HALVES.call_once(|| IrqMutex::new(BottomHalves::new()));
    if let Some(mutex) = BOTTOM_HALVES.get() {
        let mut halves = mutex.lock();
        let idx = halves.count;
        if idx < MAX_BOTTOM_HALVES {
            halves.slots[idx] = Some(callback);
            halves.count = idx + 1;
        }
    }
}

pub fn scheduler_run_bottom_halves() {
    let mut callbacks = [None; MAX_BOTTOM_HALVES];
    let mut callback_count = 0usize;

    if let Some(mutex) = BOTTOM_HALVES.get() {
        let halves = mutex.lock();
        callback_count = halves.count;
        callbacks[..callback_count].copy_from_slice(&halves.slots[..callback_count]);
    }

    for callback in &callbacks[..callback_count] {
        if let Some(callback) = callback {
            callback();
        }
    }
}

pub fn spawn_kernel_task_from_driver(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void),
    arg: *mut c_void,
    priority: u8,
) -> u32 {
    let entry_fn: TaskEntry = unsafe { core::mem::transmute(entry as *const ()) };
    let task_id = task_create(name, entry_fn, arg, priority, TASK_FLAG_KERNEL_MODE);
    if task_id == INVALID_TASK_ID {
        return task_id;
    }
    let mut task_ptr: *mut Task = core::ptr::null_mut();
    if task_get_info(task_id, &mut task_ptr) != 0 || task_ptr.is_null() {
        return INVALID_TASK_ID;
    }
    if schedule_task(task_ptr) != 0 {
        return INVALID_TASK_ID;
    }
    task_id
}

fn unified_idle_loop(_: *mut c_void) {
    loop {
        let mut any_work = false;
        if let Some(mutex) = IDLE_CBS.get() {
            let cbs = mutex.lock();
            for slot in &cbs.slots[..cbs.count] {
                if let Some(cb) = slot {
                    if cb() != 0 {
                        any_work = true;
                    }
                }
            }
        }
        if any_work {
            r#yield();
            continue;
        }
        let cpu_id = slopos_arch::pcr::get_current_cpu();
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        if cpu_id == 0 {
            slopos_sync::rcu_process_callbacks();
        }
        slopos_sync::rcu_note_qs();
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

pub fn create_idle_task() -> c_int {
    create_idle_task_for_cpu(0)
}

pub fn create_idle_task_for_cpu(cpu_id: usize) -> c_int {
    let idle_task_id = unsafe {
        crate::task::task_create(
            b"idle\0".as_ptr() as *const i8,
            core::mem::transmute(unified_idle_loop as *const ()),
            ptr::null_mut(),
            TASK_PRIORITY_IDLE,
            TASK_FLAG_KERNEL_MODE,
        )
    };
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }

    unsafe {
        (*idle_task).cpu_affinity = per_cpu::affinity_mask_for_cpu(cpu_id);
        (*idle_task).last_cpu = cpu_id as u8;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleStackResolveError {
    MissingIdleTask,
    MissingKernelStack,
}

pub(crate) fn resolve_idle_stack_for_cpu(
    cpu_id: usize,
) -> Result<(*mut Task, u64), IdleStackResolveError> {
    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
    if idle_task.is_null() {
        return Err(IdleStackResolveError::MissingIdleTask);
    }

    let stack_top = unsafe { (*idle_task).kernel_stack_top };
    if stack_top == 0 {
        return Err(IdleStackResolveError::MissingKernelStack);
    }

    Ok((idle_task, stack_top))
}

#[inline(never)]
unsafe fn enter_scheduler_on_idle_stack(cpu_id: usize, idle_task: *mut Task, stack_top: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, rdx",
            "mov rbp, rsp",
            "call {target}",
            "ud2",
            target = sym scheduler_loop_entry,
            in("rdi") cpu_id,
            in("rsi") idle_task,
            in("rdx") stack_top,
            options(noreturn)
        );
    }
}

extern "C" fn scheduler_loop_entry(cpu_id: usize, idle_task: *mut Task) -> ! {
    scheduler_loop(cpu_id, idle_task)
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    let already_enabled =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.is_enabled()).unwrap_or(false);
    if already_enabled {
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    set_scheduler_enabled(true);

    slopos_arch::pcr::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let (idle_task, idle_stack_top) = match resolve_idle_stack_for_cpu(cpu_id) {
        Ok(values) => values,
        Err(IdleStackResolveError::MissingIdleTask) => {
            klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
            slopos_mm::tlb::notify_cpu_offline();
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
        Err(IdleStackResolveError::MissingKernelStack) => {
            klog_info!(
                "SCHED: CPU {} idle task has no kernel stack, halting",
                cpu_id
            );
            slopos_mm::tlb::notify_cpu_offline();
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
    };

    unsafe {
        let return_ctx = per_cpu::get_ap_return_context(cpu_id);
        if !return_ctx.is_null() {
            crate::ffi_boundary::init_kernel_context(return_ctx);
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
    });
    task_set_current(idle_task);

    unsafe { enter_scheduler_on_idle_stack(cpu_id, idle_task, idle_stack_top) }
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            slopos_sync::rcu_note_qs();
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        if run_ready_task_from_idle(cpu_id, idle_task) {
            continue;
        }

        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        reap_zombies();

        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        slopos_sync::rcu_note_qs();

        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}
