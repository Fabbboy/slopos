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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

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
use crate::sync::intrusive::Link;
use crate::sync::intrusive_dlist::{DLink, IntrusiveDList};
use crate::task::abi::TaskAbi;
use crate::task::exit_info::ExitInfo;
use crate::task::fpu::{FPU_STATE_SIZE, FpuState};
use crate::task::job_control::ProcessGroup;
use crate::task::link_roles::{ReadyQueueRole, ReclaimRole, RemoteWakeRole, SiblingRole};
use crate::task::state::TaskState;
use crate::task::test_reports::TestReportRing;
use crate::user::context::UserContext;
use crate::{AllocError, Init, KArc, KBox, init_from_closure};

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
// Scheduler placement — runnable ownership separate from TaskStatus
// =============================================================================

/// Scheduler-owned placement for a runnable or physically running task.
///
/// `TaskStatus::Ready` is not enough to prove schedulability on SMP: a task
/// also needs exactly one scheduler owner. This byte is SlopOS's explicit
/// equivalent of Linux's `on_rq`/`on_cpu` pair, FreeBSD's `TD_ON_RUNQ`, and
/// seL4's queued-bit discipline.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedPlacement {
    /// Not owned by any scheduler structure. Valid for blocked/exited tasks
    /// and for a not-yet-published freshly-created task.
    None = 0,
    /// Linked in exactly one per-CPU ready queue via `ready_link`.
    ReadyQueue = 1,
    /// Pending in exactly one per-CPU remote wake inbox via
    /// `remote_inbox_link`.
    RemoteWake = 2,
    /// Owned by a CPU as the current/next task, including the dispatch and
    /// switch-out windows while `TaskStatus` and `on_cpu` are being updated.
    OnCpu = 3,
    /// Temporarily owned by the load balancer while moving between runqueues.
    Migrating = 4,
    /// A wake/new-task publisher has reserved scheduler ownership but has not
    /// linked the task into its final queue/inbox yet. This closes the
    /// Ready-with-no-owner publication window without blocking producers.
    Waking = 5,
}

impl SchedPlacement {
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::ReadyQueue,
            2 => Self::RemoteWake,
            3 => Self::OnCpu,
            4 => Self::Migrating,
            5 => Self::Waking,
            _ => Self::None,
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
///   `slopos_sched::task_stack::KernelStack`).
/// - `U`: SafeStack data ("unsafe") stack handle (kernel side passes
///   `slopos_sched::task_stack::UnsafeStack`).
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
    /// Strong membership in this task's process group. The group (and, via
    /// it, the session) lives while any member holds this handle; dropping the
    /// task releases the membership. `None` for kernel-mode tasks.
    pub process_group: Option<KArc<ProcessGroup>>,
    /// Per-task ring of `SYSCALL_TEST_REPORT` payloads.
    ///
    /// `None` for non-test tasks (the syscall is never invoked). The first
    /// `SYSCALL_TEST_REPORT` from a task lazily allocates a fresh ring; the
    /// kernel-side userland-test runner takes ownership once the task exits.
    pub test_reports: Option<KBox<TestReportRing>>,
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
    /// Intrusive link slot for per-CPU remote wake inboxes.
    ///
    /// Although the inbox itself is a lock-free Treiber stack, not an
    /// `IntrusiveLinkedList`, it has the same single-membership rule as a
    /// runqueue: a one-element stack has a null successor while still being a
    /// member. Using a role-typed `Link` folds the successor and membership bit
    /// into one OSTD primitive, so duplicate remote wakes cannot self-cycle the
    /// inbox and diagnostics can distinguish pending wake delivery from a
    /// genuinely stranded Ready task.
    pub remote_inbox_link: Link<TaskInner<K, U>, RemoteWakeRole>,
    /// Owning list of this task's live and zombie children.
    ///
    /// Each entry is one child task whose membership is backed by a strong
    /// reference parked exactly like ready-queue placement: linking a child
    /// pairs with `task_placement_retain`, unlinking with
    /// `task_placement_reclaim`. A zombie child stays here — pinned by that
    /// parked reference — until `waitpid` reaps it or this task's own teardown
    /// drains the list. There is no separate zombie list; a zombie is just a
    /// child whose status is `Zombie`.
    pub children: IntrusiveDList<TaskInner<K, U>, SiblingRole>,
    /// Intrusive link slot naming this task's membership in the one owner list
    /// holding it. A task is linked into at most one such list; the
    /// single-membership invariant of the role-typed slot rejects a
    /// double-link, and the slot's owner back-pointer lets the task be
    /// unlinked without naming which list that is.
    pub sibling_link: DLink<TaskInner<K, U>, SiblingRole>,
    /// Intrusive link slot for the task graveyard — the lock-free stack of
    /// tasks awaiting destruction in a context where the allocator may run.
    ///
    /// Unlike every other link slot, membership here does *not* imply a parked
    /// strong reference: the pusher won the final release, so the strong count
    /// is already zero and the pusher owns the allocation outright. That is why
    /// it gets its own role.
    pub reclaim_link: Link<TaskInner<K, U>, ReclaimRole>,
    /// Explicit scheduler placement owner for runnable tasks. This is the
    /// cross-role gate that prevents a task from being in a ready queue and a
    /// remote wake inbox at the same time.
    pub sched_placement: AtomicU8,
    /// Panic-recovery nesting depth saved while this task is not running; the
    /// live value lives in `PCR.recovery_depth` (read directly by the panic
    /// handler), and context-switch code saves/restores it here so recovery
    /// scopes survive migration.
    pub recovery_depth: AtomicU32,
    /// Panic in-flight depth saved while this task is not running; the live
    /// value lives in `PCR.panic_in_flight`. An unwinding task runs
    /// interrupts-on and can be preempted or migrate mid-unwind, so the
    /// depth must travel with the task like `recovery_depth`.
    pub panic_in_flight: AtomicU32,
    /// Idempotence bits for task/process teardown that may be split between
    /// `task_terminate` and post-switch cleanup of the current task.
    pub exit_cleanup_flags: AtomicU8,
    /// Per-task user-mode register snapshot.
    pub user_ctx: UserContext,
    /// Saved per-task value of `pcr.user_ctx_ptr`.
    pub saved_user_ctx_ptr: *mut UserContext,
    /// Saved per-task copy of `pcr.kernel_return_ctx`.
    pub saved_kernel_return_ctx: KernelReturnContext,
    /// Durable per-task exit value.
    pub exit_info: AtomicCell<ExitInfo>,
}

impl<K, U> Drop for TaskInner<K, U> {
    fn drop(&mut self) {
        crate::task::drop_context::assert_task_drop_context();
        self.assert_family_links_detached();
    }
}

impl<K, U> TaskInner<K, U> {
    /// Debug tripwire that a task's family links are detached before reclaim.
    ///
    /// `IntrusiveLinkedList` has no `Drop`, so a non-empty `children` list at
    /// reclaim would silently leak every parked child reference; a still-linked
    /// `sibling_link` would mean a parent's list still names this task. Teardown
    /// drains the children list and reap/reparent unlinks the sibling slot before
    /// a task can reach a reclaimable state, so both hold here. Factored out of
    /// `Drop` (as [`assert_task_drop_context`] is) so the destructor body carries
    /// no literal panic op; `debug_assert!` compiles out of release, so the
    /// destructor is panic-free there.
    ///
    /// [`assert_task_drop_context`]: crate::task::drop_context::assert_task_drop_context
    #[inline]
    fn assert_family_links_detached(&self) {
        debug_assert!(
            self.children.is_empty(),
            "task dropped with a non-empty children list"
        );
        debug_assert!(
            !self.sibling_link.is_linked(),
            "task dropped while still linked into an owner list"
        );
        // A still-linked scheduler slot means the container's parked reference
        // leaked: the count could not have reached zero while that reference
        // existed, so reaching the destructor here means a retain/reclaim pair
        // went unbalanced. Previously undetectable.
        debug_assert!(
            !self.ready_link.is_linked(),
            "task dropped while still linked into a ready queue"
        );
        debug_assert!(
            !self.remote_inbox_link.is_linked(),
            "task dropped while still linked into a remote wake inbox"
        );
        // The graveyard pops a node before destroying it; still being linked
        // means the destructor is running on a node another drain still names.
        debug_assert!(
            !self.reclaim_link.is_linked(),
            "task dropped while still parked in the reclaim queue"
        );
    }
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
            process_group: None,
            test_reports: None,
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
            remote_inbox_link: Link::new(),
            children: IntrusiveDList::new(),
            sibling_link: DLink::new(),
            reclaim_link: Link::new(),
            sched_placement: AtomicU8::new(SchedPlacement::None.as_u8()),
            recovery_depth: AtomicU32::new(0),
            panic_in_flight: AtomicU32::new(0),
            exit_cleanup_flags: AtomicU8::new(0),
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
                addr_of_mut!((*slot).process_group).write(None);
                addr_of_mut!((*slot).test_reports).write(None);
                addr_of_mut!((*slot).abi.unsafe_stack_sp).write(0);

                addr_of_mut!((*slot).signal_blocked).write(SIG_EMPTY);
                for i in 0..NSIG {
                    let p = (addr_of_mut!((*slot).signal_actions) as *mut SignalAction).add(i);
                    p.write(SignalAction::default());
                }

                addr_of_mut!((*slot).switch_ctx).write(SwitchContext::zero());

                addr_of_mut!((*slot).exit_info).write(AtomicCell::empty());

                Ok(())
            })
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

    /// The fused state word's ABA epoch (bumped on every state transition).
    /// Diagnostics: a task whose epoch is frozen across observation windows
    /// has genuinely stopped transitioning, distinguishing a stranded task
    /// from one merely caught mid-park by a racing scan.
    #[inline]
    pub fn state_epoch(&self) -> u32 {
        self.state.snapshot().epoch
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

    /// Reset the per-run runtime bookkeeping on a newly allocated task:
    /// clears timing, exit/fault disposition, fate tokens, scheduler
    /// placement, the intrusive scheduler links, and the refcount, and stamps
    /// a fresh creation timestamp.
    ///
    /// Drives both the task-create path and the fork path (after the child is
    /// bulk-copied from its parent), so every task starts neutral. The
    /// owning crate holds exclusive `&mut self` access at every call site.
    pub fn reset_runtime_state(&mut self) {
        self.time_slice_remaining = self.time_slice;
        self.total_runtime = 0;
        self.creation_time = crate::kdiag_timestamp();
        self.yield_count = 0;
        self.last_run_timestamp = 0;
        self.exit_reason = TaskExitReason::None;
        self.fault_reason = TaskFaultReason::None;
        self.exit_code = 0;
        self.fate_token = 0;
        self.fate_value = 0;
        self.fate_pending = 0;
        self.on_cpu.store(false, Ordering::Release);
        self.ready_link.reset();
        self.remote_inbox_link.reset();
        self.sibling_link.reset();
        self.reclaim_link.reset();
        self.sched_placement
            .store(SchedPlacement::None.as_u8(), Ordering::Release);
        self.recovery_depth.store(0, Ordering::Release);
        self.exit_cleanup_flags.store(0, Ordering::Release);
    }

    /// Bulk-copy task state using `ptr::copy_nonoverlapping`, then reset
    /// linkage, placement, and owned resources.
    ///
    /// # Safety
    /// Caller must ensure `self` and `other` do not overlap and that
    /// `self` is not concurrently accessed by another CPU. Owned handle
    /// fields are bitwise-duplicated then overwritten with neutral
    /// values via `ptr::write` so their `Drop` does not free the
    /// parent's resources.
    pub unsafe fn clone_from_raw(&mut self, other: &Self) {
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
            core::ptr::write(&mut self.process_group as *mut _, None);
            core::ptr::write(&mut self.test_reports as *mut _, None);
            self.abi.unsafe_stack_sp = 0;
            core::ptr::write(&mut self.exit_info as *mut _, AtomicCell::empty());
            core::ptr::write(&mut self.state as *mut _, TaskState::invalid());
            // The bytewise copy duplicated the parent's `children` head/tail
            // and its owner-list membership state. Overwrite the list head with
            // an empty one (no `Drop` on `IntrusiveDList`, so `ptr::write` over
            // the raw copy is correct — it must not run a destructor on the
            // copied bits) and detach the owner slot: a fresh child owns no
            // children and is linked into no owner list until registration
            // publishes it. Leaving the copied `owner` back-pointer in place
            // would make the child claim membership in its parent's list, and
            // an unlink would then corrupt that list.
            core::ptr::write(&mut self.children as *mut _, IntrusiveDList::new());
        }
        self.ready_link.reset();
        self.remote_inbox_link.reset();
        self.sibling_link.reset();
        self.reclaim_link.reset();
        // The bytewise copy carried the parent's own parent id. A fresh child
        // starts parentless; the spawn path publishes the real parent edge (id
        // + children-list membership) via `link_child` after registration.
        self.parent_task_id = INVALID_TASK_ID;
        self.sched_placement = AtomicU8::new(SchedPlacement::None.as_u8());
        self.recovery_depth = AtomicU32::new(0);
        self.exit_cleanup_flags = AtomicU8::new(0);
        self.signal_pending = AtomicU64::new(0);
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

impl<K, U> crate::task::LinkProvider<RemoteWakeRole> for TaskInner<K, U> {
    fn link(&self) -> &Link<Self, RemoteWakeRole> {
        &self.remote_inbox_link
    }
}

impl<K, U> crate::task::DLinkProvider<SiblingRole> for TaskInner<K, U> {
    fn dlink(&self) -> &DLink<Self, SiblingRole> {
        &self.sibling_link
    }
}

impl<K, U> crate::task::LinkProvider<ReclaimRole> for TaskInner<K, U> {
    fn link(&self) -> &Link<Self, ReclaimRole> {
        &self.reclaim_link
    }
}
