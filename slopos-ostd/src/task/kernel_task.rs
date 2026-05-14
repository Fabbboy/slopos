//! Kernel-internal task control block, parameterised over the stack
//! handle types so the kernel side can plug in its own
//! `KernelStack` / `UnsafeStack` aliases without forcing OSTD to grow
//! a dependency on the kernel-side stack-region machinery.
//!
//! `TaskInner<K, U>` is the structural body. The kernel side defines
//! `pub type Task = TaskInner<KernelStack, UnsafeStack>` in
//! `core/src/scheduler/task_struct.rs` so existing callers continue to
//! spell the type as `Task`.

use core::ffi::c_void;
use core::ptr;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use slopos_abi::signal::{NSIG, SIG_DFL, SIG_EMPTY, SigSet};
use slopos_abi::syscall::TtyIndex;

pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_FPU_INITIALIZED, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE,
    TASK_NAME_MAX_LEN, TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord,
    TaskFaultReason, TaskPriority, TaskStatus,
};

use crate::cpu::x86_64::pcr::KernelReturnContext;
use crate::sync::AtomicCell;
use crate::sync::WaitQueue;
use crate::sync::intrusive::Link;
use crate::task::abi::TaskAbi;
use crate::task::exit_info::ExitInfo;
use crate::task::fpu::{FPU_STATE_SIZE, FpuState};
use crate::task::link_roles::{ReadyQueueRole, ZombieListRole};
use crate::task::state::TaskState;
use crate::task::test_reports::TestReportRing;
use crate::user::context::UserContext;
use crate::{AllocError, Init, KBox, init_from_closure};

// =============================================================================
// TaskContext — full CPU register state for interrupt-driven context switches
// =============================================================================

/// CPU register state saved during context switches.
/// Size: 200 bytes (0xC8) — 25 × 8-byte registers.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct TaskContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
    pub ss: u64,
    pub cr3: u64,
}

impl TaskContext {
    pub const fn zero() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            rsp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags: 0,
            cs: 0,
            ds: 0,
            es: 0,
            fs: 0,
            gs: 0,
            ss: 0,
            cr3: 0,
        }
    }
}

// =============================================================================
// SwitchContext — alias to the OSTD-owned callee-saved snapshot
// =============================================================================

/// Callee-saved snapshot consumed by the OSTD context-switch primitives in
/// [`crate::task::switch`]. The layout (and the matching naked-asm
/// offsets) are defined by [`crate::task::TaskContext`]; aliased here so
/// that kernel call sites continue to spell the type as `SwitchContext`.
pub type SwitchContext = crate::task::TaskContext;

// =============================================================================
// fpu_reset_in_place — initialise an FpuState directly at `ptr`
// =============================================================================

/// x87 FPU Control Word offset within both FXSAVE and XSAVE legacy region.
const LEGACY_FCW_OFFSET: usize = 0;
/// MXCSR offset within both FXSAVE and XSAVE legacy region.
const LEGACY_MXCSR_OFFSET: usize = 24;

/// Initialise an [`FpuState`] directly at `ptr` without materialising the
/// 2.6 KiB rvalue on the caller's stack.  Equivalent to writing the result
/// of [`FpuState::new`] but with no temp.
///
/// # Safety
/// `ptr` must be a valid, properly-aligned, writable pointer to an
/// `FpuState`-sized region (≥ `FPU_STATE_SIZE` bytes, 64-byte aligned).
/// The caller must ensure no other reference to that region is live for
/// the duration of this call.
pub unsafe fn fpu_reset_in_place(ptr: *mut FpuState) {
    // SAFETY: ptr is a valid FpuState by caller contract.
    unsafe {
        let bytes = ptr as *mut u8;
        core::ptr::write_bytes(bytes, 0u8, FPU_STATE_SIZE);
        // Legacy FCW = 0x037F, MXCSR = 0x1F80.
        *bytes.add(LEGACY_FCW_OFFSET) = 0x7F;
        *bytes.add(LEGACY_FCW_OFFSET + 1) = 0x03;
        *bytes.add(LEGACY_MXCSR_OFFSET) = 0x80;
        *bytes.add(LEGACY_MXCSR_OFFSET + 1) = 0x1F;
    }
}

// =============================================================================
// SignalAction — kernel-internal per-signal disposition
// =============================================================================

/// Kernel-internal signal action. Mirrors the relevant fields of UserSigaction
/// but stored per-task for fast dispatch.
#[derive(Copy, Clone)]
pub struct SignalAction {
    /// Handler address: SIG_DFL (0), SIG_IGN (1), or a user function pointer.
    pub handler: u64,
    /// Signal mask to OR into blocked set while handler runs.
    pub mask: SigSet,
    /// SA_* flags (SA_RESTORER, SA_NODEFER, SA_RESETHAND, etc.)
    pub flags: u64,
    /// Restorer function pointer (set via SA_RESTORER).
    pub restorer: u64,
}

impl SignalAction {
    pub const fn default() -> Self {
        Self {
            handler: SIG_DFL,
            mask: SIG_EMPTY,
            flags: 0,
            restorer: 0,
        }
    }
}

// =============================================================================
// TaskInner — the generic kernel task control block
// =============================================================================
//
// Layout sanity checks (`offset_of!(Task, fpu_state) - offset_of!(Task,
// context)`, `size_of::<Task>() <= 8192`, `offset_of!(Task, abi) == 0`)
// live in the kernel-side shim where the concrete `Task = TaskInner<
// KernelStack, UnsafeStack>` alias is in scope so `offset_of!` resolves.

/// Generic kernel task control block.
///
/// Parameterised over:
/// - `K`: kernel-mode stack handle (kernel side passes
///   `slopos_core::scheduler::task_stack::KernelStack`).
/// - `U`: SafeStack data ("unsafe") stack handle (kernel side passes
///   `slopos_core::scheduler::task_stack::UnsafeStack`).
///
/// The `test_reports` ring is concrete because `TestReportRing` now
/// lives in OSTD alongside the rest of the task plumbing.
#[repr(C)]
pub struct TaskInner<K, U> {
    /// OSTD-owned ABI sub-struct holding every field that naked asm
    /// reads via a compile-time `const` offset operand. Must remain
    /// at offset 0; enforced by the `offset_of!(Task, abi) == 0`
    /// razor in the kernel-side shim.
    pub abi: TaskAbi,
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    /// Fused (status, reason, epoch) atomic word. Replaces the
    /// pre-Phase-5 `state_atomic: AtomicU8` + `block_reason: AtomicU8`
    /// pair, which exposed an observation window in which a stale
    /// reason could outlive its status (or vice versa). See
    /// `crate::task::state` for the bit layout.
    state: TaskState,
    pub priority: TaskPriority,
    pub flags: u16,
    pub process_id: u32,
    pub stack_base: u64,
    pub stack_size: u64,
    pub stack_pointer: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,
    pub entry_point: u64,
    pub entry_arg: *mut c_void,
    pub context: TaskContext,
    pub fpu_state: FpuState,
    // --- Fields below are NOT accessed by assembly and can be freely reordered ---
    /// Owning handle to the kernel-mode stack.
    ///
    /// `Some` for every live task; `None` only on `invalid()` slots and
    /// freed tasks (after `free_task_stacks`).  Dropping the kernel-stack
    /// handle unmaps the stack pages, returns the physical frames to the
    /// page allocator, and releases the VA slot — so freeing a task is
    /// just `task.kernel_stack = None`.
    pub kernel_stack: Option<K>,
    /// Owning handle to the SafeStack-sanitizer unsafe (data) stack.
    pub unsafe_stack: Option<U>,
    /// Per-task ring of `SYSCALL_TEST_REPORT` payloads.
    ///
    /// `None` for non-test tasks (the syscall is never invoked). The first
    /// `SYSCALL_TEST_REPORT` from a task lazily allocates a fresh ring; the
    /// kernel-side userland-test runner takes ownership once the task has
    /// exited. The handle is contiguous with `kernel_stack`/`unsafe_stack`
    /// so `reset_in_place`'s zero-byte hole covers all three Option<KBox>-
    /// style owned handles in one span.
    pub test_reports: Option<KBox<TestReportRing>>,
    /// Index of this Task's slot in the `TASK_MANAGER` pool spine.
    pub slot_index: u32,
    pub parent_task_id: u32,
    /// FS segment base address (TLS pointer). Written to MSR FS_BASE before
    /// switching to user mode, and read back on context save.
    pub fs_base: u64,
    /// Thread-group ID.
    pub tgid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub controlling_tty: Option<TtyIndex>,
    /// Current working directory path (null-terminated, max 256 bytes).
    pub cwd: [u8; 256],
    pub cwd_len: u16,
    /// User-space address to clear (and futex-wake) on thread exit.
    pub clear_child_tid: u64,
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
    pub cpu_affinity: u32,
    pub last_cpu: u8,
    pub migration_count: u32,
    // --- Signal state ---
    /// Bitmask of pending signals (written atomically by kill()).
    pub signal_pending: AtomicU64,
    /// Bitmask of blocked signals (modified by rt_sigprocmask).
    pub signal_blocked: SigSet,
    /// Per-signal action table.
    pub signal_actions: [SignalAction; NSIG],
    pub switch_ctx: SwitchContext,
    /// Set while a CPU is physically executing this task.
    pub on_cpu: AtomicBool,
    /// Intrusive link slot for the per-CPU `ReadyQueue`.
    pub ready_link: Link<TaskInner<K, U>, ReadyQueueRole>,
    /// Intrusive link slot for the global `ZombieList`.
    pub zombie_link: Link<TaskInner<K, U>, ZombieListRole>,
    pub next_inbox: AtomicPtr<TaskInner<K, U>>,
    pub refcnt: AtomicU32,
    /// Per-task user-mode register snapshot.
    pub user_ctx: UserContext,
    /// Saved per-task value of `pcr.user_ctx_ptr`.
    pub saved_user_ctx_ptr: *mut UserContext,
    /// Saved per-task copy of `pcr.kernel_return_ctx`.
    pub saved_kernel_return_ctx: KernelReturnContext,
    /// Tasks waiting for THIS task to exit.
    pub waiters: WaitQueue,
    /// Durable per-task exit value.
    pub exit_info: AtomicCell<ExitInfo>,
}

impl<K, U> TaskInner<K, U> {
    pub const fn invalid() -> Self {
        Self {
            abi: TaskAbi { unsafe_stack_sp: 0 },
            task_id: INVALID_TASK_ID,
            name: [0; TASK_NAME_MAX_LEN],
            state: TaskState::invalid(),
            priority: TaskPriority::Normal,
            flags: 0,
            process_id: INVALID_PROCESS_ID,
            stack_base: 0,
            stack_size: 0,
            stack_pointer: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            kernel_stack_size: 0,
            entry_point: 0,
            entry_arg: ptr::null_mut(),
            context: TaskContext::zero(),
            fpu_state: FpuState::new(),
            kernel_stack: None,
            unsafe_stack: None,
            test_reports: None,
            slot_index: u32::MAX,
            parent_task_id: INVALID_TASK_ID,
            fs_base: 0,
            tgid: INVALID_TASK_ID,
            pgid: INVALID_TASK_ID,
            sid: INVALID_TASK_ID,
            controlling_tty: None,
            cwd: {
                let mut c = [0u8; 256];
                c[0] = b'/';
                c
            },
            cwd_len: 1,
            clear_child_tid: 0,
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: 0,
            creation_time: 0,
            yield_count: 0,
            last_run_timestamp: 0,
            user_started: 0,
            context_from_user: 0,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
            fate_token: 0,
            fate_value: 0,
            fate_pending: 0,
            cpu_affinity: 0,
            last_cpu: 0,
            migration_count: 0,
            signal_pending: AtomicU64::new(0),
            signal_blocked: SIG_EMPTY,
            signal_actions: [SignalAction::default(); NSIG],
            switch_ctx: SwitchContext::zero(),
            on_cpu: AtomicBool::new(false),
            ready_link: Link::new(),
            zombie_link: Link::new(),
            next_inbox: AtomicPtr::new(ptr::null_mut()),
            refcnt: AtomicU32::new(0),
            user_ctx: UserContext::const_zeroed(),
            saved_user_ctx_ptr: ptr::null_mut(),
            saved_kernel_return_ctx: KernelReturnContext {
                rbx: 0,
                rbp: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                rsp: 0,
                rip: 0,
            },
            waiters: WaitQueue::new(),
            exit_info: AtomicCell::empty(),
        }
    }

    /// In-place Init recipe for a fresh `Invalid` Task, equivalent in
    /// observable state to [`TaskInner::invalid`] but constructed
    /// field-by-field at the destination slot — no 3.8 KiB rvalue on
    /// the caller's stack.
    pub fn init_invalid() -> impl Init<Self, AllocError> {
        // SAFETY: the closure writes every byte of `slot` — first via
        // `write_bytes` to zero the struct, then targeted writes for the
        // fields whose valid `Invalid` value is not all-zero.
        unsafe {
            init_from_closure(|slot: *mut Self| -> Result<(), AllocError> {
                core::ptr::write_bytes(slot as *mut u8, 0, core::mem::size_of::<Self>());

                addr_of_mut!((*slot).task_id).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).priority).write(TaskPriority::Normal);
                addr_of_mut!((*slot).process_id).write(INVALID_PROCESS_ID);
                addr_of_mut!((*slot).entry_arg).write(ptr::null_mut());
                addr_of_mut!((*slot).parent_task_id).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).tgid).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).pgid).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).sid).write(INVALID_TASK_ID);

                addr_of_mut!((*slot).controlling_tty).write(None);

                addr_of_mut!((*slot).cwd_len).write(1);
                (addr_of_mut!((*slot).cwd) as *mut u8).write(b'/');

                fpu_reset_in_place(addr_of_mut!((*slot).fpu_state));

                addr_of_mut!((*slot).kernel_stack).write(None);
                addr_of_mut!((*slot).unsafe_stack).write(None);
                addr_of_mut!((*slot).test_reports).write(None);
                addr_of_mut!((*slot).abi.unsafe_stack_sp).write(0);

                addr_of_mut!((*slot).slot_index).write(u32::MAX);

                addr_of_mut!((*slot).signal_blocked).write(SIG_EMPTY);
                for i in 0..NSIG {
                    let p = (addr_of_mut!((*slot).signal_actions) as *mut SignalAction).add(i);
                    p.write(SignalAction::default());
                }

                addr_of_mut!((*slot).switch_ctx).write(SwitchContext::zero());

                addr_of_mut!((*slot).waiters).write(WaitQueue::new());
                addr_of_mut!((*slot).exit_info).write(AtomicCell::empty());

                Ok(())
            })
        }
    }

    /// Reset a Task slot in place to the `invalid` state.
    ///
    /// # Safety
    /// - `this` must be non-null, aligned, and point to a writable
    ///   `TaskInner<K, U>` slot that the caller has exclusive access to.
    /// - The slot must currently hold a valid `TaskInner<K, U>`.
    pub unsafe fn reset_in_place(this: *mut Self) {
        unsafe {
            let preserved_slot_index = (*this).slot_index;
            let _ = (*this).kernel_stack.take();
            let _ = (*this).unsafe_stack.take();
            let _ = (*this).test_reports.take();
            (*this).exit_info.reset();
            let bytes = core::mem::size_of::<Self>();
            let kernel_stack_off = core::mem::offset_of!(Self, kernel_stack);
            let test_reports_off = core::mem::offset_of!(Self, test_reports);
            let test_reports_size = core::mem::size_of::<Option<KBox<TestReportRing>>>();
            debug_assert!(
                kernel_stack_off < test_reports_off,
                "TaskInner: kernel_stack must precede test_reports for reset_in_place hole span"
            );
            let tail_start = test_reports_off + test_reports_size;
            let base = this as *mut u8;
            core::ptr::write_bytes(base, 0, kernel_stack_off);
            core::ptr::write_bytes(base.add(tail_start), 0, bytes - tail_start);
            (*this).task_id = INVALID_TASK_ID;
            (*this).priority = TaskPriority::Normal;
            (*this).process_id = INVALID_PROCESS_ID;
            (*this).slot_index = preserved_slot_index;
            (*this).parent_task_id = INVALID_TASK_ID;
            (*this).tgid = INVALID_TASK_ID;
            (*this).pgid = INVALID_TASK_ID;
            (*this).sid = INVALID_TASK_ID;
            (*this).cwd[0] = b'/';
            (*this).cwd_len = 1;
            addr_of_mut!((*this).waiters).write(WaitQueue::new());
            addr_of_mut!((*this).exit_info).write(AtomicCell::empty());
            addr_of_mut!((*this).state).write(TaskState::invalid());
        }
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        self.state.status()
    }

    /// Force-publish a new status without changing the block reason.
    #[inline]
    pub fn set_status(&self, status: TaskStatus) {
        let reason = self.state.reason();
        self.state.force_set(status, reason);
    }

    /// Force-publish (status, reason) atomically. Single-owner only.
    #[inline]
    pub fn force_set_state(&self, status: TaskStatus, reason: BlockReason) {
        self.state.force_set(status, reason);
    }

    #[inline]
    pub fn try_transition_to(&self, target: TaskStatus) -> bool {
        let current = self.state.status();
        if !current.can_transition_to(target) {
            return false;
        }
        self.state
            .try_transition_keep_reason(current, target)
            .is_ok()
    }

    /// Atomically transition from `expected` to `target`.
    #[inline]
    pub fn try_transition_from(&self, expected: TaskStatus, target: TaskStatus) -> bool {
        if !expected.can_transition_to(target) {
            return false;
        }
        self.state
            .try_transition_keep_reason(expected, target)
            .is_ok()
    }

    /// Block from a specific expected state, stamping the block reason
    /// in the same CAS.
    #[inline]
    pub fn block_from(&self, expected: TaskStatus, reason: BlockReason) -> bool {
        if !expected.can_transition_to(TaskStatus::Blocked) {
            return false;
        }
        self.state
            .try_transition(expected, TaskStatus::Blocked, reason)
            .is_ok()
    }

    #[inline]
    pub fn mark_ready(&self) -> bool {
        self.try_transition_to(TaskStatus::Ready)
    }

    #[inline]
    pub fn mark_running(&self) -> bool {
        self.try_transition_to(TaskStatus::Running)
    }

    #[inline]
    pub fn block(&self, reason: BlockReason) -> bool {
        let current = self.state.status();
        if !current.can_transition_to(TaskStatus::Blocked) {
            return false;
        }
        self.state
            .try_transition(current, TaskStatus::Blocked, reason)
            .is_ok()
    }

    /// Load the block reason. Only meaningful when `status() == Blocked`.
    #[inline]
    pub fn load_block_reason(&self) -> BlockReason {
        self.state.reason()
    }

    /// Store the block reason directly.
    #[inline]
    pub fn store_block_reason(&self, reason: BlockReason) {
        self.state.store_reason(reason);
    }

    #[inline]
    pub fn terminate(&self) -> bool {
        self.try_transition_to(TaskStatus::Terminated)
    }

    /// Transition to `Zombie`.
    #[inline]
    pub fn mark_zombie(&self) -> bool {
        self.try_transition_to(TaskStatus::Zombie)
    }

    #[inline]
    pub fn is_blocked(&self) -> bool {
        self.status() == TaskStatus::Blocked
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        self.status() == TaskStatus::Ready
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        self.status() == TaskStatus::Running
    }

    #[inline]
    pub fn is_terminated(&self) -> bool {
        self.status() == TaskStatus::Terminated
    }

    #[inline]
    pub fn is_zombie(&self) -> bool {
        self.status() == TaskStatus::Zombie
    }

    /// True if the task has exited (Zombie or Terminated).
    #[inline]
    pub fn is_exited(&self) -> bool {
        matches!(self.status(), TaskStatus::Zombie | TaskStatus::Terminated)
    }

    /// Bulk-copy task state using `ptr::copy_nonoverlapping`, then reset
    /// linkage, refcount, and owned resources.
    ///
    /// # Safety
    /// Caller must ensure `self` and `other` do not overlap and that
    /// `self` is not concurrently accessed by another CPU. Owned handle
    /// fields are bitwise-duplicated then overwritten with neutral
    /// values via `ptr::write` so their `Drop` does not free the
    /// parent's resources.
    pub unsafe fn clone_from_raw(&mut self, other: &Self) {
        let preserved_slot_index = self.slot_index;
        // SAFETY: Both pointers are valid, non-overlapping TaskInner
        // instances. The caller guarantees exclusive write access to
        // `self`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                other as *const Self as *const u8,
                self as *mut Self as *mut u8,
                core::mem::size_of::<Self>(),
            );
            core::ptr::write(&mut self.kernel_stack as *mut _, None);
            core::ptr::write(&mut self.unsafe_stack as *mut _, None);
            core::ptr::write(&mut self.test_reports as *mut _, None);
            self.abi.unsafe_stack_sp = 0;
            core::ptr::write(&mut self.waiters as *mut _, WaitQueue::new());
            core::ptr::write(&mut self.exit_info as *mut _, AtomicCell::empty());
            core::ptr::write(&mut self.state as *mut _, TaskState::invalid());
        }
        self.slot_index = preserved_slot_index;
        self.ready_link.reset();
        self.zombie_link.reset();
        self.next_inbox = AtomicPtr::new(ptr::null_mut());
        self.refcnt = AtomicU32::new(0);
        self.signal_pending = AtomicU64::new(0);
    }

    #[inline]
    pub fn inc_ref(&self) -> u32 {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == u32::MAX {
            return u32::MAX;
        }
        self.refcnt.fetch_add(1, Ordering::AcqRel) + 1
    }

    #[inline]
    pub fn dec_ref(&self) -> bool {
        let prev = self.refcnt.load(Ordering::Acquire);
        if prev == 0 {
            return false;
        }
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }

    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
}

// Per-role `Linked<Role> for TaskInner` impls are absorbed via OSTD's
// blanket `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T` — only
// the two safe `LinkProvider` impls live here, returning distinct
// fields per role.

impl<K, U> crate::task::LinkProvider<ReadyQueueRole> for TaskInner<K, U> {
    fn link(&self) -> &Link<Self, ReadyQueueRole> {
        &self.ready_link
    }
}

impl<K, U> crate::task::LinkProvider<ZombieListRole> for TaskInner<K, U> {
    fn link(&self) -> &Link<Self, ZombieListRole> {
        &self.zombie_link
    }
}

// `TaskOps` plug for the OSTD-side typestate handles.
impl<K, U> crate::task::TaskOps for TaskInner<K, U> {
    #[inline]
    fn handle_mark_ready(&self) {
        self.set_status(slopos_abi::task::TaskStatus::Ready);
    }
    #[inline]
    fn handle_mark_terminated(&self) {
        self.set_status(slopos_abi::task::TaskStatus::Terminated);
    }
    #[inline]
    fn handle_mark_blocked(&self) {
        self.set_status(slopos_abi::task::TaskStatus::Blocked);
    }
    #[inline]
    fn handle_inc_ref(&self) {
        TaskInner::inc_ref(self);
    }
    #[inline]
    fn handle_dec_ref(&self) -> bool {
        TaskInner::dec_ref(self)
    }
    #[inline]
    fn handle_ref_count(&self) -> u32 {
        TaskInner::ref_count(self)
    }
    #[inline]
    fn handle_status_is_ready(&self) -> bool {
        self.status() == slopos_abi::task::TaskStatus::Ready
    }
    #[inline]
    fn handle_try_cas_running(&self) -> bool {
        self.try_transition_from(
            slopos_abi::task::TaskStatus::Ready,
            slopos_abi::task::TaskStatus::Running,
        )
    }
}
