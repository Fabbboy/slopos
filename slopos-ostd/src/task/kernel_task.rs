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
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering,
};

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
use crate::sync::intrusive::Link;
use crate::sync::intrusive_dlist::{DLink, IntrusiveDList};
use crate::sync::{AtomicCell, LOCK_LEVEL_RESOURCE, RcuArcSlot, SpinLock};
use crate::task::abi::TaskAbi;
use crate::task::cell::{TaskExclusive, TaskOwnCell};
use crate::task::exit_info::ExitInfo;
use crate::task::fpu::{FPU_STATE_SIZE, FpuState};
use crate::task::fpu_owner::{
    FPU_CPU_NONE, fpu_owner_assert_may_take, fpu_owner_take, fpu_owner_yield_after_save,
};
use crate::task::job_control::ProcessGroup;
use crate::task::link_roles::{ReadyQueueRole, ReclaimRole, RemoteWakeRole, SiblingRole};
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

/// Capacity of a task's working-directory buffer, NUL terminator included.
pub const CWD_MAX: usize = 256;

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

/// One task's disposition for one signal, stored as four atomics.
///
/// Atomic because `task_signal_post` reads `handler` from whichever CPU is
/// sending — every `kill`, every process-group and session fanout — while the
/// owner rewrites the whole entry in `rt_sigaction`, on exec, and through the
/// spawn `sigdefault` mask. A plain struct made that a data race the pointer
/// accessors happened to hide behind a `*mut` reborrow.
///
/// Four independent atomics rather than a lock: the fields are never read as a
/// group from another CPU (senders want only `handler`), and the owner reads
/// them in its own program order, so there is no group to keep consistent. The
/// layout matches the plain struct, so `[SignalActionCell; NSIG]` leaves
/// `Task`'s size unchanged.
#[repr(C)]
pub struct SignalActionCell {
    handler: AtomicU64,
    mask: AtomicU64,
    flags: AtomicU64,
    restorer: AtomicU64,
}

const _: () = assert!(
    core::mem::size_of::<SignalActionCell>() == core::mem::size_of::<SignalAction>(),
    "SignalActionCell must not change Task's layout"
);

impl SignalActionCell {
    pub const fn default() -> Self {
        Self {
            handler: AtomicU64::new(SIG_DFL),
            mask: AtomicU64::new(SIG_EMPTY),
            flags: AtomicU64::new(0),
            restorer: AtomicU64::new(0),
        }
    }

    /// The handler address alone — the one field a remote CPU reads.
    #[inline]
    pub fn handler(&self) -> u64 {
        self.handler.load(Ordering::Acquire)
    }

    /// Read the whole disposition.
    ///
    /// Not atomic as a group, and named for the only caller that may rely on
    /// it: the owning task, which is the sole writer and so cannot observe a
    /// half-written entry. A remote CPU must use [`handler`](Self::handler) —
    /// reading the group from another CPU can pair an old handler with a new
    /// mask.
    ///
    /// `handler` is loaded **first**, and must stay first: its Acquire pairs
    /// with the Release in [`store`](Self::store), which writes it last, so
    /// observing a handler guarantees the three fields written before it.
    #[inline]
    pub fn load_owner_only(&self) -> SignalAction {
        SignalAction {
            handler: self.handler.load(Ordering::Acquire),
            mask: self.mask.load(Ordering::Acquire),
            flags: self.flags.load(Ordering::Acquire),
            restorer: self.restorer.load(Ordering::Acquire),
        }
    }

    /// Publish a whole disposition. `handler` is written last so a remote
    /// reader that observes a new handler also observes the mask, flags and
    /// restorer that belong with it.
    #[inline]
    pub fn store(&self, action: SignalAction) {
        self.mask.store(action.mask, Ordering::Release);
        self.flags.store(action.flags, Ordering::Release);
        self.restorer.store(action.restorer, Ordering::Release);
        self.handler.store(action.handler, Ordering::Release);
    }

    /// Reset to the default disposition.
    #[inline]
    pub fn reset(&self) {
        self.store(SignalAction::default());
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
    /// Not owned by any scheduler structure. Valid for blocked and exited
    /// tasks.
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
    /// Allocated, possibly registered, never published. No publisher has ever
    /// made this task schedulable.
    ///
    /// Distinct from `None` because `None` is also the placement of a blocked
    /// task and of a terminated one — and that coincidence was exploitable.
    /// A task is registry-visible from `register_task`, with status `Blocked`
    /// and placement `None`, until its creator calls `publish_new_task`; those
    /// two facts together are indistinguishable from a legitimate wake target,
    /// so a process-group signal arriving in that window drove a half-built
    /// task onto a runqueue. `task_create` publishes `pgid = task_id` before it
    /// registers, which is exactly how such a signal finds one.
    ///
    /// Entered once, at allocation. Left by exactly four paths: the two
    /// publication entry points (`publish_new_task` and the `schedule_task` /
    /// `schedule_new_task` reservation, both → `Waking`, both rolling back to
    /// `Nascent` if the publication fails), the per-CPU idle installer (which
    /// stores `OnCpu` directly — an idle task is dispatched, never published),
    /// and teardown (→ `None`). Never a durable owner: it holds no reference.
    Nascent = 6,
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
            6 => Self::Nascent,
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
    pub context: TaskOwnCell<TaskContext>,
    pub fpu_state: TaskOwnCell<FpuState>,
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
    /// task releases the membership. Empty for kernel-mode tasks.
    ///
    /// Published through an RCU slot because the writer is not the owner:
    /// `setpgid(pid, …)` re-homes a *different* task, so the store lands on a
    /// field a reader on another CPU may be cloning from at that instant. The
    /// slot is what keeps the two apart. A reader clones its own handle inside
    /// a read-side section — one acquire load, one increment — and the
    /// displaced reference is released only once a grace period has elapsed, so
    /// the writer can never drive to zero a count a reader is still raising,
    /// and no destructor runs on the writer's stack.
    pub process_group: RcuArcSlot<ProcessGroup>,
    /// Per-task ring of `SYSCALL_TEST_REPORT` payloads.
    ///
    /// `None` for non-test tasks (the syscall is never invoked). The first
    /// `SYSCALL_TEST_REPORT` from a task lazily allocates a fresh ring; the
    /// kernel-side userland-test runner takes ownership once the task exits.
    /// Per-task ring of `SYSCALL_TEST_REPORT` payloads.
    ///
    /// Behind a `SpinLock` rather than an atomic: it owns a `KBox`, which no
    /// atomic can hold. The owner lazily allocates it on its first report while
    /// a *foreign* task drains it after exit (`task_drain_test_reports`), so a
    /// shared borrow has to be enough for both — which is exactly what the lock
    /// buys. `LOCK_LEVEL_RESOURCE`: neither side is on the switch path or runs
    /// with interrupts off, and the allocation happens before the store so the
    /// lock never covers one.
    pub test_reports: SpinLock<Option<KBox<TestReportRing>>>,
    pub parent_task_id: u32,
    /// FS segment base address (TLS pointer). Written to MSR FS_BASE before
    /// switching to user mode, and read back on context save.
    /// FS segment base (TLS pointer).
    ///
    /// Atomic because the owner writes it via `arch_prctl` while
    /// `prepare_switch_to` reads the *incoming* task's copy from whichever CPU
    /// is performing the switch — a cross-task read, which a plain field cannot
    /// express without a data race. Release/Acquire rather than Relaxed: the
    /// value must be visible to the next switch on any CPU.
    pub fs_base: AtomicU64,
    /// Thread-group ID.
    pub tgid: u32,
    pub pgid: u32,
    pub sid: u32,
    /// Controlling terminal of this task's session, encoded per
    /// [`TTY_INDEX_NONE`]. Read and written through
    /// [`controlling_tty`](TaskInner::controlling_tty) and
    /// [`set_controlling_tty`](TaskInner::set_controlling_tty).
    ///
    /// Atomic because the session-hangup path clears it through a *shared*
    /// snapshot of the task table: the leader's exit walks every task in the
    /// session and clears the field on each, concurrently with those tasks
    /// reading it. A plain field made that a data race that the pointer
    /// accessors happened to hide behind a `*mut` reborrow.
    pub controlling_tty: AtomicU16,
    /// Current working directory path (null-terminated, max 256 bytes).
    /// Only the owning task reads or writes it (chdir/getcwd), so the cell's
    /// witness is always a `CurrentTask`.
    pub cwd: TaskOwnCell<[u8; CWD_MAX]>,
    /// Length of `cwd` up to but not including the NUL. Atomic so it can be
    /// published after the bytes.
    pub cwd_len: AtomicU16,
    /// User-space address to clear (and futex-wake) on thread exit.
    ///
    /// Atomic because the exit path reads and clears it, and that path runs on
    /// whichever task called `task_terminate` — not necessarily this one.
    pub clear_child_tid: AtomicU64,
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    /// Accumulated on-CPU time.
    ///
    /// Atomic because it has three writers on different CPUs: the switch-out
    /// tail bumps the *outgoing* task's tally, the exit path adds the final
    /// slice, and the task-list syscall reads it from a registry walk while
    /// both are running. A plain field made that a data race.
    pub total_runtime: AtomicU64,
    pub creation_time: u64,
    /// Voluntary-yield count.
    ///
    /// Atomic for the same reason as `migration_count` below, and Relaxed for
    /// the same reason: a diagnostic tally with no publication relationship.
    pub yield_count: AtomicU32,
    /// Timestamp of this task's last dispatch.
    ///
    /// Atomic because the context-switch path writes it on the owning CPU
    /// while the stranded-task rescue sweep reads it from another. The old
    /// plain field needed a hand-rolled `read_volatile` accessor to paper over
    /// exactly that.
    pub last_run_timestamp: AtomicU64,
    pub user_started: u8,
    pub context_from_user: u8,
    /// Why the task exited, as [`TaskExitReason::as_u16`].
    ///
    /// This trio is atomic because `stamp_exit_state` runs from
    /// `task_terminate(other_tid)` — *any* CPU terminating *any* task — so the
    /// writer is generally not the owner and no exclusivity witness is
    /// obtainable. Release on store / Acquire on load, not Relaxed: the three
    /// are read back into `ExitInfo` before the status store publishes them, so
    /// they carry a publication relationship rather than being counters.
    pub exit_reason: AtomicU16,
    /// The specific fault, as [`TaskFaultReason::as_u16`]. See `exit_reason`.
    pub fault_reason: AtomicU16,
    /// Exit code. See `exit_reason`.
    pub exit_code: AtomicU32,
    /// Pending Wheel-of-Fate outcome, published by the `fate` syscall on
    /// whatever task issued it and cleared by whoever consumes it or by the
    /// exit path. None of those three is guaranteed to be this task, so the
    /// trio is atomic; `fate_pending` is the flag that publishes the pair, so
    /// it is Release on store and Acquire on load while the values are Relaxed.
    pub fate_token: AtomicU32,
    pub fate_value: AtomicU32,
    pub fate_pending: AtomicU8,
    pub cpu_affinity: u32,
    /// CPU this task last ran on, a placement hint.
    ///
    /// Atomic because the enqueue paths stamp it on whichever CPU is
    /// publishing the task while `select_target_cpu` reads it from another.
    pub last_cpu: AtomicU8,
    /// The CPU whose register file last held this task's FPU/vector state, or
    /// [`FPU_CPU_NONE`] for none — the per-task half of the FPU owner tag.
    ///
    /// Not to be confused with [`last_cpu`](Self::last_cpu) directly above,
    /// which is the *scheduler's* placement hint. This one is about the vector
    /// register file and nothing else: it is stamped by an `XSAVE`/`XRSTOR`
    /// pairing, not by an enqueue, and it is meaningful only in agreement with
    /// the per-CPU owner slot. See [`crate::task::fpu_owner`] for why the tag
    /// has two halves and what each one catches.
    ///
    /// Signed rather than a `usize` with a max sentinel because
    /// [`FPU_CPU_NONE`] must not be a representable CPU index, and because it
    /// mirrors Linux's `fpu->last_cpu`.
    fpu_last_cpu: AtomicI32,
    /// How many times this task has been migrated between CPUs.
    ///
    /// Atomic because the **thief** CPU increments it in `work_steal`, while
    /// the task may be running on its victim — a plain `u32` made that a
    /// cross-CPU non-atomic read-modify-write, i.e. a data race, not merely a
    /// soundness wart. It was also unconvertible: no shared borrow can express
    /// a write to a plain field, so this had to become an atomic before the
    /// accessor could take `&self` at all.
    ///
    /// Relaxed: a counter nobody orders anything against. Reaching for
    /// Acquire/Release here would imply a publication relationship that does
    /// not exist.
    pub migration_count: AtomicU32,
    // --- Signal state ---
    /// Bitmask of pending signals (written atomically by kill()).
    pub signal_pending: AtomicU64,
    /// Bitmask of blocked signals (modified by rt_sigprocmask).
    /// Bitmask of blocked signals.
    ///
    /// Atomic because `task_signal_post` reads it from whichever CPU is
    /// sending — every `kill`, every process-group and session fanout — while
    /// the owner writes it in `rt_sigprocmask`, `rt_sigreturn`, and on exec.
    /// A plain field made that a data race the pointer accessors happened to
    /// hide behind a `*mut` reborrow.
    pub signal_blocked: AtomicU64,
    /// Per-signal action table.
    pub signal_actions: [SignalActionCell; NSIG],
    pub switch_ctx: TaskOwnCell<SwitchContext>,
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
    /// Whether this task currently holds one strong reference to *itself* —
    /// the reference it is handed at registration and that is taken back
    /// exactly once when it is reaped.
    ///
    /// Every other owner of a task is a container: a ready queue, a remote
    /// inbox, a CPU's dispatch slot, a parent's children list, a wait map. This
    /// one is the task itself, and it is what keeps a task alive in the states
    /// where no container holds it at all — a blocked kernel thread (whose wait
    /// node stores only an opaque handle, and which has no parent), a placement
    /// reservation that has not reached its queue yet, a freshly created task
    /// before publication, and a child mid-fork before it joins its parent's
    /// list.
    ///
    /// The flag, not the count, is the witness that authorises taking the
    /// reference back, so the reclaim is exactly-once even under a race.
    pub existence_ref_parked: AtomicBool,
    /// Per-task user-mode register snapshot.
    pub user_ctx: TaskOwnCell<UserContext>,
    /// Saved per-task value of `pcr.user_ctx_ptr`.
    pub saved_user_ctx_ptr: AtomicPtr<UserContext>,
    /// Saved per-task copy of `pcr.kernel_return_ctx`.
    pub saved_kernel_return_ctx: TaskOwnCell<KernelReturnContext>,
    /// Durable per-task exit value.
    pub exit_info: AtomicCell<ExitInfo>,
}

impl<K, U> Drop for TaskInner<K, U> {
    fn drop(&mut self) {
        crate::task::drop_context::assert_task_drop_context();
        self.assert_no_owner_holds_this_task();
    }
}

/// Encoding of "no controlling terminal" in [`TaskInner::controlling_tty`].
/// Out of range for a `TtyIndex`, whose payload is a `u8`, so it cannot
/// collide with a real terminal. Deliberately not zero: the task initialiser
/// bulk-zeroes, and a zero sentinel would silently mean "TTY 0".
pub const TTY_INDEX_NONE: u16 = u16::MAX;

impl<K, U> TaskInner<K, U> {
    /// This task's FS segment base (TLS pointer).
    ///
    /// Acquire, pairing with [`set_fs_base`](Self::set_fs_base)'s Release: the
    /// value must be visible to the next `prepare_switch_to` on any CPU, which
    /// reads the *incoming* task's copy.
    #[inline]
    pub fn fs_base(&self) -> u64 {
        self.fs_base.load(Ordering::Acquire)
    }

    /// Stamp this task's FS segment base. See [`fs_base`](Self::fs_base).
    #[inline]
    pub fn set_fs_base(&self, value: u64) {
        self.fs_base.store(value, Ordering::Release);
    }

    /// The CPU whose register file last held this task's FPU state, or
    /// [`FPU_CPU_NONE`]. The per-task half of the FPU owner tag; meaningful
    /// only in agreement with the per-CPU half, via
    /// [`fpu_state_valid`](crate::task::fpu_owner::fpu_state_valid).
    #[inline]
    pub fn fpu_last_cpu(&self) -> i32 {
        self.fpu_last_cpu.load(Ordering::Acquire)
    }

    /// Stamp the per-task half of the FPU owner tag.
    ///
    /// `pub(crate)` on purpose: outside OSTD the only sanctioned way to move
    /// this field is [`fpu_save_current`](Self::fpu_save_current) /
    /// [`fpu_restore_to_cpu`](Self::fpu_restore_to_cpu), which move both halves
    /// together. A caller able to stamp one half alone could manufacture the
    /// agreement the tag exists to check.
    #[inline]
    pub(crate) fn set_fpu_last_cpu(&self, cpu: i32) {
        self.fpu_last_cpu.store(cpu, Ordering::Release);
    }

    /// Capture this task's live FPU/vector registers into its save area and
    /// hand the register file back.
    ///
    /// Pairs the `XSAVE64` with both halves of the owner tag so a call site
    /// cannot do one without the other. The witness proves this CPU has
    /// exclusive access to the task's register state — the same fact that makes
    /// the save sound, now checkable rather than commented.
    ///
    /// Eager by construction: no "save only if dirty" branch. Lazy FPU
    /// switching is CVE-2018-3665 and this is not it.
    #[inline]
    pub fn fpu_save_current(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        fpu_owner_assert_may_take(self, cpu);
        // SAFETY: the witness proves exclusive access to this task's register
        // state, and `get_ptr` yields a `SharedReadWrite` derivation of the
        // cell — never a `&mut` — so a nested witness on the same task (an
        // interrupt above a syscall) cannot invalidate this pointer. XSAVE64's
        // 64-byte alignment is pinned by the razors beside `TaskOwnCell`.
        unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_ptr(witness), xcr0_mask) };
        fpu_owner_yield_after_save(self, cpu);
    }

    /// Load this task's saved FPU/vector state into the register file and take
    /// ownership of it. Mirror of [`fpu_save_current`](Self::fpu_save_current).
    ///
    /// Deliberately unconditional. The tag makes it *possible* to skip a
    /// restore when [`fpu_state_valid`](crate::task::fpu_owner::fpu_state_valid)
    /// already holds — Linux's optimisation, and not lazy FPU — but taking it
    /// is a behaviour change, so this entry point stays eager and the predicate
    /// is exported for a caller that opts in explicitly.
    #[inline]
    pub fn fpu_restore_to_cpu(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        // No precondition: a restore defines the register file's new contents,
        // and is not always preceded by a save on this CPU — see
        // `fpu_owner_assert_may_take`.
        // SAFETY: as `fpu_save_current`; XRSTOR64 only reads the buffer.
        unsafe {
            crate::task::fpu::fpu_xrstor(self.fpu_state.get_ptr(witness).cast_const(), xcr0_mask)
        };
        fpu_owner_take(self, cpu);
    }

    /// Capture the live registers into the save area **without** handing the
    /// register file back.
    ///
    /// For the two saves that are not switch-outs — the signal-frame save and
    /// the fork flush. Both run while the task keeps executing, so its state is
    /// still live in the registers afterwards and the owner tag must keep
    /// saying so.
    #[inline]
    pub fn fpu_save_in_place(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        fpu_owner_assert_may_take(self, cpu);
        // SAFETY: as `fpu_save_current`.
        unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_ptr(witness), xcr0_mask) };
        fpu_owner_take(self, cpu);
    }

    /// `&mut self`-authorised counterparts to the two ops above.
    ///
    /// Exclusive access proven by the borrow rather than by a witness, for the
    /// paths that already hold `&mut TaskInner` — the signal-frame save and
    /// restore reach the task through `SyscallContext::task_mut`. Minting a
    /// `CurrentTask` witness alongside that `&mut` would alias it, so these
    /// exist rather than forcing a caller to hold two exclusive proofs of the
    /// same fact at once. Both maintain the owner tag.
    #[inline]
    pub fn fpu_save_in_place_mut(&mut self, xcr0_mask: u64) {
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        fpu_owner_assert_may_take(self, cpu);
        // SAFETY: `&mut self` is exclusive access to the whole task.
        unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_mut(), xcr0_mask) };
        fpu_owner_take(self, cpu);
    }

    /// See [`fpu_save_in_place_mut`](Self::fpu_save_in_place_mut).
    #[inline]
    pub fn fpu_restore_to_cpu_mut(&mut self, xcr0_mask: u64) {
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        // Restore side takes no precondition — see `fpu_restore_to_cpu`.
        // SAFETY: `&mut self` is exclusive access to the whole task.
        unsafe { crate::task::fpu::fpu_xrstor(self.fpu_state.get_mut(), xcr0_mask) };
        fpu_owner_take(self, cpu);
    }

    /// Borrow this task's FPU save area as bytes, authorised by `witness`.
    ///
    /// The signal-frame paths copy the area to and from user memory and cannot
    /// stage it through a 2.6 KiB stack buffer (the frame gate is 2 KiB), so
    /// they borrow it in place. Same re-entrancy contract as
    /// [`with_cwd`](Self::with_cwd): `f` must not itself take a witness on this
    /// task and call back in, or the `&mut` handed out here would alias.
    #[inline]
    pub fn with_fpu_bytes_mut<R>(
        &self,
        witness: &impl TaskExclusive<K, U>,
        f: impl FnOnce(&mut [u8; FPU_STATE_SIZE]) -> R,
    ) -> R {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        // SAFETY: the witness proves exclusive access; the contract above
        // forbids re-entering through a second witness while `f` runs.
        f(unsafe { &mut (*self.fpu_state.get_ptr(witness)).data })
    }

    /// Reset the FPU save area to the kernel default (x87/SSE exceptions
    /// masked, XSTATE header zeroed), authorised by `witness`.
    ///
    /// The execve disposition reset: the old image's vector state must not
    /// survive into the new one. Paired with
    /// [`fpu_restore_to_cpu`](Self::fpu_restore_to_cpu) under one IRQ-off
    /// window, so a context switch cannot re-save the old image's live
    /// registers over the reset before the new image runs.
    #[inline]
    pub fn fpu_reset(&self, witness: &impl TaskExclusive<K, U>) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        // SAFETY: the witness proves exclusive access; `fpu_reset_in_place`
        // writes a whole valid `FpuState` into the slot.
        unsafe { fpu_reset_in_place(self.fpu_state.get_ptr(witness)) };
    }

    /// This task's user-mode register snapshot, authorised by `witness`. Raw
    /// rather than `&mut` for the reason `TaskOwnCell::get_ptr` is: two
    /// witnesses on one task may legitimately coexist.
    #[inline]
    pub fn user_ctx_ptr(&self, witness: &impl TaskExclusive<K, U>) -> *mut UserContext {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        self.user_ctx.get_ptr(witness)
    }

    /// This task's saved callee-saved register frame, authorised by `witness`.
    /// Feeds [`switch_registers`](crate::task::switch_registers), whose asm
    /// takes both endpoints as raw pointers.
    #[inline]
    pub fn switch_ctx_ptr(&self, witness: &impl TaskExclusive<K, U>) -> *mut SwitchContext {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        self.switch_ctx.get_ptr(witness)
    }

    /// Replace the working directory. The witness is what proves no other CPU
    /// is reading the bytes while they are half-written; the length is
    /// published after them.
    ///
    /// Returns false if `path` does not fit with its NUL terminator.
    #[inline]
    pub fn set_cwd(&self, witness: &impl TaskExclusive<K, U>, path: &[u8]) -> bool {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        if path.len() + 1 > CWD_MAX {
            return false;
        }
        let cwd = self.cwd.get_ptr(witness).cast::<u8>();
        // SAFETY: the witness proves exclusive access to this task's `cwd`, and
        // the bounds check above keeps the write inside the array.
        let dst = unsafe { core::slice::from_raw_parts_mut(cwd, CWD_MAX) };
        dst[..path.len()].copy_from_slice(path);
        dst[path.len()] = 0;
        self.cwd_len.store(path.len() as u16, Ordering::Release);
        true
    }

    /// Call `f` with the working directory including its NUL terminator.
    #[inline]
    pub fn with_cwd<R>(&self, witness: &impl TaskExclusive<K, U>, f: impl FnOnce(&[u8]) -> R) -> R {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let len = self.cwd_len.load(Ordering::Acquire) as usize;
        let cwd = self.cwd.get_ptr(witness).cast::<u8>();
        // SAFETY: the witness proves exclusive access; `len` was published
        // after the bytes it describes, and `set_cwd` bounds it by the array.
        let bytes = unsafe { core::slice::from_raw_parts(cwd, (len + 1).min(CWD_MAX)) };
        f(bytes)
    }

    /// Read the blocked-signal mask. `&self` because every signal sender reads
    /// it from its own CPU.
    #[inline]
    pub fn signal_blocked(&self) -> SigSet {
        self.signal_blocked.load(Ordering::Acquire)
    }

    /// Replace the blocked-signal mask. `&self` for the same reason; only the
    /// owning task writes it.
    #[inline]
    pub fn set_signal_blocked(&self, mask: SigSet) {
        self.signal_blocked.store(mask, Ordering::Release);
    }

    /// This task's controlling terminal, if any.
    #[inline]
    pub fn controlling_tty(&self) -> Option<TtyIndex> {
        match self.controlling_tty.load(Ordering::Acquire) {
            TTY_INDEX_NONE => None,
            raw => Some(TtyIndex(raw as u8)),
        }
    }

    /// Set or clear the controlling terminal. `&self` because the
    /// session-hangup path reaches tasks through a shared snapshot.
    #[inline]
    pub fn set_controlling_tty(&self, tty: Option<TtyIndex>) {
        let raw = tty.map_or(TTY_INDEX_NONE, |t| u16::from(t.0));
        self.controlling_tty.store(raw, Ordering::Release);
    }

    /// Clear the controlling terminal only if it currently names `tty`,
    /// reporting whether this call did the clearing.
    ///
    /// Compare-and-clear rather than load-then-store so a session teardown
    /// racing a task that has already moved to a different terminal cannot
    /// clear the new one.
    #[inline]
    pub fn clear_controlling_tty_if(&self, tty: TtyIndex) -> bool {
        self.controlling_tty
            .compare_exchange(
                u16::from(tty.0),
                TTY_INDEX_NONE,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Timestamp of this task's last dispatch.
    #[inline]
    pub fn last_run_timestamp(&self) -> u64 {
        self.last_run_timestamp.load(Ordering::Acquire)
    }

    /// Stamp the last-dispatch timestamp. `&self`: written by the owning CPU's
    /// switch path while other CPUs read it.
    #[inline]
    pub fn set_last_run_timestamp(&self, timestamp: u64) {
        self.last_run_timestamp.store(timestamp, Ordering::Release);
    }

    /// CPU this task last ran on.
    #[inline]
    pub fn last_cpu(&self) -> u8 {
        self.last_cpu.load(Ordering::Acquire)
    }

    /// Stamp the last-CPU placement hint.
    #[inline]
    pub fn set_last_cpu(&self, cpu: u8) {
        self.last_cpu.store(cpu, Ordering::Release);
    }

    /// Debug tripwire that nothing still claims to own this task at reclaim.
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
    fn assert_no_owner_holds_this_task(&self) {
        // The count could not have reached zero while the task still held a
        // reference to itself, so observing this here means a reap released the
        // reference without clearing the flag, or a copy inherited it.
        debug_assert!(
            !self.existence_ref_parked.load(Ordering::Relaxed),
            "task dropped while still holding its own existence reference"
        );
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
            context: TaskOwnCell::new(TaskContext::zero()),
            fpu_state: TaskOwnCell::new(FpuState::new()),
            kernel_stack: None,
            unsafe_stack: None,
            process_group: RcuArcSlot::empty(),
            test_reports: SpinLock::new(None, LOCK_LEVEL_RESOURCE),
            parent_task_id: INVALID_TASK_ID,
            fs_base: AtomicU64::new(0),
            tgid: INVALID_TASK_ID,
            pgid: INVALID_TASK_ID,
            sid: INVALID_TASK_ID,
            controlling_tty: AtomicU16::new(TTY_INDEX_NONE),
            cwd: TaskOwnCell::new({
                let mut c = [0u8; CWD_MAX];
                c[0] = b'/';
                c
            }),
            cwd_len: AtomicU16::new(1),
            clear_child_tid: AtomicU64::new(0),
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: AtomicU64::new(0),
            creation_time: 0,
            yield_count: AtomicU32::new(0),
            last_run_timestamp: AtomicU64::new(0),
            user_started: 0,
            context_from_user: 0,
            exit_reason: AtomicU16::new(TaskExitReason::None.as_u16()),
            fault_reason: AtomicU16::new(TaskFaultReason::None.as_u16()),
            exit_code: AtomicU32::new(0),
            fate_token: AtomicU32::new(0),
            fate_value: AtomicU32::new(0),
            fate_pending: AtomicU8::new(0),
            cpu_affinity: 0,
            last_cpu: AtomicU8::new(0),
            fpu_last_cpu: AtomicI32::new(FPU_CPU_NONE),
            migration_count: AtomicU32::new(0),
            signal_pending: AtomicU64::new(0),
            signal_blocked: AtomicU64::new(SIG_EMPTY),
            signal_actions: [const { SignalActionCell::default() }; NSIG],
            switch_ctx: TaskOwnCell::new(SwitchContext::zero()),
            on_cpu: AtomicBool::new(false),
            ready_link: Link::new(),
            remote_inbox_link: Link::new(),
            children: IntrusiveDList::new(),
            sibling_link: DLink::new(),
            reclaim_link: Link::new(),
            sched_placement: AtomicU8::new(SchedPlacement::Nascent.as_u8()),
            recovery_depth: AtomicU32::new(0),
            panic_in_flight: AtomicU32::new(0),
            exit_cleanup_flags: AtomicU8::new(0),
            existence_ref_parked: AtomicBool::new(false),
            user_ctx: TaskOwnCell::new(UserContext::const_zeroed()),
            saved_user_ctx_ptr: AtomicPtr::new(ptr::null_mut()),
            saved_kernel_return_ctx: TaskOwnCell::new(KernelReturnContext {
                rbx: 0,
                rbp: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                rsp: 0,
                rip: 0,
            }),
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

                // Not all-zero: zero would name TTY 0, not "no controlling
                // terminal".
                addr_of_mut!((*slot).controlling_tty).write(AtomicU16::new(TTY_INDEX_NONE));

                // Also not all-zero: zero would name CPU 0 as holding this
                // task's vector state, which would let a never-run task pass
                // the FPU owner agreement check on the boot CPU.
                addr_of_mut!((*slot).fpu_last_cpu).write(AtomicI32::new(FPU_CPU_NONE));

                addr_of_mut!((*slot).cwd_len).write(AtomicU16::new(1));
                // `TaskOwnCell` is `repr(transparent)`, so the cell's address
                // is the array's.
                (addr_of_mut!((*slot).cwd) as *mut u8).write(b'/');

                // `TaskOwnCell` is `repr(transparent)`, so the cell's
                // address is the buffer's.
                fpu_reset_in_place(addr_of_mut!((*slot).fpu_state).cast::<FpuState>());

                addr_of_mut!((*slot).kernel_stack).write(None);
                addr_of_mut!((*slot).unsafe_stack).write(None);
                addr_of_mut!((*slot).process_group).write(RcuArcSlot::empty());
                addr_of_mut!((*slot).test_reports).write(SpinLock::new(None, LOCK_LEVEL_RESOURCE));
                addr_of_mut!((*slot).abi.unsafe_stack_sp).write(0);

                addr_of_mut!((*slot).signal_blocked).write(AtomicU64::new(SIG_EMPTY));
                for i in 0..NSIG {
                    let p = (addr_of_mut!((*slot).signal_actions) as *mut SignalActionCell).add(i);
                    p.write(SignalActionCell::default());
                }

                addr_of_mut!((*slot).switch_ctx).write(TaskOwnCell::new(SwitchContext::zero()));

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
        self.total_runtime.store(0, Ordering::Relaxed);
        self.creation_time = crate::kdiag_timestamp();
        self.yield_count.store(0, Ordering::Relaxed);
        self.last_run_timestamp.store(0, Ordering::Relaxed);
        self.exit_reason
            .store(TaskExitReason::None.as_u16(), Ordering::Release);
        self.fault_reason
            .store(TaskFaultReason::None.as_u16(), Ordering::Release);
        self.exit_code.store(0, Ordering::Release);
        self.clear_fate();
        self.on_cpu.store(false, Ordering::Release);
        self.ready_link.reset();
        self.remote_inbox_link.reset();
        self.sibling_link.reset();
        self.reclaim_link.reset();
        // Nascent, not None: a task that has been reset for (re)construction
        // has not been published, and None is also a blocked task's placement.
        self.sched_placement
            .store(SchedPlacement::Nascent.as_u8(), Ordering::Release);
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
            core::ptr::write(&mut self.process_group as *mut _, RcuArcSlot::empty());
            core::ptr::write(
                &mut self.test_reports as *mut _,
                SpinLock::new(None, LOCK_LEVEL_RESOURCE),
            );
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
        self.sched_placement = AtomicU8::new(SchedPlacement::Nascent.as_u8());
        self.recovery_depth = AtomicU32::new(0);
        self.exit_cleanup_flags = AtomicU8::new(0);
        self.signal_pending = AtomicU64::new(0);
        // The bytewise copy duplicated the live parent's parked flag. A child
        // is handed its own existence reference at registration; inheriting the
        // parent's `true` would let the child's reap take back a reference that
        // was never given, dropping the count one below what any owner holds.
        self.existence_ref_parked = AtomicBool::new(false);
        // The bytewise copy also duplicated the parent's FPU owner tag. The
        // child's vector state has never been live in any register file, so
        // inheriting a CPU index would let it agree with a slot that names the
        // *parent* — and skip a restore it genuinely needs. Poisoned so the
        // agreement check fails and the child takes the slow path.
        self.fpu_last_cpu = AtomicI32::new(FPU_CPU_NONE);
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
