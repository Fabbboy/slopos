use core::ffi::{c_char, c_int, c_void};
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use slopos_arch::cpu;
use slopos_ostd::kdiag_timestamp;
use slopos_ostd::string::bytes_as_str;
use slopos_ostd::task::accessors::{
    TASK_EXIT_CLEANUP_ACCOUNTED, TASK_EXIT_CLEANUP_RESOURCES, TASK_EXIT_CLEANUP_VM,
    task_exit_cleanup_mark,
};
use slopos_ostd::{klog_debug, klog_info};

use slopos_ostd::task::accessors::task_process_group;
use slopos_ostd::task::new_session_group;
use slopos_ostd::task::switch::task_entry_trampoline;

use super::task_cleanup_hooks::run_task_resource_cleanup_hooks;
use super::task_session::{notify_parent_of_child_exit, release_task_dependents};
use super::task_stats::record_task_created;
use super::task_table::{
    PendingTask, TaskAllocError, TaskRef, allocate_task, register_task, task_find_by_id, task_reap,
    with_task_manager,
};
use super::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE,
    TASK_UNSAFE_STACK_SIZE, Task, TaskContext, TaskEntry, TaskExitReason, TaskFaultReason,
    TaskPriority, TaskStatus, task_id_of, task_on_cpu_load, task_panic_in_flight_store,
    task_recovery_depth_store, task_status,
};
use crate::exit_info::ExitInfo;
use crate::scheduler;
use crate::task_stack::{KernelStack, UnsafeStack};
use crate::task_struct::SwitchContext;
use slopos_fs::fileio::{
    fileio_clone_table_for_process, fileio_create_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_clone_cow, process_vm_get_page_dir,
    process_vm_get_stack_top,
};
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::task::TaskAddr;

slopos_ostd::extern_block! {
    mod task_externs {
        fn user_task_first_run();
    }
}

fn user_entry_is_allowed(addr: u64) -> bool {
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000;
    addr >= PROCESS_CODE_START_VA && addr < PROCESS_CODE_END
}

struct TaskCreateResources {
    process_id: u32,
    /// User-mode stack base (for user tasks, this lives in process VM;
    /// for kernel tasks, this aliases the kernel stack base).
    stack_base: u64,
    /// Owning handle to the kernel-mode stack.  Moves into `Task` on
    /// success; dropped on failure to auto-release all backing memory.
    kernel_stack: KernelStack,
    /// Owning handle to the SafeStack-sanitizer unsafe (data) stack.
    /// Allocated alongside `kernel_stack` so every live task owns both.
    unsafe_stack: UnsafeStack,
}

struct ProcessResourceLease {
    process_id: u32,
    owns_vm: bool,
    owns_file_table: bool,
}

impl ProcessResourceLease {
    const fn none() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            owns_vm: false,
            owns_file_table: false,
        }
    }

    #[inline]
    const fn process_id(&self) -> u32 {
        self.process_id
    }

    fn create_user_process() -> Option<Self> {
        let process_id = create_process_vm();
        if process_id == INVALID_PROCESS_ID {
            klog_info!("task_create: Failed to create process VM");
            return None;
        }

        if fileio_create_table_for_process(process_id) != 0 {
            destroy_process_vm(process_id);
            return None;
        }

        Some(Self {
            process_id,
            owns_vm: true,
            owns_file_table: true,
        })
    }

    fn clone_from_parent(parent_process_id: u32) -> Option<Self> {
        let child_process_id = process_vm_clone_cow(parent_process_id);
        if child_process_id == INVALID_PROCESS_ID {
            return None;
        }

        if fileio_clone_table_for_process(parent_process_id, child_process_id) != 0 {
            destroy_process_vm(child_process_id);
            return None;
        }

        Some(Self {
            process_id: child_process_id,
            owns_vm: true,
            owns_file_table: true,
        })
    }

    fn disarm(mut self) -> u32 {
        let process_id = self.process_id;
        self.process_id = INVALID_PROCESS_ID;
        self.owns_vm = false;
        self.owns_file_table = false;
        process_id
    }

    fn cleanup_owned_process(process_id: u32, owns_vm: bool, owns_file_table: bool) {
        if process_id == INVALID_PROCESS_ID {
            return;
        }

        if owns_file_table {
            fileio_destroy_table_for_process(process_id);
        }
        if owns_vm {
            destroy_process_vm(process_id);
        }
    }
}

impl Drop for ProcessResourceLease {
    fn drop(&mut self) {
        Self::cleanup_owned_process(self.process_id, self.owns_vm, self.owns_file_table);
    }
}

fn allocate_kernel_stack(size: u64, what: &'static str) -> Option<KernelStack> {
    match KernelStack::allocate(size as usize) {
        Ok(s) => Some(s),
        Err(e) => {
            klog_info!("task_create: {} failed: {:?}", what, e);
            None
        }
    }
}

fn allocate_unsafe_stack(size: u64, what: &'static str) -> Option<UnsafeStack> {
    match UnsafeStack::allocate(size as usize) {
        Ok(s) => Some(s),
        Err(e) => {
            klog_info!("task_create: {} failed: {:?}", what, e);
            None
        }
    }
}

fn allocate_kernel_task_resources() -> Option<TaskCreateResources> {
    let kernel_stack = allocate_kernel_stack(TASK_STACK_SIZE, "kernel stack")?;
    let unsafe_stack = allocate_unsafe_stack(TASK_UNSAFE_STACK_SIZE, "SafeStack data stack")?;
    let stack_base = kernel_stack.base().as_u64();
    Some(TaskCreateResources {
        process_id: INVALID_PROCESS_ID,
        stack_base,
        kernel_stack,
        unsafe_stack,
    })
}

fn allocate_user_task_resources() -> Option<TaskCreateResources> {
    let process = ProcessResourceLease::create_user_process()?;
    let process_id = process.process_id();

    let stack_top = process_vm_get_stack_top(process_id);
    if stack_top == 0 {
        klog_info!("task_create: Failed to get process stack");
        return None;
    }

    let kernel_stack = allocate_kernel_stack(TASK_KERNEL_STACK_SIZE, "kernel RSP0 stack")?;
    let unsafe_stack = allocate_unsafe_stack(TASK_UNSAFE_STACK_SIZE, "SafeStack data stack")?;

    Some(TaskCreateResources {
        process_id: process.disarm(),
        stack_base: stack_top - TASK_STACK_SIZE,
        kernel_stack,
        unsafe_stack,
    })
}

fn allocate_task_create_resources(flags: u16) -> Option<TaskCreateResources> {
    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        allocate_kernel_task_resources()
    } else {
        allocate_user_task_resources()
    }
}

/// Release resources allocated by `allocate_task_create_resources` when
/// the surrounding `task_create` bails out mid-flight.
///
/// The caller passes both stacks by value so their `Drop` runs here
/// (releasing VA slots; the physical frames remain mapped for reuse by
/// the next slot allocation — see `KernelStack::drop` / `UnsafeStack::drop`).
fn cleanup_task_create_resources(
    process_id: u32,
    kernel_stack: KernelStack,
    unsafe_stack: UnsafeStack,
) {
    ProcessResourceLease::cleanup_owned_process(process_id, true, true);
    drop(kernel_stack);
    drop(unsafe_stack);
}

enum TaskProcessCleanupMode {
    KeepVm,
    DropVm,
}

fn cleanup_task_process_resources(
    task_ptr: *mut Task,
    resolved_id: u32,
    mode: TaskProcessCleanupMode,
) {
    if task_exit_cleanup_mark(task_ptr, TASK_EXIT_CLEANUP_RESOURCES) & TASK_EXIT_CLEANUP_RESOURCES
        != 0
    {
        run_task_resource_cleanup_hooks(resolved_id);
    }

    let process_id = super::task_accessors::task_process_id(task_ptr).unwrap_or(INVALID_PROCESS_ID);
    if process_id == INVALID_PROCESS_ID {
        return;
    }

    let task_id = task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID);
    if !process_has_other_live_tasks(process_id, task_id) {
        fileio_destroy_table_for_process(process_id);
        if matches!(mode, TaskProcessCleanupMode::DropVm)
            && task_exit_cleanup_mark(task_ptr, TASK_EXIT_CLEANUP_VM) & TASK_EXIT_CLEANUP_VM != 0
        {
            destroy_process_vm(process_id);
        }
    }
}

fn process_has_other_live_tasks(process_id: u32, excluding_task_id: u32) -> bool {
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.task_id == excluding_task_id {
                continue;
            }
            if matches!(
                task.status(),
                TaskStatus::Invalid | TaskStatus::Terminated | TaskStatus::Zombie
            ) {
                continue;
            }
            if task.process_id == process_id {
                return true;
            }
        }
        false
    })
}

/// Bytes reserved at the top of every user task's per-task kernel stack
/// for handling interrupts/exceptions that arrive from user mode.
///
/// `TSS.RSP0` and `pcr.kernel_rsp` point to `kernel_stack_top`; on a
/// user→kernel transition the CPU pushes the IRET frame there and the
/// ISR/handler chain grows downward.  Concurrently, `user_task_loop`
/// (the OSTD round-trip supervisor) holds a multi-hundred-byte safe-stack
/// frame *on the same stack*, including a SafeStack-saved unsafe-SP slot
/// at `[rbp-0xb8]`.
///
/// If the supervisor's frame sat at the top of the stack (the historical
/// `kernel_stack_top - 16` arrangement), every IRQ from user mode would
/// step onto that frame:
/// `common_exception_handler_impl` alone allocates 264 bytes of safe
/// stack, which pulls RSP into the supervisor's [rbp-0xb8] slot.  The
/// resulting clobber surfaces later as a kernel page fault when the
/// supervisor reads its now-corrupt unsafe-SP back.
///
/// The fix: place the supervisor's RSP at `kernel_stack_top -
/// SUPERVISOR_RESERVE`.  That gives the CPU `SUPERVISOR_RESERVE` bytes
/// at the top of the stack to land IRETs, ISR pushes, and the deepest
/// IRQ-handler call chain (timer → scheduler → context-switch); the
/// supervisor's own frame plus the syscall-dispatch chain live below
/// that line and cannot be reached by IRQ-from-user-mode pushes.
///
/// 0x2000 (8 KiB) covers the worst observed IRQ chain (CPU IRET + ISR
/// pushes + `common_exception_handler` + `common_exception_handler_impl`
/// + scheduler timer-tick / context-switch through `switch_registers`,
/// totalling ~2 KiB safe-stack frames) with comfortable margin.  The
/// remaining 24 KiB of the 32 KiB per-task kernel stack covers the
/// supervisor + every syscall handler's call chain.
///
/// The per-task-stack model splits each task's kernel stack into
/// supervisor + IRQ regions and places the supervisor at the bottom.
/// No scheduler / TSS / per-task-RSP plumbing required.
const SUPERVISOR_RESERVE: u64 = 0x2000;

const _: () = {
    // 16-byte alignment: required for SystemV ABI.  After `ret` pops
    // the synthetic RA from `kernel_stack_top - SUPERVISOR_RESERVE`,
    // RSP = `kernel_stack_top - SUPERVISOR_RESERVE + 8`; with
    // `SUPERVISOR_RESERVE` a multiple of 16 this is `mod 16 == 8`,
    // satisfying "RSP mod 16 == 8 at function entry".
    assert!(SUPERVISOR_RESERVE % 16 == 0);
    // Bound: must fit comfortably inside the per-task kernel stack so
    // both halves remain usable.  Cap at half the stack — a
    // SUPERVISOR_RESERVE that ate more than half would leave the
    // supervisor + every syscall-dispatch chain crammed into <16 KiB.
    assert!(SUPERVISOR_RESERVE < TASK_KERNEL_STACK_SIZE / 2);
    // Floor: must hold the worst-case IRQ chain (CPU IRET + ISR pushes
    // + Rust handler frames through the scheduler's context-switch).
    // 4 KiB is the smallest value that still has comfortable margin.
    assert!(SUPERVISOR_RESERVE >= 0x1000);
};

/// Build a user-task entry frame on a task's kernel stack.  The frame
/// is empty save for a single return-address slot containing
/// `user_task_first_run`'s address.  When `switch_registers` executes
/// `ret`, control jumps into `user_task_first_run`, which seeds
/// `pcr.user_ctx_ptr` and enters the OSTD `UserMode::execute()` round
/// trip.  The user-mode register state must already have been written
/// to `task.user_ctx` by the caller (typically via
/// `crate::syscall::user_loop::init_user_ctx_*`).
///
/// The slot lives at `kernel_stack_top - SUPERVISOR_RESERVE` (rather
/// than at the very top of the stack) so the IRQ-handler chain that
/// fires on user→kernel transitions, which lands at `TSS.RSP0 =
/// kernel_stack_top`, has `SUPERVISOR_RESERVE` bytes of headroom before
/// it could overlap with the supervisor's own frame.  See the
/// `SUPERVISOR_RESERVE` comment for the full rationale.  Alignment:
/// after `ret` pops the synthetic address, `RSP = kernel_stack_top -
/// SUPERVISOR_RESERVE + 8`; with `SUPERVISOR_RESERVE` a multiple of 16
/// and `kernel_stack_top` page-aligned, that satisfies the SystemV ABI's
/// "`RSP mod 16 == 8` at function entry" invariant.
///
/// # Safety
/// Caller must ensure that the slot at `kernel_stack_top -
/// SUPERVISOR_RESERVE` is writable, properly aligned, and not
/// concurrently accessed. This is upheld by the surrounding
/// `task_create` / `task_fork` / `task_clone` paths, where the
/// kernel stack was just allocated and no other CPU can observe it.
pub(crate) fn build_user_task_entry_frame(kernel_stack_top: u64) -> SwitchContext {
    let entry = task_externs::user_task_first_run as *const () as u64;
    let ret_addr_slot = kernel_stack_top - SUPERVISOR_RESERVE;
    // OSTD's `write_kernel_va` carries the one `unsafe`; the caller-
    // facing contract says the kernel stack was just allocated and no
    // observer is yet attached.
    slopos_ostd::util::ptr_buf::write_kernel_va::<u64>(ret_addr_slot, entry);
    SwitchContext {
        rbx: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rbp: 0,
        rsp: ret_addr_slot,
        rflags: 0x02,
        rip: entry,
        // Fresh task starts at the running baseline (preemption enabled).
        preempt_count: 0,
    }
}

/// First thing a fresh kernel task runs, reached from
/// [`task_entry_trampoline`] with this task's **id** in `rdi`.
///
/// An **id**, not an address: a task handle parked in a saved register file
/// across every context switch until first dispatch, then cast back, is a
/// handle with no owner reconstituted on faith — which is exactly what checks
/// 3a and 3b of `check_task_ownership.sh` name.
///
/// An id is a scalar. It cannot be dereferenced, so it carries no claim about
/// the task still existing, and the task is re-derived from the PCR instead —
/// which is authoritative, because by the time this runs the task *is* this
/// CPU's current. `execute_task` calls `dispatch` (which publishes the PCR)
/// before `switch_context`, and the trampoline `sti`s before calling here, so
/// `Current::get()` is two gs-relative loads that cannot fail. Deliberately
/// **not** a registry lookup: that would take the global registry cli-spinlock
/// and scan it on every kernel thread's first dispatch to learn what the PCR
/// already publishes.
///
/// The id survives only as a cross-check — a `debug_assert` that the task the
/// PCR names is the one this shim was seeded for.
///
/// `extern "sysv64" fn(u64)` rather than `extern "C" fn(*mut c_void)` so
/// "this argument is not a caller payload" is checked by the compiler. The
/// trampoline ABI is one integer in `rdi` either way, so no asm moves.
extern "sysv64" fn kernel_task_entry_shim(task_id: u64) {
    let seeded = {
        let Some(current) = crate::task_struct::Current::get() else {
            klog_info!("kernel_task_entry_shim: no current task on this CPU");
            return;
        };
        debug_assert_eq!(
            current.id() as u64,
            task_id,
            "entry shim ran on a different task than it was seeded for"
        );
        let task = current.task();
        let raw_entry = task.entry_point as usize as *mut ();
        let entry = slopos_ostd::util::fn_ptr::fn_ptr_from_raw::<TaskEntry>(raw_entry);
        let fatal_if_panics = crate::per_cpu::is_idle_task(slopos_ostd::task::TaskAddr::of(task))
            || (task.flags & TASK_FLAG_SYSTEM) != 0
            || !slopos_ostd::panic_recovery::production_recovery_enabled();
        // `kernel_thread_trampoline` (the spawn() facade's entry point) already
        // wraps the real entry in its own run_recoverable — skip the shim's
        // boundary for it so a panic is caught exactly once.
        let already_recoverable =
            task.entry_point == crate::runtime::kernel_thread_trampoline as *const () as u64;
        // The guard is dropped here, before `entry` runs: the task body may
        // block, migrate or exit, and none of that is a borrow this frame
        // should still be holding.
        (entry, task.entry_arg, fatal_if_panics, already_recoverable)
    };
    let (entry, arg, fatal_if_panics, already_recoverable) = seeded;

    if fatal_if_panics || already_recoverable {
        entry(arg);
        return;
    }

    match slopos_ostd::panic_recovery::run_recoverable(|| entry(arg)) {
        Ok(()) => {}
        Err(oops) => {
            klog_info!(
                "panic recovery: legacy kthread task={} {}:{}:{}: {} (oops total={})",
                oops.task_id,
                oops.file.as_str(),
                oops.line,
                oops.column,
                oops.reason.as_str(),
                slopos_ostd::panic_recovery::oops_count(),
            );
        }
    }
}

fn init_task_context(task: &mut Task) {
    *task.context.get_mut() = TaskContext::default();
    // `task.fpu_state` is a valid in-place FpuState owned by the
    // caller-provided `&mut Task`. Writing into it does not alias
    // with any other reference because we hold the unique `&mut`.
    super::task_accessors::task_reset_fpu_state(task);

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        let trampoline = task_entry_trampoline as *const () as u64;
        let shim = kernel_task_entry_shim as *const () as u64;
        // The task's id, not its address: a scalar the trampoline can carry in
        // a saved register file without it being an unowned task handle.
        let shim_arg = task.task_id as u64;
        super::task_accessors::task_kernel_stack_seed_ret(task.kernel_stack_top, trampoline);
        *task.switch_ctx.get_mut() =
            SwitchContext::new_for_task(shim, shim_arg, task.kernel_stack_top, trampoline);
        task.context.get_mut().rip = trampoline;
        task.context.get_mut().rsi = shim_arg;
        task.context.get_mut().rdi = shim;
        task.context.get_mut().rsp = task.stack_pointer;
        task.context.get_mut().rflags = 0x202;
        task.context.get_mut().cs = 0x08;
        task.context.get_mut().ds = 0x10;
        task.context.get_mut().es = 0x10;
        task.context.get_mut().ss = 0x10;
    } else {
        // OSTD user-mode entry: populate `task.user_ctx` with the
        // initial register snapshot and set up the kernel stack so
        // `switch_registers` rets into `user_task_first_run`.  The
        // first iteration of `user_task_loop` will iretq into user
        // mode at (entry_point, stack_pointer) with rdi=entry_arg.
        crate::task::init_user_ctx_for_new_task(
            task.user_ctx.get_mut(),
            task.entry_point,
            task.stack_pointer,
            task.entry_arg as u64,
        );
        // SAFETY: kernel_stack region was just allocated and is writable.
        *task.switch_ctx.get_mut() = build_user_task_entry_frame(task.kernel_stack_top);
        task.context.get_mut().rip = task.entry_point;
        task.context.get_mut().rsp = task.stack_pointer;
        task.context.get_mut().rflags = 0x202;
        task.context.get_mut().cs = 0x23;
        task.context.get_mut().ds = 0x1B;
        task.context.get_mut().es = 0x1B;
        task.context.get_mut().fs = 0x1B;
        task.context.get_mut().gs = 0x1B;
        task.context.get_mut().ss = 0x1B;
        task.context.get_mut().rdi = task.entry_arg as u64;
    }

    task.context.get_mut().cr3 = 0;
}

/// Walk a caller-owned C string up to `TASK_NAME_MAX_LEN-1` bytes,
/// copying into `dest` and NUL-padding the remainder.
///
/// Copy `src` (NUL-terminated kernel pointer) into the fixed-length
/// `dest` buffer, padding the tail with zero. Null `src` clears `dest`.
///
/// The interior `unsafe` (NUL-bounded walk through the C string) lives
/// inside OSTD's `cstr_from_kernel_ptr`; this consumer sees only the
/// resulting byte slice.
fn copy_name(dest: &mut [u8; TASK_NAME_MAX_LEN], src: *const c_char) {
    *dest = [0u8; TASK_NAME_MAX_LEN];
    let Some(bytes) = slopos_ostd::util::cstr::cstr_from_kernel_ptr(src) else {
        return;
    };
    let take = core::cmp::min(bytes.len(), TASK_NAME_MAX_LEN - 1);
    dest[..take].copy_from_slice(&bytes[..take]);
    // `dest[take]` is already 0 from the wipe above; tail stays zero.
}

/// Build a task and hand back the token that solely owns it.
///
/// The task is fully formed — address space, both stacks, entry frame, job
/// control identity — and reachable through nothing but the returned token, so
/// a caller with more to write finishes construction through
/// [`PendingTask::as_mut`] and only then makes the task reachable with
/// [`task_commit`]. That ordering is what removes the pre-publication window
/// entirely rather than papering over it: there is no interval in which a
/// half-built task is visible to a registry lookup or an active-task walk.
///
/// A caller with nothing to add wants [`task_create`], which is this and
/// [`task_commit`] with no gap between them.
pub fn task_build(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    mut flags: u16,
) -> Option<PendingTask> {
    if entry_point as usize == 0 {
        klog_info!("task_create: Invalid entry point");
        return None;
    }

    if flags & TASK_FLAG_KERNEL_MODE == 0 && flags & TASK_FLAG_USER_MODE == 0 {
        flags |= TASK_FLAG_USER_MODE;
    }

    if flags & TASK_FLAG_KERNEL_MODE != 0 && flags & TASK_FLAG_USER_MODE != 0 {
        klog_info!("task_create: Conflicting mode flags");
        return None;
    }

    let mut pending = match allocate_task() {
        Ok(pending) => pending,
        Err(TaskAllocError::MaxTasks) => {
            klog_info!("task_create: Maximum tasks reached");
            return None;
        }
        Err(TaskAllocError::NoFreeSlot | TaskAllocError::IdExhausted) => {
            klog_info!("task_create: No free task slots");
            return None;
        }
    };
    let task_id = pending.id();

    let resources = match allocate_task_create_resources(flags) {
        Some(resources) => resources,
        None => {
            drop(pending);
            return None;
        }
    };

    // Both ways this can still fail are decided by the arguments alone, so
    // they are settled before the first field write rather than midway
    // through it. That is also what lets the initialisation below hold one
    // uninterrupted exclusive borrow: an early return in the middle would
    // have had to move `pending` out from under it.
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        klog_info!("task_create: user entry outside user_text window");
        cleanup_task_create_resources(
            resources.process_id,
            resources.kernel_stack,
            resources.unsafe_stack,
        );
        drop(pending);
        return None;
    }

    // A user task is born its own session and group leader. Kernel tasks never
    // join a terminal session, so they carry no group object (ints only).
    let process_group = if flags & TASK_FLAG_USER_MODE != 0 {
        match new_session_group(task_id) {
            Some(pg) => Some(pg),
            None => {
                cleanup_task_create_resources(
                    resources.process_id,
                    resources.kernel_stack,
                    resources.unsafe_stack,
                );
                drop(pending);
                return None;
            }
        }
    } else {
        None
    };

    // Exclusive because the token holds the only reference to this task.
    let task_ref = pending.as_mut();
    task_ref.task_id = task_id;
    copy_name(&mut task_ref.name, name);
    // Status stays Blocked (set during allocation) until fully initialised.
    task_ref.priority = TaskPriority::from_u8(priority);
    task_ref.flags = flags;
    task_ref.process_id = resources.process_id;
    task_ref.tgid = task_id;
    task_ref.set_pgid(task_id);
    task_ref.set_sid(task_id);
    task_ref.set_controlling_tty(None);
    // The token proves this task is unpublished, so there is no reader to defer
    // a release past and no displaced handle to release — exclusivity carries
    // the write, not a grace period.
    let _ = task_ref.process_group.replace_exclusive(process_group);
    task_ref.set_clear_child_tid(0);
    task_ref.parent_task_id = INVALID_TASK_ID;
    task_ref.stack_base = resources.stack_base;
    task_ref.stack_size = TASK_STACK_SIZE;
    task_ref.stack_pointer = resources.stack_base + TASK_STACK_SIZE - 8;

    // Populate the plain-u64 fields from the `KernelStack` handle, then
    // move ownership of the handle into the task slot.  Dropping the
    // task's `kernel_stack = None` later releases all backing memory.
    let kstack_base = resources.kernel_stack.base().as_u64();
    let kstack_top = resources.kernel_stack.top().as_u64();
    let kstack_size = resources.kernel_stack.size() as u64;
    task_ref.kernel_stack_base = kstack_base;
    task_ref.kernel_stack_top = kstack_top;
    task_ref.kernel_stack_size = kstack_size;
    task_ref.kernel_stack = Some(resources.kernel_stack);
    // Install the unsafe stack and prime its RSP at the top.  Every
    // instrumented function prologue walks this pointer downward; at
    // context-switch time it is saved/restored exactly like RSP.
    task_ref.abi.unsafe_stack_sp = resources.unsafe_stack.top().as_u64();
    task_ref.unsafe_stack = Some(resources.unsafe_stack);
    task_ref.entry_point = entry_point as usize as u64;
    task_ref.entry_arg = arg;
    task_ref.time_slice = 10;
    task_ref.reset_runtime_state();
    task_ref.user_started.store(0, Ordering::Relaxed);
    task_ref.context_from_user.store(0, Ordering::Relaxed);

    init_task_context(task_ref);

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        task_ref.context.get_mut().cr3 = cpu::read_cr3() & !0xFFF;
    } else {
        // task.context.cr3 holds the OSTD PML4 paddr — that's what
        // VmSpace::activate writes to CR3 during context switch, and
        // the user-fault dispatcher compares it against hardware CR3.
        task_ref.context.get_mut().cr3 =
            slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(resources.process_id);
    }

    Some(pending)
}

/// Make a built task reachable, returning the strong reference that pins it.
///
/// The task becomes findable here, not runnable: `scheduler::publish_new_task`
/// is the sole new-task runnable edge, and it reserves scheduler placement and
/// publishes `Ready` as one protocol. Between the two the task is registered
/// but nascent, which is why every field a caller owns is written *before* this
/// call, through the token's exclusive borrow.
///
/// A registry that cannot take the task abandons it, so the token is consumed
/// either way and no caller can be left holding one it must remember to release.
pub fn task_commit(pending: PendingTask) -> Option<TaskRef> {
    let task_id = pending.id();
    let registered = match register_task(pending) {
        Ok(registered) => registered,
        Err(pending) => {
            klog_info!("task_commit: task registry full");
            task_abandon(pending);
            return None;
        }
    };
    record_task_created();

    klog_debug!(
        "Created task '{}' with ID {}",
        bytes_as_str(&registered.name),
        task_id
    );

    Some(registered)
}

/// Release a task whose construction failed before it was ever made reachable.
///
/// The token owns the task, so dropping it releases both stacks and everything
/// else the task itself holds. The address space and its file table are
/// process-scoped and outlive any single task, so they are torn down only when
/// no other live task shares the process — the same rule the ordinary exit path
/// applies, and what makes this correct for a `CLONE_VM` thread whose address
/// space belongs to the parent it was branching from.
pub fn task_abandon(mut pending: PendingTask) {
    let (task_id, process_id) = {
        let task = pending.as_mut();
        (task.task_id, task.process_id)
    };
    if process_id != INVALID_PROCESS_ID && !process_has_other_live_tasks(process_id, task_id) {
        ProcessResourceLease::cleanup_owned_process(process_id, true, true);
    }
    drop(pending);
}

/// Build and commit a task in one step, for callers with nothing to add
/// between the two. Returns the new task's id, or [`INVALID_TASK_ID`].
pub fn task_create(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    flags: u16,
) -> u32 {
    let Some(pending) = task_build(name, entry_point, arg, priority, flags) else {
        return INVALID_TASK_ID;
    };
    let task_id = pending.id();
    match task_commit(pending) {
        Some(_) => task_id,
        None => INVALID_TASK_ID,
    }
}

pub fn task_terminate(task_id: u32) -> c_int {
    // One owning handle covers both entry shapes. A named id resolves through
    // the registry, which hands back a guard. `u32::MAX` means "me", and there
    // the handle is minted from this CPU's current-task pointer — sound because
    // the dispatch reference the scheduler parked on the idle stack keeps this
    // task alive for exactly as long as it is the current one.
    //
    // Holding it for the whole function is what lets the teardown below work
    // from a borrow: nothing it calls can be the last release.
    let target: Option<TaskRef> = if task_id == u32::MAX {
        NonNull::new(scheduler::scheduler_get_current_task()).map(TaskRef::clone_of)
    } else {
        task_find_by_id(task_id)
    };

    let Some(target) = target else {
        if task_id == u32::MAX {
            klog_info!("task_terminate: No current task to terminate");
            return -1;
        }
        // A specific id that no longer resolves either named a task that has
        // been fully terminated and reclaimed (idempotent success) or one
        // that never existed. Monotonic non-reused ids tell the two apart.
        if super::task_table::task_id_was_allocated(task_id) {
            return 0;
        }
        klog_info!("task_terminate: Task not found");
        return -1;
    };

    let task: &Task = &target;
    let task_ptr = target.as_ptr();
    let resolved_id = if task_id == u32::MAX {
        task.task_id
    } else {
        task_id
    };

    // A resolvable `Invalid` task is not a missing one — it is a fork/clone
    // child between `register_task` and `publish_new_task`. Its construction is
    // complete (stacks, address space and user context are all installed before
    // registration); only scheduler publication is outstanding, which is why it
    // is still hidden from every active-task scan. Refusing to terminate it is
    // what abandoned it on the pre-publication failure paths — a failed
    // `publish_new_task`, or a faulting `CLONE_*_SETTID` write — leaking the
    // task together with its kernel stack, unsafe stack and address space.
    // `mark_task_terminated` force-publishes the terminal status, so the
    // `Invalid -> Ready`-only transition rule does not stand in the way.

    if matches!(task.status(), TaskStatus::Terminated | TaskStatus::Zombie) {
        return 0;
    }

    klog_info!(
        "Terminating task '{}' (ID {})",
        bytes_as_str(&task.name),
        resolved_id
    );

    let is_current = TaskAddr::current() == Some(TaskAddr::of(task));

    // Hold preemption disabled across the `mark_task_terminated` →
    // cleanup → defer/free sequence below. Without this guard, a
    // spinlock-drop deep inside `mark_task_terminated` (e.g. inside
    // `release_task_dependents` → `task_wake_all_waiters` → `wake_all`
    // → `unblock_task` → `schedule_task`'s runqueue lock) can release
    // the only outstanding `PreemptGuard` while a `reschedule_pending`
    // flag is set. That flag was set by a previous timer-tick ISR that
    // saw `is_scheduling_active()` true but couldn't reschedule because
    // some inner critical section still held preemption.
    //
    // When the spinlock guard finally drops with preempt_count → 0 and
    // reschedule_pending set, OSTD invokes
    // `deferred_reschedule_callback`, which calls `schedule()`. We are
    // already in `Zombie` state (mark_task_terminated transitioned us
    // there before its first lock-drop), so the scheduler picks any
    // other task — and the current task's task_terminate continuation
    // (the call to `cleanup_task_process_resources` two lines below)
    // never runs. Zombie tasks are not schedulable, so the dropped
    // continuation stays dropped: the fd table is never destroyed, FD
    // refcounts on inherited file objects never reach zero, and any
    // waiter on a pipe whose only writer was this task hangs forever.
    let _preempt = slopos_ostd::cpu::preempt::PreemptGuard::new();
    mark_task_terminated(task, resolved_id);

    let defer_cleanup_to_running_cpu = !is_current && task_on_cpu_load(task);
    if is_current {
        cleanup_task_process_resources(task_ptr, resolved_id, TaskProcessCleanupMode::KeepVm);
    } else if !defer_cleanup_to_running_cpu {
        cleanup_terminated_task_resources(task_ptr, resolved_id);
    }

    let account_here = !is_current
        && !defer_cleanup_to_running_cpu
        && task_exit_cleanup_mark(task_ptr, TASK_EXIT_CLEANUP_ACCOUNTED)
            & TASK_EXIT_CLEANUP_ACCOUNTED
            != 0;
    with_task_manager(|mgr| {
        if account_here && mgr.num_tasks > 0 {
            mgr.num_tasks -= 1;
        }
        mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);
    });

    // Release through the reclaim path, never through `KArc`'s own `Drop`. This
    // can be the final reference — terminating a task that no queue, inbox or
    // parent list still holds leaves this handle as the last one — and the
    // preempt guard above is still live, so the destructor must not run here.
    // `task_put` parks it for the graveyard instead.
    super::task_put(target);
    0
}

/// What [`stamp_exit_state`] hands to the teardown tail.
///
/// Carries the decisions the stamping phase made, so the tail does not have to
/// re-derive them from fields it has already retired.
struct ExitPlan {
    /// `clear_child_tid`, if this task is the one running and the address is
    /// non-zero — i.e. if the futex clear is ours to perform.
    clear_tid: Option<u64>,
}

/// Write every field the exit path owns.
///
/// Shared, not exclusive. The dying task is still reachable: a peer CPU may be
/// in `wake_blocked_task` reading its status and placement, and its parent may
/// be in `waitpid` reading `exit_info`. An `&mut` here would have claimed those
/// readers do not exist. Every field this writes is therefore atomic or behind
/// a lock, which is also what lets the teardown tail below keep using this one
/// borrow rather than re-deriving one per call.
fn stamp_exit_state(task: &Task, resolved_id: u32, now: u64) -> ExitPlan {
    let last_run = task.last_run_timestamp();
    if last_run != 0 && now >= last_run {
        task.add_total_runtime(now - last_run);
    }
    task.set_last_run_timestamp(0);
    if TaskExitReason::from_u16(task.exit_reason.load(Ordering::Acquire)) == TaskExitReason::None {
        task.exit_reason
            .store(TaskExitReason::Kernel.as_u16(), Ordering::Release);
    }

    // Stash the per-task test-report ring (if any) so the userland-test
    // runner can drain reports even after slot recycle. Exit info no
    // longer needs an out-of-slot copy: `Task::exit_info` is the single
    // source of truth, kept stable until either `waitpid` consumes the
    // Zombie or the parent itself dies and auto-reaps.
    // One `take` under the lock rather than `is_some()` then `take()`: the
    // ring is installed by the owning task on its first report, so the old
    // two-step could observe `Some` and then take `None`. The guard is a
    // temporary, so it is released before `stash_pending_drain` runs and the
    // `KBox` never travels under the lock.
    let reports = task.test_reports.lock().take();
    if reports.is_some() {
        crate::test_reports::stash_pending_drain(
            resolved_id,
            crate::test_reports::PendingDrain { reports },
        );
    }

    // Publish exit_info BEFORE the status transition so any racing
    // observer that sees status==Zombie is guaranteed to see a fully
    // populated `exit_info` cell on its next read. Order is load-bearing:
    // `task_consume_zombie` keys on `status == Zombie` and then reads
    // `exit_info.try_get()`; with this ordering, the consumer never
    // sees Zombie+empty (which would force it to either return None
    // and spin, or transition to Terminated and silently drop the
    // exit code).
    //
    // `try_set` returns Err on a re-entry (e.g. fault path also calling
    // task_terminate(self)); discard via `_` — the operation is
    // idempotent.
    // Acquire loads pairing with the Release stores above and in
    // `task_record_user_fault_exit`: whoever publishes the status below must
    // publish a fully-populated `ExitInfo`, and these three are its inputs.
    let info = ExitInfo {
        exit_code: task.exit_code.load(Ordering::Acquire) as i32,
        exit_reason: TaskExitReason::from_u16(task.exit_reason.load(Ordering::Acquire)),
        fault_reason: TaskFaultReason::from_u16(task.fault_reason.load(Ordering::Acquire)),
        signal: 0,
        exit_time_ms: now,
    };
    let _ = task.exit_info.try_set(info);

    // Pick Zombie or Terminated based on whether a live parent might
    // call `waitpid` on us. Kernel-mode tasks and orphans skip Zombie
    // and go straight to Terminated — their exit code has no consumer,
    // so the slot is immediately reapable.
    let parent_alive = parent_alive_for(task.parent_task_id);
    let final_status = if parent_alive {
        TaskStatus::Zombie
    } else {
        TaskStatus::Terminated
    };
    task.set_status(final_status);
    task.clear_fate();

    // The futex clear is ours only when we are the running task. The swap is
    // what elects a single performer: whoever takes a non-zero address does
    // the wake, and there is no window in which two teardowns both see one.
    let clear_tid = if TaskAddr::current() == Some(TaskAddr::of(task)) {
        Some(task.take_clear_child_tid()).filter(|addr| *addr != 0)
    } else {
        None
    };

    ExitPlan { clear_tid }
}

/// Drive one task to its terminal status and unhook it from everything that
/// still names it.
///
/// The caller holds an owning reference for the whole call — either the
/// registry guard it resolved the id through, or, for self-termination, a
/// handle minted from this CPU's dispatch reference. That is what makes a
/// single borrow good across the whole sequence: nothing below can be the last
/// release, because the caller's handle outlives all of them.
fn mark_task_terminated(task: &Task, resolved_id: u32) {
    let now = kdiag_timestamp();

    let plan = stamp_exit_state(task, resolved_id, now);

    scheduler::cancel_sleep(resolved_id);
    crate::futex::futex_remove_task(resolved_id);

    // Release any waitpid wait-reference this task held on its target. A task
    // SIGKILL'd while parked in `task_wait_for` never unwinds its own stack,
    // so the reference it took on the target must be dropped here or the
    // target cannot be destroyed.
    scheduler::release_wait_ref(resolved_id);

    if let Some(clear_tid) = plan.clear_tid {
        if let Ok(clear_ptr) = UserPtr::<u32>::try_new(clear_tid) {
            let _ = copy_to_user(clear_ptr, &0u32);
        }
        let _ = crate::futex::futex_wake_one(clear_tid);
    }

    notify_parent_of_child_exit(task);

    // Session-leader hangup. Every field here is either a frozen identity or an
    // atomic, so a shared borrow suffices.
    let should_hangup = if task.task_id != INVALID_TASK_ID
        && task.is_session_leader()
        && let Some(tty_idx) = task.controlling_tty()
    {
        task.set_controlling_tty(None);
        Some(tty_idx)
    } else {
        None
    };

    scheduler::unschedule_task(task);

    // A task that dies before it was ever published leaves `Nascent` behind.
    // Retire it to `None` so placement keeps meaning "no scheduler owner" for
    // every dead task, whatever stage it died at.
    let _ = slopos_ostd::task::accessors::task_sched_placement_compare_exchange(
        task,
        slopos_ostd::task::SchedPlacement::Nascent,
        slopos_ostd::task::SchedPlacement::None,
    );

    release_task_dependents(resolved_id);

    // Adopt children: live ones become orphans (parent id cleared, so their
    // later termination skips Zombie); zombie ones are auto-reaped (Zombie →
    // Terminated). There is no userland-PID-1 reaper, so orphan adoption happens
    // entirely in the kernel.
    reparent_and_reap_children(task);

    if let Some(tty_idx) = should_hangup {
        tty::hangup(tty_idx);
    }
}

/// True if `parent_id` refers to a task slot that is currently
/// runnable / blocked (i.e. has not itself exited). Used by
/// [`mark_task_terminated`] to decide between Zombie (live parent will
/// reap) and Terminated (no reaper, slot immediately reusable).
fn parent_alive_for(parent_id: u32) -> bool {
    if parent_id == INVALID_TASK_ID {
        return false;
    }
    let Some(parent) = task_find_by_id(parent_id) else {
        return false;
    };
    match task_status(parent.as_ptr()) {
        Some(TaskStatus::Ready) | Some(TaskStatus::Running) | Some(TaskStatus::Blocked) => true,
        _ => false,
    }
}

/// Drain the dying task's owned children list — O(children), not a registry
/// scan. For each child:
///   * clear its parent id so a live child's later exit skips the Zombie state
///     and a reaped zombie leaves no dangling parent id;
///   * if it is already a Zombie, demote it to Terminated (the would-be reaper
///     just died, so its exit code has no consumer and it can be destroyed);
///   * drop the owning reference the list held.
///
/// Each `take_one_child` pop runs under the registry lock; the returned
/// reference is dropped off-lock and is never the last one, because the child
/// still holds its own existence reference until it is reaped.
///
/// A child demoted to `Terminated` here has just lost its only reaper, so this
/// is the one place that must reap it. A live child keeps its existence
/// reference and reaps itself when it exits.
fn reparent_and_reap_children(dying: &Task) {
    while let Some(child) = super::take_one_child(dying) {
        let child_ptr = child.as_ptr();
        let child_id = task_id_of(child_ptr).unwrap_or(INVALID_TASK_ID);
        super::task_accessors::task_set_parent_task_id(child_ptr, INVALID_TASK_ID);
        let orphaned_zombie =
            child.status() == TaskStatus::Zombie && child.try_transition_to(TaskStatus::Terminated);
        super::task_put(child);
        if orphaned_zombie {
            let _ = task_reap(child_id);
        }
    }
}

fn cleanup_terminated_task_resources(task_ptr: *mut Task, resolved_id: u32) {
    if task_on_cpu_load(task_ptr) {
        return;
    }

    cleanup_task_process_resources(task_ptr, resolved_id, TaskProcessCleanupMode::DropVm);
    task_recovery_depth_store(task_ptr, 0);
    task_panic_in_flight_store(task_ptr, 0);

    let _ = task_reap(task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID));
}

pub fn cleanup_current_task_after_switch(task_ptr: *mut Task) {
    if task_ptr.is_null() {
        return;
    }
    let Some(status) = task_status(task_ptr) else {
        return;
    };
    if !matches!(status, TaskStatus::Terminated | TaskStatus::Zombie) {
        return;
    }
    if super::task_accessors::task_kernel_stack_top(task_ptr).unwrap_or(0) == 0 {
        return;
    }

    let resolved_id = task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID);
    cleanup_task_process_resources(task_ptr, resolved_id, TaskProcessCleanupMode::DropVm);
    task_recovery_depth_store(task_ptr, 0);
    task_panic_in_flight_store(task_ptr, 0);

    if task_exit_cleanup_mark(task_ptr, TASK_EXIT_CLEANUP_ACCOUNTED) & TASK_EXIT_CLEANUP_ACCOUNTED
        != 0
    {
        with_task_manager(|mgr| {
            if mgr.num_tasks > 0 {
                mgr.num_tasks -= 1;
            }
        });
    }

    let _ = task_reap(task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID));
}

#[inline]
fn should_collect_for_shutdown(task: &Task, task_ptr: *mut Task, current: *mut Task) -> bool {
    if task.status() == TaskStatus::Invalid {
        return false;
    }
    if task_ptr == current {
        return false;
    }
    if crate::per_cpu::is_idle_task(slopos_ostd::task::TaskAddr::of(task)) {
        return false;
    }
    task.task_id != INVALID_TASK_ID
}

fn collect_shutdown_task_ids(current: *mut Task) -> slopos_ostd::KVec<u32> {
    with_task_manager(|mgr| {
        let mut ids: slopos_ostd::KVec<u32> = slopos_ostd::KVec::new();
        for task in mgr.iter_tasks() {
            let task_ptr = task.as_ptr();
            if should_collect_for_shutdown(&task, task_ptr, current) {
                let _ = ids.push(task.task_id);
            }
        }
        ids
    })
}

fn terminate_task_ids(task_ids: &slopos_ostd::KVec<u32>) -> c_int {
    let mut result = 0;
    for task_id in task_ids.iter() {
        if task_terminate(*task_id) != 0 {
            result = -1;
        }
    }
    result
}

fn refresh_num_tasks_after_shutdown() {
    with_task_manager(|mgr| {
        let mut preserved = 0u32;
        for task in mgr.iter_tasks() {
            if !matches!(
                task.status(),
                TaskStatus::Invalid | TaskStatus::Terminated | TaskStatus::Zombie
            ) {
                preserved += 1;
            }
        }
        mgr.num_tasks = preserved;
    });
}

pub fn task_shutdown_all() -> c_int {
    let was_paused = crate::per_cpu::pause_all_aps();
    let current = scheduler::scheduler_get_current_task();
    let tasks_to_terminate = collect_shutdown_task_ids(current);
    let result = terminate_task_ids(&tasks_to_terminate);

    crate::per_cpu::clear_all_cpu_queues();
    refresh_num_tasks_after_shutdown();

    crate::per_cpu::resume_all_aps_if_not_nested(was_paused);
    // Queue teardown releases every parked reference, so this is where the last
    // one usually lands. Drain now — with the APs resumed and no lock held —
    // rather than leaving corpses for an idle pass that may never come.
    super::task_graveyard_drain();
    result
}

/// Flush the parent's live FPU/vector registers into its `fpu_state`
/// slot before a fork/clone copies that slot into the child. Without
/// this the child inherits the parent's last context-switch snapshot
/// rather than its register state at the `fork()`/`clone()` call site.
///
/// `save_current` captures the *current CPU's* vector file, so this is
/// gated on the parent being the running task — the fork/clone
/// self-syscall contract. If it ever isn't current, the scheduler's own
/// switch-out already holds a correct snapshot and we skip the flush.
fn flush_live_fpu_for_clone(parent: &Task) {
    let parent_addr = TaskAddr::of(parent);
    if TaskAddr::current() != Some(parent_addr) {
        return;
    }
    // The check above established the parent is this CPU's current task, so the
    // guard names it — and that is exactly the fact that makes capturing *this
    // CPU's* vector file the right thing to do.
    let Some(current) = crate::task_struct::Current::get() else {
        return;
    };
    if TaskAddr::of(current.task()) != parent_addr {
        return;
    }
    // Not a switch-out: the parent keeps running and its state stays live in
    // the register file, so the owner tag must keep saying so.
    current
        .task()
        .fpu_save_in_place(&current, slopos_ostd::cpu::x86_64::xsave::active_xcr0());
}

pub fn task_fork(
    parent: &Task,
    parent_user_ctx: *const slopos_ostd::user::context::UserContext,
) -> u32 {
    flush_live_fpu_for_clone(parent);

    if parent.process_id == INVALID_PROCESS_ID {
        klog_info!("task_fork: parent has no process VM (kernel task?)");
        return INVALID_TASK_ID;
    }

    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 {
        klog_info!("task_fork: cannot fork kernel-mode task");
        return INVALID_TASK_ID;
    }

    let child_process = match ProcessResourceLease::clone_from_parent(parent.process_id) {
        Some(process) => process,
        None => {
            klog_info!("task_fork: process_vm_clone_cow failed");
            return INVALID_TASK_ID;
        }
    };
    let child_process_id = child_process.process_id();

    let child_kernel_stack = match KernelStack::allocate(TASK_KERNEL_STACK_SIZE as usize) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_fork: kernel stack alloc failed: {:?}", e);
            return INVALID_TASK_ID;
        }
    };
    let child_kernel_stack_base = child_kernel_stack.base().as_u64();

    let child_unsafe_stack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_fork: data-stack alloc failed: {:?}", e);
            drop(child_kernel_stack);
            return INVALID_TASK_ID;
        }
    };
    let child_unsafe_stack_top = child_unsafe_stack.top().as_u64();

    let mut pending = match allocate_task() {
        Ok(pending) => pending,
        Err(_) => {
            klog_info!("task_fork: no free task slots");
            return INVALID_TASK_ID;
        }
    };
    let child_task_id = pending.id();

    // Exclusive because the token holds the only reference to the child; the
    // parent is a separate allocation reached through a shared borrow, so one
    // `&mut` and one `&` coexist without aliasing.
    let child = pending.as_mut();

    super::task_accessors::task_clone_from(child, parent);

    // The bulk copy above may carry the parent's saved recovery/in-flight
    // depths, which were written at its last switch-out and can be stale
    // (e.g. saved mid-unwind, then the parent caught the panic without
    // another switch-out). The child starts outside any recovery scope.
    task_recovery_depth_store(child as *mut Task, 0);
    task_panic_in_flight_store(child as *mut Task, 0);

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    // The parent link and children-list membership are published together, after
    // registration, via `link_child` — never a bare field write.
    child.tgid = child_task_id;
    child.set_pgid(parent.pgid());
    child.set_sid(parent.sid());
    // Share the parent's group object (clone_from_raw emptied the copied
    // slot); the shared identity is what a stale pid check keys on. Exclusive
    // because the child is not published yet.
    let _ = child
        .process_group
        .replace_exclusive(task_process_group(parent as *const Task));
    child.set_clear_child_tid(0);

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack.top().as_u64();
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // OSTD user-mode entry: seed `child.user_ctx` from the parent's
    // syscall-time UserContext (when supplied) with rax forced to 0
    // for fork's child return, and set up the kernel stack so
    // `switch_registers` rets into `user_task_first_run`. The caller-
    // facing contract on `parent_user_ctx` (null or valid snapshot)
    // is upheld by `task_fork`'s callers in the syscall layer;
    // OSTD's `try_borrow_ref` carries the one `unsafe` deref.
    let parent_ctx_opt = slopos_ostd::util::ptr_buf::try_borrow_ref::<
        slopos_ostd::user::context::UserContext,
    >(parent_user_ctx);
    if let Some(parent_ctx) = parent_ctx_opt {
        let mut regs = *parent_ctx.regs();
        regs.rax = 0;
        child.user_ctx.get_mut().set_regs(regs);
    } else {
        // No parent UserContext available — `clone_from_raw` already
        // copied the parent's `user_ctx` into the child; force rax to
        // 0 through `set_regs` so CS/SS/RFLAGS-mask invariants hold.
        let mut regs = *child.user_ctx.get_mut().regs();
        regs.rax = 0;
        child.user_ctx.get_mut().set_regs(regs);
    }
    // SAFETY: child kernel stack was just allocated and is writable.
    *child.switch_ctx.get_mut() = build_user_task_entry_frame(child.kernel_stack_top);
    child.context_from_user.store(0, Ordering::Relaxed);
    child.context.get_mut().rax = 0;

    let child_page_dir = process_vm_get_page_dir(child_process_id);
    if !child_page_dir.is_null() {
        child.context.get_mut().cr3 =
            slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(child_process_id);
    }

    child.reset_runtime_state();
    let _ = child_process.disarm();
    // Transfer ownership of the kernel stack into the task; its `Drop`
    // runs when the task is destroyed.
    child.kernel_stack = Some(child_kernel_stack);
    child.abi.unsafe_stack_sp = child_unsafe_stack_top;
    child.unsafe_stack = Some(child_unsafe_stack);

    // Held across link_child and publication below: registration alone would
    // let a concurrent shutdown sweep reclaim the child under them, and this
    // handle is also where those two get the child's address from.
    let Some(registered) = task_commit(pending) else {
        return INVALID_TASK_ID;
    };

    klog_debug!(
        "task_fork: created child task {} (process {}) from parent task {} (process {})",
        child_task_id,
        child_process_id,
        parent.task_id,
        parent.process_id
    );

    // Publish the parent→child ownership edge: sets the child's parent id and
    // parks one owning reference in the parent's children list. Done after
    // registration (so the child is a live registry entry) and after the
    // `child` borrow above has ended. The parent is the borrow this function
    // was handed, which outlives every child-side write.
    if let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr()) {
        super::link_child(parent, child_nn);
    }

    // Publish Ready only after all child-specific fields are fully initialized.
    // `publish_new_task` reserves scheduler placement before setting Ready, so
    // the child is never visible as Ready with no runqueue/inbox owner.
    if scheduler::publish_new_task(&registered) != 0 {
        klog_info!("fork: initial runnable publish failed for task {child_task_id}");
        let _ = task_terminate(child_task_id);
        return INVALID_TASK_ID;
    }

    child_task_id
}

pub fn task_clone(
    parent: &Task,
    parent_user_ctx: *const slopos_ostd::user::context::UserContext,
    flags: u64,
    child_stack: u64,
    parent_tidptr: u64,
    child_tidptr: u64,
    tls: u64,
) -> Result<u32, u64> {
    use slopos_abi::syscall::*;

    flush_live_fpu_for_clone(parent);

    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 || parent.process_id == INVALID_PROCESS_ID {
        return Err(ERRNO_EINVAL);
    }

    if flags & !CLONE_SUPPORTED_MASK != 0 {
        klog_info!("task_clone: unsupported flags 0x{:x}", flags);
        return Err(ERRNO_EINVAL);
    }

    if flags & CLONE_THREAD != 0 {
        if flags & CLONE_VM == 0 || flags & CLONE_SIGHAND == 0 {
            klog_info!("task_clone: CLONE_THREAD requires CLONE_VM | CLONE_SIGHAND");
            return Err(ERRNO_EINVAL);
        }
    }

    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        klog_info!("task_clone: CLONE_SIGHAND requires CLONE_VM");
        return Err(ERRNO_EINVAL);
    }

    if flags & CLONE_FILES != 0 && flags & CLONE_VM == 0 {
        klog_info!("task_clone: CLONE_FILES without CLONE_VM is unsupported");
        return Err(ERRNO_EINVAL);
    }

    let share_vm = flags & CLONE_VM != 0;
    let is_thread = flags & CLONE_THREAD != 0;

    let child_process = if share_vm {
        ProcessResourceLease::none()
    } else {
        match ProcessResourceLease::clone_from_parent(parent.process_id) {
            Some(process) => process,
            None => {
                klog_info!("task_clone: process_vm_clone_cow failed");
                return Err(ERRNO_ENOMEM);
            }
        }
    };
    let child_process_id = if share_vm {
        parent.process_id
    } else {
        child_process.process_id()
    };

    let child_kernel_stack = match KernelStack::allocate(TASK_KERNEL_STACK_SIZE as usize) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_clone: kernel stack alloc failed: {:?}", e);
            return Err(ERRNO_ENOMEM);
        }
    };
    let child_kernel_stack_base = child_kernel_stack.base().as_u64();

    let child_unsafe_stack = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_clone: data-stack alloc failed: {:?}", e);
            drop(child_kernel_stack);
            return Err(ERRNO_ENOMEM);
        }
    };
    let child_unsafe_stack_top = child_unsafe_stack.top().as_u64();

    let mut pending = match allocate_task() {
        Ok(pending) => pending,
        Err(_) => return Err(ERRNO_EAGAIN),
    };
    let child_task_id = pending.id();

    // Exclusive because the token holds the only reference to the child; the
    // parent is a separate allocation reached through a shared borrow, so one
    // `&mut` and one `&` coexist without aliasing.
    let child = pending.as_mut();

    super::task_accessors::task_clone_from(child, parent);

    // The bulk copy above may carry the parent's saved recovery/in-flight
    // depths, which were written at its last switch-out and can be stale
    // (e.g. saved mid-unwind, then the parent caught the panic without
    // another switch-out). The child starts outside any recovery scope.
    task_recovery_depth_store(child as *mut Task, 0);
    task_panic_in_flight_store(child as *mut Task, 0);

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    // The parent link and children-list membership are published together, after
    // registration, via `link_child` — never a bare field write.

    if is_thread {
        child.tgid = if parent.tgid != INVALID_TASK_ID {
            parent.tgid
        } else {
            parent.task_id
        };
    } else {
        child.tgid = child_task_id;
    }
    child.set_pgid(parent.pgid());
    child.set_sid(parent.sid());
    // Share the parent's group object (clone_from_raw emptied the copied
    // slot); the shared identity is what a stale pid check keys on. Exclusive
    // because the child is not published yet.
    let _ = child
        .process_group
        .replace_exclusive(task_process_group(parent as *const Task));

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack.top().as_u64();
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // OSTD user-mode entry: seed `child.user_ctx` from the parent's
    // live syscall-time `UserContext` — the registers captured at the
    // `clone` trap, whose `rip` is the instruction after the syscall.
    // The child resumes there with `rax = 0` so the libc clone shim
    // takes its child branch and runs the new thread's start routine.
    // Seeding from the legacy `context` instead would resume the child
    // at a stale rip (e.g. the ELF entry), re-running `main`. This
    // mirrors `task_fork`; `task_clone_from` already copied the
    // parent's `user_ctx` as the fallback when no live snapshot is
    // supplied.
    {
        let parent_ctx_opt = slopos_ostd::util::ptr_buf::try_borrow_ref::<
            slopos_ostd::user::context::UserContext,
        >(parent_user_ctx);
        let mut regs = match parent_ctx_opt {
            Some(parent_ctx) => *parent_ctx.regs(),
            None => *child.user_ctx.get_mut().regs(),
        };
        regs.rax = 0;
        if child_stack != 0 {
            regs.rsp = child_stack;
        }
        if flags & CLONE_SETTLS != 0 {
            regs.fs_base = tls;
        }
        child.user_ctx.get_mut().set_regs(regs);
        // SAFETY: child kernel stack was just allocated and is writable.
        *child.switch_ctx.get_mut() = build_user_task_entry_frame(child.kernel_stack_top);
    }
    child.context_from_user.store(0, Ordering::Relaxed);
    child.context.get_mut().rax = 0;
    if child_stack != 0 {
        child.context.get_mut().rsp = child_stack;
    }

    if flags & CLONE_CHILD_CLEARTID != 0 && child_tidptr != 0 {
        child.set_clear_child_tid(child_tidptr);
    } else {
        child.set_clear_child_tid(0);
    }

    if flags & CLONE_SETTLS != 0 {
        child.fs_base.store(tls, Ordering::Release);
    }

    if !share_vm {
        let child_page_dir = process_vm_get_page_dir(child_process_id);
        if !child_page_dir.is_null() {
            child.context.get_mut().cr3 =
                slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(child_process_id);
        }
    }

    child.reset_runtime_state();
    if !share_vm {
        let _ = child_process.disarm();
    }
    // Transfer ownership of the kernel stack to the task; its `Drop`
    // runs when the task is destroyed.
    child.kernel_stack = Some(child_kernel_stack);
    child.abi.unsafe_stack_sp = child_unsafe_stack_top;
    child.unsafe_stack = Some(child_unsafe_stack);
    let child_tgid = child.tgid;
    // Held across the settid writes, link_child and publication below.
    // Registration alone would let a concurrent shutdown sweep reclaim the
    // child under them; the scope-held guard also covers every faulting early
    // return between here and the end, and is where link_child reads the
    // child's address from.
    let Some(registered) = task_commit(pending) else {
        return Err(ERRNO_EAGAIN);
    };

    if flags & CLONE_PARENT_SETTID != 0 && parent_tidptr != 0 {
        let parent_tid_user = match UserPtr::<u32>::try_new(parent_tidptr) {
            Ok(p) => p,
            Err(_) => {
                let _ = task_terminate(child_task_id);
                return Err(ERRNO_EFAULT);
            }
        };
        if copy_to_user(parent_tid_user, &child_task_id).is_err() {
            let _ = task_terminate(child_task_id);
            return Err(ERRNO_EFAULT);
        }
    }

    if flags & CLONE_CHILD_SETTID != 0 && child_tidptr != 0 {
        if share_vm {
            let child_tid_user = match UserPtr::<u32>::try_new(child_tidptr) {
                Ok(p) => p,
                Err(_) => {
                    let _ = task_terminate(child_task_id);
                    return Err(ERRNO_EFAULT);
                }
            };
            if copy_to_user(child_tid_user, &child_task_id).is_err() {
                let _ = task_terminate(child_task_id);
                return Err(ERRNO_EFAULT);
            }
        }
    }

    klog_info!(
        "task_clone: created child task {} (process {}, tgid {}) flags=0x{:x} from parent {} (process {})",
        child_task_id,
        child_process_id,
        child_tgid,
        flags,
        parent.task_id,
        parent.process_id
    );

    // Publish the parent→child ownership edge (parent id + children-list
    // membership) after registration and after the `child` borrow above has
    // ended. The parent is the borrow this function was handed, which outlives
    // every child-side write.
    if let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr()) {
        super::link_child(parent, child_nn);
    }

    // Publish Ready only after all child-specific fields are fully initialized.
    // `publish_new_task` reserves scheduler placement before setting Ready, so
    // the child is never visible as Ready with no runqueue/inbox owner.
    if scheduler::publish_new_task(&registered) != 0 {
        klog_info!("clone: initial runnable publish failed for task {child_task_id}");
        let _ = task_terminate(child_task_id);
        return Err(ERRNO_EAGAIN);
    }

    Ok(child_task_id)
}
