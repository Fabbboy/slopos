use core::ffi::{c_char, c_int, c_void};
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use slopos_arch::cpu;
use slopos_ostd::KArc;
use slopos_ostd::kdiag_timestamp;
use slopos_ostd::process::{
    AccountId, PROCESS_HANDLE_NONE, Process, ProcessId, process_retire, process_spawn,
};
use slopos_ostd::string::bytes_as_str;
use slopos_ostd::task::ops::{
    TASK_EXIT_CLEANUP_ACCOUNTED, TASK_EXIT_CLEANUP_CHARGES, TASK_EXIT_CLEANUP_RESOURCES,
    TASK_EXIT_CLEANUP_VM,
};
use slopos_ostd::{klog_debug, klog_info};

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
    TaskPriority, TaskStatus,
};
use crate::exit_info::ExitInfo;
use crate::scheduler;
use crate::task_stack::{KernelStack, UnsafeStack};
use crate::task_struct::SwitchContext;
use slopos_fs::fileio::{
    FdTable, fileio_clone_table_for_process, fileio_create_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::process_vm::{
    create_process_vm_for, destroy_process_vm, pack_process_vm_handle, process_vm_clone_cow_for,
    process_vm_get_stack_top,
};
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::task::TaskAddr;

fn user_entry_is_allowed(addr: u64) -> bool {
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000;
    addr >= PROCESS_CODE_START_VA && addr < PROCESS_CODE_END
}

struct TaskCreateResources {
    process_id: u32,
    /// Packed handle to the task's address space; 0 for a kernel task.
    process_vm_handle: u64,
    /// The process the new task joins, or `None` for a kernel task.
    process: Option<KArc<Process>>,
    /// For a user task this lives in process VM; for a kernel task it aliases
    /// the kernel stack base.
    stack_base: u64,
    kernel_stack: KernelStack,
    unsafe_stack: UnsafeStack,
}

/// RAII lease over everything a new process owns — id, address space,
/// descriptor table and `KArc<Process>` — released on any early return.
/// [`disarm`](Self::disarm) hands ownership to the task that reached commit.
struct ProcessResourceLease {
    process_id: u32,
    /// Packed handle to the address space this lease created: names it without
    /// a pid lookup, which could not tell a later holder of the same id apart.
    process_vm_handle: u64,
    /// The process object, or `None` for a lease that owns nothing
    /// ([`none`](Self::none), the `CLONE_VM` case).
    process: Option<KArc<Process>>,
    owns_vm: bool,
    owns_file_table: bool,
}

impl ProcessResourceLease {
    const fn none() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            process_vm_handle: 0,
            process: None,
            owns_vm: false,
            owns_file_table: false,
        }
    }

    #[inline]
    const fn process_id(&self) -> u32 {
        self.process_id
    }

    #[inline]
    const fn process_vm_handle(&self) -> u64 {
        self.process_vm_handle
    }

    #[inline]
    fn process_handle(&self) -> u64 {
        self.process
            .as_ref()
            .map_or(PROCESS_HANDLE_NONE, |process| process.handle_raw())
    }

    /// Register a process object for `parent`'s child, or log and refuse.
    /// The accounting edge is the spawner's account, fixed here with no later
    /// opportunity to set it.
    fn mint_process(parent: Option<&Process>) -> Option<KArc<Process>> {
        let (wait_parent, account_parent) = match parent {
            Some(parent) => (parent.handle(), parent.account()),
            None => (None, slopos_ostd::process::quota::root()),
        };
        match process_spawn(wait_parent, account_parent) {
            Ok(process) => Some(process),
            Err(error) => {
                klog_info!("task_create: process registration failed: {:?}", error);
                None
            }
        }
    }

    fn create_user_process(parent: Option<&Process>) -> Option<Self> {
        let process = Self::mint_process(parent)?;

        // The address-space table and the process registry are one slot space:
        // the address space is indexed by process, never scanned for by id.
        let Some(vm) = create_process_vm_for(process.clone()) else {
            klog_info!("task_create: Failed to create process VM");
            return None;
        };
        let vm_id = ProcessId::of(&process)?;

        if process.handle().map(fileio_create_table_for_process) != Some(0) {
            destroy_process_vm(vm_id);
            return None;
        }

        Some(Self {
            process_id: vm.process_id,
            process_vm_handle: pack_process_vm_handle(vm.handle),
            process: Some(process),
            owns_vm: true,
            owns_file_table: true,
        })
    }

    fn clone_from_parent(parent: &Task) -> Option<Self> {
        let process = Self::mint_process(parent.process().as_deref())?;
        let parent_id = parent.process().as_deref().and_then(ProcessId::of)?;
        let child = process_vm_clone_cow_for(parent_id, process.clone())?;
        let child_id = ProcessId::of(&process)?;

        let Some(parent_table) = parent.process().and_then(|p| FdTable::of(&p)) else {
            destroy_process_vm(child_id);
            return None;
        };
        if process
            .handle()
            .map(|handle| fileio_clone_table_for_process(parent_table, handle))
            != Some(0)
        {
            destroy_process_vm(child_id);
            return None;
        }

        Some(Self {
            process_id: child.process_id,
            process_vm_handle: pack_process_vm_handle(child.handle),
            process: Some(process),
            owns_vm: true,
            owns_file_table: true,
        })
    }

    /// Hand ownership to the task that reached commit. Both fields are cleared
    /// here, so the `Drop` below releases nothing.
    fn disarm(&mut self) -> (u32, Option<KArc<Process>>) {
        let process_id = self.process_id;
        self.process_id = INVALID_PROCESS_ID;
        self.owns_vm = false;
        self.owns_file_table = false;
        (process_id, self.process.take())
    }

    /// Release the address space and descriptor table a process owns.
    ///
    /// Takes the process rather than its id: a pid would have to be
    /// re-resolved, and that lookup cannot tell a recycled number apart.
    fn cleanup_owned_process(
        process: Option<&KArc<Process>>,
        owns_vm: bool,
        owns_file_table: bool,
    ) {
        let Some(process) = process else {
            return;
        };
        if owns_file_table && let Some(handle) = process.handle() {
            fileio_destroy_table_for_process(handle);
        }
        if owns_vm && let Some(id) = ProcessId::of(process) {
            destroy_process_vm(id);
        }
    }
}

impl Drop for ProcessResourceLease {
    fn drop(&mut self) {
        // Teardown before retire: the two share a slot space, so retiring first
        // would free the registry slot with the old page tables still bound.
        Self::cleanup_owned_process(self.process.as_ref(), self.owns_vm, self.owns_file_table);
        // A lease that never reached an address space still holds a
        // registration, and only this can retire it.
        if let Some(process) = self.process.take()
            && let Some(handle) = process.handle()
        {
            process_retire(handle);
        }
    }
}

/// Both stacks for a cloned child, charged to the account they will serve.
///
/// `#[inline(never)]`: resolving the account in `task_clone` put the
/// temporaries in that frame and pushed it over the 2 KiB stack gate.
#[inline(never)]
fn clone_child_stacks(
    parent: &Task,
    child_process: &ProcessResourceLease,
    share_vm: bool,
) -> Option<(KernelStack, UnsafeStack)> {
    let account = if share_vm {
        parent.process().map_or(AccountId::NONE, |p| p.account())
    } else {
        child_process
            .process
            .as_deref()
            .map_or(AccountId::NONE, |p| p.account())
    };

    let kernel = match KernelStack::allocate(TASK_KERNEL_STACK_SIZE as usize, account) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_clone: kernel stack alloc failed: {:?}", e);
            return None;
        }
    };
    let data = match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize, account) {
        Ok(stack) => stack,
        Err(e) => {
            klog_info!("task_clone: data-stack alloc failed: {:?}", e);
            drop(kernel);
            return None;
        }
    };
    Some((kernel, data))
}

fn allocate_kernel_stack(size: u64, what: &'static str, account: AccountId) -> Option<KernelStack> {
    match KernelStack::allocate(size as usize, account) {
        Ok(s) => Some(s),
        Err(e) => {
            klog_info!("task_create: {} failed: {:?}", what, e);
            None
        }
    }
}

fn allocate_unsafe_stack(size: u64, what: &'static str, account: AccountId) -> Option<UnsafeStack> {
    match UnsafeStack::allocate(size as usize, account) {
        Ok(s) => Some(s),
        Err(e) => {
            klog_info!("task_create: {} failed: {:?}", what, e);
            None
        }
    }
}

fn allocate_kernel_task_resources() -> Option<TaskCreateResources> {
    // A kernel task has no process: the root account is named explicitly, not
    // taken as the residue of a failed lookup.
    let account = slopos_ostd::process::quota::root();
    let kernel_stack = allocate_kernel_stack(TASK_STACK_SIZE, "kernel stack", account)?;
    let unsafe_stack =
        allocate_unsafe_stack(TASK_UNSAFE_STACK_SIZE, "SafeStack data stack", account)?;
    let stack_base = kernel_stack.base().as_u64();
    Some(TaskCreateResources {
        process_id: INVALID_PROCESS_ID,
        process_vm_handle: 0,
        process: None,
        stack_base,
        kernel_stack,
        unsafe_stack,
    })
}

fn allocate_user_task_resources() -> Option<TaskCreateResources> {
    // No parent: this is a process the kernel starts, not one a process
    // spawned — a spawn goes through `clone_from_parent`.
    let mut process = ProcessResourceLease::create_user_process(None)?;
    let vm_id = process.process.as_deref().and_then(ProcessId::of)?;

    let stack_top = process_vm_get_stack_top(vm_id);
    if stack_top == 0 {
        klog_info!("task_create: Failed to get process stack");
        return None;
    }

    let account = process
        .process
        .as_deref()
        .map_or(AccountId::NONE, |p| p.account());
    let kernel_stack = allocate_kernel_stack(TASK_KERNEL_STACK_SIZE, "kernel RSP0 stack", account)?;
    let unsafe_stack =
        allocate_unsafe_stack(TASK_UNSAFE_STACK_SIZE, "SafeStack data stack", account)?;
    let process_vm_handle = process.process_vm_handle();
    let (process_id, process) = process.disarm();

    Some(TaskCreateResources {
        process_id,
        process_vm_handle,
        process,
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

/// Release resources allocated by `allocate_task_create_resources` when the
/// surrounding `task_create` bails out mid-flight. Takes the whole bundle by
/// value so nothing can be forgotten.
fn cleanup_task_create_resources(resources: TaskCreateResources) {
    let TaskCreateResources {
        process,
        kernel_stack,
        unsafe_stack,
        ..
    } = resources;
    ProcessResourceLease::cleanup_owned_process(process.as_ref(), true, true);
    if let Some(process) = process.as_ref()
        && let Some(handle) = process.handle()
    {
        process_retire(handle);
    }
    drop(kernel_stack);
    drop(unsafe_stack);
}

enum TaskProcessCleanupMode {
    KeepVm,
    DropVm,
}

fn cleanup_task_process_resources(task: &Task, resolved_id: u32, mode: TaskProcessCleanupMode) {
    if task.exit_cleanup_mark(TASK_EXIT_CLEANUP_RESOURCES) & TASK_EXIT_CLEANUP_RESOURCES != 0 {
        run_task_resource_cleanup_hooks(resolved_id);
    }

    // The task's own process, not a re-resolution of its id: this is the exit
    // path, so the id may already name somebody else by the time a lookup ran.
    let Some(process) = task.process() else {
        return;
    };

    if !task_leaves_process(task) {
        return;
    }

    if let Some(handle) = process.handle() {
        fileio_destroy_table_for_process(handle);
    }
    if matches!(mode, TaskProcessCleanupMode::DropVm)
        && task.exit_cleanup_mark(TASK_EXIT_CLEANUP_VM) & TASK_EXIT_CLEANUP_VM != 0
    {
        if let Some(id) = ProcessId::of(&process) {
            destroy_process_vm(id);
        }
    }
}

/// Give back `task`'s share of its process, answering whether it was the last.
///
/// Latched by `TASK_EXIT_CLEANUP_CHARGES`: exit cleanup runs from both an
/// external `task_terminate` and the owning CPU's post-switch path, and a
/// second decrement would report a live process as torn down — a second
/// `destroy_process_vm` on an address space another task is running in.
///
/// A task whose process handle no longer resolves answers `false`; the process
/// was already reaped.
///
/// The registration is **not** retired here: `destroy_process_vm` retires as
/// its own last step, after the unbind.
fn task_leaves_process(task: &Task) -> bool {
    if task.exit_cleanup_mark(TASK_EXIT_CLEANUP_CHARGES) & TASK_EXIT_CLEANUP_CHARGES == 0 {
        return false;
    }
    let Some(process) = task.process() else {
        return false;
    };
    if !process.task_leave() {
        return false;
    }
    process.mark_exited();
    true
}

/// Bytes reserved at the top of every user task's per-task kernel stack for
/// the interrupt/exception chain that arrives from user mode.
///
/// `TSS.RSP0` and `pcr.kernel_rsp` point at `kernel_stack_top`, so IRQ pushes
/// land there and grow downward while `user_task_loop` holds a frame on the
/// same stack; the supervisor's RSP sits at `kernel_stack_top -
/// SUPERVISOR_RESERVE` so those pushes cannot reach it. 8 KiB covers the worst
/// observed IRQ chain (~2 KiB of safe-stack frames) with margin.
const SUPERVISOR_RESERVE: u64 = 0x2000;

const _: () = {
    // SystemV ABI: after `ret` pops the synthetic return address, RSP is
    // `mod 16 == 8` only if this is a multiple of 16.
    assert!(SUPERVISOR_RESERVE % 16 == 0);
    // Cap at half the stack, so the supervisor plus every syscall-dispatch
    // chain keeps the rest.
    assert!(SUPERVISOR_RESERVE < TASK_KERNEL_STACK_SIZE / 2);
    // Floor: the worst-case IRQ chain through the scheduler's context switch.
    assert!(SUPERVISOR_RESERVE >= 0x1000);
};

/// Build a user-task entry frame: a single return-address slot at
/// `kernel_stack_top - SUPERVISOR_RESERVE` holding the OSTD user-task entry,
/// so `switch_registers`' `ret` enters the `UserMode::execute()` round trip.
/// The caller must already have written the user-mode register state to
/// `task.user_ctx`.
///
/// # Safety
/// Caller must ensure that the slot at `kernel_stack_top -
/// SUPERVISOR_RESERVE` is writable, properly aligned, and not
/// concurrently accessed. This is upheld by the surrounding
/// `task_create` / `task_fork` / `task_clone` paths, where the
/// kernel stack was just allocated and no other CPU can observe it.
pub(crate) fn build_user_task_entry_frame(kernel_stack_top: u64) -> SwitchContext {
    let entry = slopos_ostd::task::user_task_entry_addr();
    let ret_addr_slot = kernel_stack_top - SUPERVISOR_RESERVE;
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
        preempt_count: 0,
    }
}

/// First thing a fresh kernel task runs, reached from
/// [`task_entry_trampoline`] with this task's **id** in `rdi`.
///
/// An **id**, not an address: a task handle parked in a saved register file
/// until first dispatch would be a handle with no owner — what checks 3a and 3b
/// of `check_task_ownership.sh` name. The task is re-derived from the PCR,
/// authoritative because by the time this runs the task *is* this CPU's
/// current; a registry lookup would take the global cli-spinlock to learn what
/// the PCR already publishes. The id survives only as a `debug_assert`
/// cross-check.
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
        let Some(entry) = slopos_ostd::util::fn_ptr::fn_ptr_decode_opt::<TaskEntry>(raw_entry)
        else {
            klog_info!(
                "kernel_task_entry_shim: task {} has a null entry point",
                task_id
            );
            return;
        };
        let fatal_if_panics = crate::per_cpu::is_idle_task(slopos_ostd::task::TaskAddr::of(task))
            || (task.flags & TASK_FLAG_SYSTEM) != 0
            || !slopos_ostd::panic_recovery::production_recovery_enabled();
        // `kernel_thread_trampoline` already wraps the real entry in its own
        // `run_recoverable`; skipping the shim's boundary keeps a panic caught
        // exactly once.
        let already_recoverable =
            task.entry_point == crate::runtime::kernel_thread_trampoline as *const () as u64;
        // The guard is dropped before `entry` runs: the task body may block,
        // migrate or exit, none of which this frame should still be borrowing
        // across.
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
    super::task_ops::task_reset_fpu_state(task);

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        let trampoline = task_entry_trampoline as *const () as u64;
        let shim = kernel_task_entry_shim as *const () as u64;
        let shim_arg = task.task_id as u64;
        super::task_ops::task_kernel_stack_seed_ret(task.kernel_stack_top, trampoline);
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

/// Copy the NUL-terminated `src` into `dest`, truncating to
/// `TASK_NAME_MAX_LEN-1` bytes and zero-padding the tail. A null `src` clears
/// `dest`.
fn copy_name(dest: &mut [u8; TASK_NAME_MAX_LEN], src: *const c_char) {
    *dest = [0u8; TASK_NAME_MAX_LEN];
    let Some(bytes) = slopos_ostd::util::cstr::cstr_from_kernel_ptr(src) else {
        return;
    };
    let take = core::cmp::min(bytes.len(), TASK_NAME_MAX_LEN - 1);
    dest[..take].copy_from_slice(&bytes[..take]);
}

/// Build a task and hand back the token that solely owns it.
///
/// The task is fully formed but reachable through nothing but the token: a
/// caller with more to write finishes construction through
/// [`PendingTask::as_mut`] and only then publishes with [`task_commit`], so no
/// half-built task is ever visible to a registry lookup or an active-task walk.
/// [`task_create`] is the two with no gap between them.
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

    // Non-blocking from here to the commit, where the token is the sole
    // reference to a task that already owns its process lease. Allocation under
    // the guard is fine — interrupts stay on, so cross-CPU TLB acks still land.
    let _preempt = slopos_ostd::cpu::preempt::PreemptGuard::new();
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

    // Argument-only failures are settled before the first field write, so the
    // initialisation below holds one uninterrupted exclusive borrow.
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        klog_info!("task_create: user entry outside user_text window");
        cleanup_task_create_resources(resources);
        drop(pending);
        return None;
    }

    // A user task is born its own session and group leader. Kernel tasks never
    // join a terminal session, so they carry no group object (ints only).
    let process_group = if flags & TASK_FLAG_USER_MODE != 0 {
        match new_session_group(task_id) {
            Some(pg) => Some(pg),
            None => {
                cleanup_task_create_resources(resources);
                drop(pending);
                return None;
            }
        }
    } else {
        None
    };

    let task_ref = pending.as_mut();
    task_ref.task_id = task_id;
    copy_name(&mut task_ref.name, name);
    // Status stays Blocked (set during allocation) until fully initialised.
    task_ref.priority = TaskPriority::from_u8(priority);
    task_ref.flags = flags;
    task_ref.process_id = resources.process_id;
    task_ref.set_process_vm_handle_raw(resources.process_vm_handle);
    // Join before the handle is stored, so the count is never behind the set
    // of tasks naming the process.
    if let Some(process) = resources.process.as_ref() {
        process.task_join();
        task_ref.set_process_handle_raw(process.handle_raw());
        // Charged here, not at `allocate_task`: there is no principal to bill
        // before the task has a process. Kept in the side table so the refund
        // lands at the exit latch rather than at the graveyard drain.
        if let Some(reservation) = super::task_quota::reserve(process.account()) {
            super::task_quota::commit(task_id, reservation);
        } else {
            process.task_leave();
            drop(pending);
            return None;
        }
    }
    task_ref.tgid = task_id;
    task_ref.set_pgid(task_id);
    task_ref.set_sid(task_id);
    task_ref.set_controlling_tty(None);
    // The token proves this task is unpublished: no reader to defer a release
    // past, and no displaced handle to release.
    let _ = task_ref.process_group.replace_exclusive(process_group);
    task_ref.set_clear_child_tid(0);
    task_ref.set_parent_task_id(INVALID_TASK_ID);
    task_ref.stack_base = resources.stack_base;
    task_ref.stack_size = TASK_STACK_SIZE;
    task_ref.stack_pointer = resources.stack_base + TASK_STACK_SIZE - 8;

    let kstack_base = resources.kernel_stack.base().as_u64();
    let kstack_top = resources.kernel_stack.top().as_u64();
    let kstack_size = resources.kernel_stack.size() as u64;
    task_ref.kernel_stack_base = kstack_base;
    task_ref.kernel_stack_top = kstack_top;
    task_ref.kernel_stack_size = kstack_size;
    task_ref.kernel_stack = Some(resources.kernel_stack);
    // Primed at the top: every instrumented prologue walks it downward.
    task_ref.abi.unsafe_stack_sp = resources.unsafe_stack.top().as_u64();
    task_ref.unsafe_stack = Some(resources.unsafe_stack);
    task_ref.entry_point = entry_point as usize as u64;
    task_ref.entry_arg = arg;
    task_ref.set_time_slice(10);
    task_ref.reset_runtime_state();
    task_ref.user_started.store(0, Ordering::Relaxed);
    task_ref.context_from_user.store(0, Ordering::Relaxed);

    init_task_context(task_ref);

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        task_ref.context.get_mut().cr3 = cpu::read_cr3() & !0xFFF;
    } else {
        // `context.cr3` holds the OSTD PML4 paddr: what `VmSpace::activate`
        // writes at switch time and what the user-fault dispatcher compares
        // against hardware CR3.
        task_ref.context.get_mut().cr3 = resources
            .process
            .as_deref()
            .and_then(ProcessId::of)
            .map_or(0, slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr);
    }

    Some(pending)
}

/// Make a built task reachable, returning the strong reference that pins it.
///
/// Findable, not runnable: `scheduler::publish_new_task` is the sole new-task
/// runnable edge, reserving scheduler placement and publishing `Ready` as one
/// protocol, so every field a caller owns is written *before* this call.
///
/// A registry that cannot take the task abandons it, so the token is consumed
/// either way.
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
/// The address space and its file table are process-scoped, so they are torn
/// down only when no other live task shares the process — which is what makes
/// this correct for a `CLONE_VM` thread whose address space is the parent's.
pub fn task_abandon(mut pending: PendingTask) {
    let process = {
        let task = pending.as_mut();
        task_leaves_process(task).then(|| task.process()).flatten()
    };
    if let Some(process) = process.as_ref() {
        ProcessResourceLease::cleanup_owned_process(Some(process), true, true);
        if let Some(handle) = process.handle() {
            process_retire(handle);
        }
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
    // `u32::MAX` means "me": the handle is minted from this CPU's current-task
    // pointer, sound because the dispatch reference the scheduler parked on the
    // idle stack keeps the current task alive.
    let target: Option<TaskRef> = if task_id == u32::MAX {
        crate::task_struct::Current::get()
            .and_then(|current| NonNull::new(current.as_ptr()).map(TaskRef::clone_of))
    } else {
        task_find_by_id(task_id)
    };

    let Some(target) = target else {
        if task_id == u32::MAX {
            klog_info!("task_terminate: No current task to terminate");
            return -1;
        }
        // An unresolvable id either named a task already terminated and
        // reclaimed (idempotent success) or one that never existed; monotonic
        // non-reused ids tell the two apart.
        if super::task_table::task_id_was_allocated(task_id) {
            return 0;
        }
        klog_info!("task_terminate: Task not found");
        return -1;
    };

    let task: &Task = &target;
    let resolved_id = if task_id == u32::MAX {
        task.task_id
    } else {
        task_id
    };

    // A resolvable `Invalid` task is a fork/clone child between `register_task`
    // and `publish_new_task` — fully constructed, so it is terminable here;
    // `mark_task_terminated` force-publishes the terminal status past the
    // `Invalid -> Ready`-only transition rule.

    if matches!(task.status(), TaskStatus::Terminated | TaskStatus::Zombie) {
        return 0;
    }

    klog_info!(
        "Terminating task '{}' (ID {})",
        bytes_as_str(&task.name),
        resolved_id
    );

    let is_current = TaskAddr::current() == Some(TaskAddr::of(task));

    // Preemption stays off across the `mark_task_terminated` → cleanup →
    // defer/free sequence: a spinlock drop inside `mark_task_terminated` would
    // otherwise release the last guard with `reschedule_pending` set, and this
    // task — already `Zombie`, so never scheduled again — would never run the
    // cleanup below.
    let _preempt = slopos_ostd::cpu::preempt::PreemptGuard::new();
    mark_task_terminated(task, resolved_id);

    let defer_cleanup_to_running_cpu = !is_current && task.on_cpu();
    if defer_cleanup_to_running_cpu {
        // Without the IPI the victim runs on its peer CPU until that CPU's next
        // timer tick; the tick handler's terminal-status escape is what turns
        // the interrupt into a deschedule.
        if let Some(cpu) = scheduler::cpu_running_task(TaskAddr::of(task)) {
            crate::lifecycle::send_reschedule_ipi(cpu);
        }
    }
    if is_current {
        cleanup_task_process_resources(task, resolved_id, TaskProcessCleanupMode::KeepVm);
    } else if !defer_cleanup_to_running_cpu {
        cleanup_terminated_task_resources(&target, resolved_id);
    }

    let account_here = !is_current
        && !defer_cleanup_to_running_cpu
        && task.exit_cleanup_mark(TASK_EXIT_CLEANUP_ACCOUNTED) & TASK_EXIT_CLEANUP_ACCOUNTED != 0;
    with_task_manager(|mgr| {
        if account_here && mgr.num_tasks > 0 {
            mgr.num_tasks -= 1;
        }
        mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);
    });
    if account_here {
        super::task_quota::release(resolved_id);
    }

    // Never `KArc`'s own `Drop`: this can be the final reference and the preempt
    // guard above is still live, so the destructor must not run here.
    // `task_put` parks it for the graveyard instead.
    super::task_put(target);
    0
}

/// What [`stamp_exit_state`] hands to the teardown tail: decisions the tail
/// cannot re-derive from fields already retired.
struct ExitPlan {
    /// `clear_child_tid`, if this task is the one running and the address is
    /// non-zero — i.e. if the futex clear is ours to perform.
    clear_tid: Option<u64>,
}

/// Write every field the exit path owns.
///
/// Shared, not exclusive: the dying task is still reachable — a peer CPU may be
/// reading its status and placement, its parent its `exit_info` — so every
/// field written here is atomic or behind a lock.
fn stamp_exit_state(task: &Task, now: u64) -> ExitPlan {
    let last_run = task.last_run_timestamp();
    if last_run != 0 && now >= last_run {
        task.add_total_runtime(now - last_run);
    }
    task.set_last_run_timestamp(0);
    if TaskExitReason::from_u16(task.exit_reason.load(Ordering::Acquire)) == TaskExitReason::None {
        task.exit_reason
            .store(TaskExitReason::Kernel.as_u16(), Ordering::Release);
    }

    // Published before the status transition: `task_consume_zombie` keys on
    // `status == Zombie` and then reads `exit_info`, so Zombie+empty would make
    // it spin or drop the exit code. The Acquire loads pair with the Release
    // stores above and in `task_record_user_fault_exit`; a failing `try_set` is
    // a re-entry, and the operation is idempotent.
    let info = ExitInfo {
        exit_code: task.exit_code.load(Ordering::Acquire) as i32,
        exit_reason: TaskExitReason::from_u16(task.exit_reason.load(Ordering::Acquire)),
        fault_reason: TaskFaultReason::from_u16(task.fault_reason.load(Ordering::Acquire)),
        signal: 0,
        exit_time_ms: now,
    };
    let _ = task.exit_info.try_set(info);

    let parent_alive = parent_alive_for(task.parent_task_id());
    let final_status = if parent_alive {
        TaskStatus::Zombie
    } else {
        TaskStatus::Terminated
    };
    let _ = task.set_status(final_status);
    task.clear_fate();

    // The swap elects a single performer: whoever takes a non-zero address does
    // the wake, and no window lets two teardowns both see one.
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
/// The caller holds an owning reference for the whole call, which is what makes
/// a single borrow good across the sequence: nothing below can be the last
/// release.
fn mark_task_terminated(task: &Task, resolved_id: u32) {
    let now = kdiag_timestamp();

    let plan = stamp_exit_state(task, now);

    scheduler::cancel_sleep(resolved_id);

    // A `wait_event*` node lives on the dying task's kernel stack, which is
    // recycled rather than quarantined: a node left linked is written through
    // by the next wake on that queue, after another task owns the memory.
    slopos_ostd::sync::wait_queue::purge_parked_wait_node(task.parked_wait_queue(), resolved_id);

    if let Some(clear_tid) = plan.clear_tid {
        if let Ok(clear_ptr) = UserPtr::<u32>::try_new(clear_tid) {
            let _ = copy_to_user(clear_ptr, &0u32);
        }
        let _ = crate::futex::futex_wake_one(clear_tid);
    }

    notify_parent_of_child_exit(task);

    // The parent is the one principal whose zombie budget can have been
    // exceeded, and by exactly one.
    if task.status() == TaskStatus::Zombie
        && let Some(parent) = task_find_by_id(task.parent_task_id())
    {
        super::enforce_zombie_budget(&parent);
    }

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

    // A task that dies before it was ever published leaves `Nascent` behind;
    // retiring it keeps placement meaning "no scheduler owner" for every dead
    // task, whatever stage it died at.
    let _ = task.sched_placement_compare_exchange(
        slopos_ostd::task::SchedPlacement::Nascent,
        slopos_ostd::task::SchedPlacement::None,
    );

    release_task_dependents(resolved_id);

    // There is no userland PID-1, so orphan adoption happens in the kernel.
    reparent_and_reap_children(task);

    if let Some(tty_idx) = should_hangup {
        tty::hangup(tty_idx);
    }
}

/// True if `parent_id` names a task that has not itself exited, deciding
/// between Zombie (a live parent will reap) and Terminated (no reaper, slot
/// immediately reusable).
///
/// A parent that has explicitly set `SIGCHLD` to `SIG_IGN` answers `false` even
/// while running: it will never reap, so the Zombie would be held forever.
/// POSIX `SA_NOCLDWAIT` semantics.
fn parent_alive_for(parent_id: u32) -> bool {
    if parent_id == INVALID_TASK_ID {
        return false;
    }
    let Some(parent) = task_find_by_id(parent_id) else {
        return false;
    };
    if !matches!(
        parent.status(),
        TaskStatus::Ready | TaskStatus::Running | TaskStatus::Blocked
    ) {
        return false;
    }
    !parent_disclaims_children(&parent)
}

/// Whether `parent` has explicitly disclaimed reaping by installing `SIG_IGN`
/// for `SIGCHLD`.
///
/// `SIG_DFL` is deliberately excluded even though SlopOS maps `SIGCHLD`'s
/// default action to `Ignore`: the default disposition discards the
/// *notification*, an explicit `SIG_IGN` also discards the *status*.
/// Conflating them would leave `waitpid` nothing to reap.
fn parent_disclaims_children(parent: &Task) -> bool {
    let idx = (slopos_abi::signal::SIGCHLD - 1) as usize;
    parent.signal_handler(idx) == Some(slopos_abi::signal::SIG_IGN)
}

/// Drain the dying task's owned children list: a live child's parent id is
/// cleared so its later exit skips Zombie, and a child already Zombie is
/// demoted and reaped here, having just lost its only reaper.
///
/// Each `take_one_child` pop runs under the registry lock; the returned
/// reference is dropped off-lock and is never the last one, because the child
/// holds its own existence reference until it is reaped.
fn reparent_and_reap_children(dying: &Task) {
    while let Some(child) = super::take_one_child(dying) {
        let child_id = child.task_id;
        child.set_parent_task_id(INVALID_TASK_ID);
        let orphaned_zombie =
            child.status() == TaskStatus::Zombie && child.try_transition_to(TaskStatus::Terminated);
        super::task_put(child);
        if orphaned_zombie {
            let _ = task_reap(child_id);
        }
    }
}

/// Takes the guard rather than a borrow: the `task_reap` below may run the
/// destructor inline, so the caller's handle is what keeps the body addressable
/// across this call.
fn cleanup_terminated_task_resources(task: &TaskRef, resolved_id: u32) {
    if task.on_cpu() {
        return;
    }

    cleanup_task_process_resources(task, resolved_id, TaskProcessCleanupMode::DropVm);
    task.set_recovery_depth(0);
    task.set_panic_in_flight(0);

    let _ = task_reap(task.task_id);
}

/// Same guard contract as [`cleanup_terminated_task_resources`].
pub fn cleanup_current_task_after_switch(task: &TaskRef) {
    if !matches!(task.status(), TaskStatus::Terminated | TaskStatus::Zombie) {
        return;
    }
    if task.kernel_stack_top == 0 {
        return;
    }

    let resolved_id = task.task_id;

    cleanup_task_process_resources(task, resolved_id, TaskProcessCleanupMode::DropVm);
    task.set_recovery_depth(0);
    task.set_panic_in_flight(0);

    if task.exit_cleanup_mark(TASK_EXIT_CLEANUP_ACCOUNTED) & TASK_EXIT_CLEANUP_ACCOUNTED != 0 {
        with_task_manager(|mgr| {
            if mgr.num_tasks > 0 {
                mgr.num_tasks -= 1;
            }
        });
        // The task's share of the per-process bound is free now, not when the
        // graveyard drains.
        super::task_quota::release(resolved_id);
    }

    let _ = task_reap(resolved_id);
}

#[inline]
fn should_collect_for_shutdown(task: &Task, current: Option<slopos_ostd::task::TaskAddr>) -> bool {
    if task.status() == TaskStatus::Invalid {
        return false;
    }
    if current == Some(slopos_ostd::task::TaskAddr::of(task)) {
        return false;
    }
    if crate::per_cpu::is_idle_task(slopos_ostd::task::TaskAddr::of(task)) {
        return false;
    }
    task.task_id != INVALID_TASK_ID
}

fn collect_shutdown_task_ids(
    current: Option<slopos_ostd::task::TaskAddr>,
) -> slopos_ostd::KVec<u32> {
    with_task_manager(|mgr| {
        let mut ids: slopos_ostd::KVec<u32> = slopos_ostd::KVec::new();
        for task in mgr.iter_tasks() {
            if should_collect_for_shutdown(&task, current) {
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

/// How long shutdown gives the kernel-I/O threads to finish. A thread that has
/// not stopped by then is torn down with everything else and named in the log.
const KERNEL_IO_JOIN_MS: u64 = 250;

/// Ask every kernel-I/O thread to stop and wait, bounded, for it to say it
/// finished.
///
/// Must run while every CPU is still scheduling: a thread parked on a paused
/// CPU cannot reach its own exit point, and its last act is the one that
/// matters (the ext2 flusher's final sync). Not part of the task sweep, which
/// also runs per test scope.
pub fn stop_kernel_io_tasks() {
    use slopos_ostd::sync::kernel_io_task::{
        for_each_unstopped_kernel_io, kernel_io_stops_pending, request_kernel_io_stop_all,
    };

    if request_kernel_io_stop_all() == 0 {
        return;
    }

    let deadline =
        slopos_kernel_services::platform::get_time_ms().saturating_add(KERNEL_IO_JOIN_MS);
    while kernel_io_stops_pending() > 0 {
        if slopos_kernel_services::platform::get_time_ms() >= deadline {
            break;
        }
        crate::scheduler::yield_();
    }

    // This log line is the list of threads whose park is not stop-aware.
    for_each_unstopped_kernel_io(|name| {
        klog_info!("SCHED: kernel-io task '{}' did not stop in time", name);
    });
}

pub fn task_shutdown_all() -> c_int {
    // A pause that cannot be taken is stepped over rather than propagated:
    // refusing to tear tasks down would leave the machine up with no way
    // forward.
    let ap_pause = match crate::per_cpu::pause_all_aps() {
        Ok(token) => Some(token),
        Err(crate::per_cpu::ApPauseError::Timeout { cpu_id }) => {
            klog_info!(
                "SCHED: CPU {} would not park; shutting tasks down with the APs running",
                cpu_id
            );
            None
        }
    };
    let tasks_to_terminate = collect_shutdown_task_ids(slopos_ostd::task::TaskAddr::current());
    let result = terminate_task_ids(&tasks_to_terminate);

    crate::per_cpu::clear_all_cpu_queues();
    // Queue teardown runs before the recount: `refresh_num_tasks_after_shutdown`
    // recomputes `num_tasks` from the registry, which cannot see a parked token.
    refresh_num_tasks_after_shutdown();

    if let Some(token) = ap_pause {
        crate::per_cpu::resume_all_aps_if_not_nested(token);
    }
    // Drained with the APs resumed and no lock held: queue teardown released
    // the last parked references, and no idle pass is guaranteed to come.
    super::task_graveyard_drain();
    result
}

/// Flush the parent's live FPU/vector registers into its `fpu_state` before a
/// fork/clone copies that slot: otherwise the child inherits the parent's last
/// context-switch snapshot rather than its state at the call site.
///
/// Gated on the parent being the running task, since the save captures this
/// CPU's vector file; otherwise its switch-out snapshot is already correct.
fn flush_live_fpu_for_clone(parent: &Task) {
    let parent_addr = TaskAddr::of(parent);
    if TaskAddr::current() != Some(parent_addr) {
        return;
    }
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
    parent_user_ctx: Option<&slopos_ostd::user::context::UserContext>,
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

    let mut child_process = match ProcessResourceLease::clone_from_parent(parent) {
        Some(process) => process,
        None => {
            klog_info!("task_fork: process_vm_clone_cow failed");
            return INVALID_TASK_ID;
        }
    };
    let child_process_id = child_process.process_id();
    let child_process_handle = child_process.process_handle();
    let child_vm_id = child_process.process.as_deref().and_then(ProcessId::of);
    let stack_account = child_process
        .process
        .as_deref()
        .map_or(AccountId::NONE, |p| p.account());

    let child_kernel_stack =
        match KernelStack::allocate(TASK_KERNEL_STACK_SIZE as usize, stack_account) {
            Ok(stack) => stack,
            Err(e) => {
                klog_info!("task_fork: kernel stack alloc failed: {:?}", e);
                return INVALID_TASK_ID;
            }
        };
    let child_kernel_stack_base = child_kernel_stack.base().as_u64();

    let child_unsafe_stack =
        match UnsafeStack::allocate(TASK_UNSAFE_STACK_SIZE as usize, stack_account) {
            Ok(stack) => stack,
            Err(e) => {
                klog_info!("task_fork: data-stack alloc failed: {:?}", e);
                drop(child_kernel_stack);
                return INVALID_TASK_ID;
            }
        };
    let child_unsafe_stack_top = child_unsafe_stack.top().as_u64();

    let _preempt = slopos_ostd::cpu::preempt::PreemptGuard::new();
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

    super::task_ops::task_clone_from(child, parent);

    // The bulk copy above can carry the parent's recovery depths, saved at its
    // last switch-out and possibly stale; the child starts outside any
    // recovery scope.
    child.set_recovery_depth(0);
    child.set_panic_in_flight(0);

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    child.set_process_vm_handle_raw(child_process.process_vm_handle());
    // `clone_from_raw` cleared the copied handle, so a miss here leaves a child
    // with no process rather than one silently sharing its parent's.
    child.set_process_handle_raw(child_process_handle);
    // The parent link and children-list membership are published together, after
    // registration, via `link_child` — never a bare field write.
    child.tgid = child_task_id;
    child.set_pgid(parent.pgid());
    child.set_sid(parent.sid());
    // The parent's group object, shared: `clone_from_raw` emptied the copied
    // slot, and the shared identity is what a stale pid check keys on.
    let _ = child
        .process_group
        .replace_exclusive(parent.process_group.load());
    child.set_clear_child_tid(0);

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack.top().as_u64();
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // `rax = 0` is fork's child return, written through `set_regs` so the
    // CS/SS/RFLAGS-mask invariants hold.
    let mut regs = match parent_user_ctx {
        Some(parent_ctx) => parent_ctx.regs(),
        None => child.user_ctx.get_mut().regs(),
    };
    regs.rax = 0;
    child.user_ctx.get_mut().set_regs(regs);
    // SAFETY: child kernel stack was just allocated and is writable.
    *child.switch_ctx.get_mut() = build_user_task_entry_frame(child.kernel_stack_top);
    child.context_from_user.store(0, Ordering::Relaxed);
    child.context.get_mut().rax = 0;

    // `clone_from_raw` copied the parent's whole `TaskInner`, so `cr3` still
    // names the parent's root until this write; 0 is what the dispatcher and
    // `task_find_by_cr3` read as "no VM".
    child.context.get_mut().cr3 =
        child_vm_id.map_or(0, slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr);

    child.reset_runtime_state();
    if let (_, Some(process)) = child_process.disarm() {
        process.task_join();
    }
    child.kernel_stack = Some(child_kernel_stack);
    child.abi.unsafe_stack_sp = child_unsafe_stack_top;
    child.unsafe_stack = Some(child_unsafe_stack);

    // Held across `link_child` and publication below: registration alone would
    // let a concurrent shutdown sweep reclaim the child under them.
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

    // After registration and after the `child` borrow above has ended:
    // `link_child` parks one owning reference in the parent's children list.
    if let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr()) {
        super::link_child(parent, child_nn);
    }

    if scheduler::publish_new_task(&registered) != 0 {
        klog_info!("fork: initial runnable publish failed for task {child_task_id}");
        let _ = task_terminate(child_task_id);
        return INVALID_TASK_ID;
    }

    child_task_id
}

pub fn task_clone(
    parent: &Task,
    parent_user_ctx: Option<&slopos_ostd::user::context::UserContext>,
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

    let mut child_process = if share_vm {
        ProcessResourceLease::none()
    } else {
        match ProcessResourceLease::clone_from_parent(parent) {
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
    // A `CLONE_VM` thread is a member of the parent's process, not a new one,
    // so it carries the same handle: the exit-time count is what makes the last
    // member out — and only it — tear the address space down.
    let child_process_handle = if share_vm {
        parent.process_handle_raw()
    } else {
        child_process.process_handle()
    };
    let child_vm_id = child_process.process.as_deref().and_then(ProcessId::of);

    let Some((child_kernel_stack, child_unsafe_stack)) =
        clone_child_stacks(parent, &child_process, share_vm)
    else {
        return Err(ERRNO_ENOMEM);
    };
    let child_kernel_stack_base = child_kernel_stack.base().as_u64();
    let child_unsafe_stack_top = child_unsafe_stack.top().as_u64();

    let _preempt = slopos_ostd::cpu::preempt::PreemptGuard::new();
    let mut pending = match allocate_task() {
        Ok(pending) => pending,
        Err(_) => return Err(ERRNO_EAGAIN),
    };
    let child_task_id = pending.id();

    let child = pending.as_mut();

    super::task_ops::task_clone_from(child, parent);

    child.set_recovery_depth(0);
    child.set_panic_in_flight(0);

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    child.set_process_vm_handle_raw(if share_vm {
        parent.process_vm_handle_raw()
    } else {
        child_process.process_vm_handle()
    });
    child.set_process_handle_raw(child_process_handle);

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
    let _ = child
        .process_group
        .replace_exclusive(parent.process_group.load());

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack.top().as_u64();
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // Seeded from the parent's live syscall-time `UserContext`, whose `rip` is
    // the instruction after the `clone` trap; the legacy `context` would resume
    // the child at a stale rip (the ELF entry) and re-run `main`.
    {
        let mut regs = match parent_user_ctx {
            Some(parent_ctx) => parent_ctx.regs(),
            None => child.user_ctx.get_mut().regs(),
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

    // A VM-sharing child keeps the parent's root, already copied by
    // `clone_from_raw`.
    if !share_vm {
        child.context.get_mut().cr3 =
            child_vm_id.map_or(0, slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr);
    }

    child.reset_runtime_state();
    // Both branches join exactly once; the lease is disarmed in the same step
    // so the join and the ownership transfer cannot drift apart.
    if share_vm {
        if let Some(process) = parent.process() {
            process.task_join();
        }
    } else if let (_, Some(process)) = child_process.disarm() {
        process.task_join();
    }
    child.kernel_stack = Some(child_kernel_stack);
    child.abi.unsafe_stack_sp = child_unsafe_stack_top;
    child.unsafe_stack = Some(child_unsafe_stack);
    let child_tgid = child.tgid;
    // Held across the settid writes, `link_child` and publication below,
    // including every faulting early return between here and the end.
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

    if let Some(child_nn) = core::ptr::NonNull::new(registered.as_ptr()) {
        super::link_child(parent, child_nn);
    }

    if scheduler::publish_new_task(&registered) != 0 {
        klog_info!("clone: initial runnable publish failed for task {child_task_id}");
        let _ = task_terminate(child_task_id);
        return Err(ERRNO_EAGAIN);
    }

    Ok(child_task_id)
}
