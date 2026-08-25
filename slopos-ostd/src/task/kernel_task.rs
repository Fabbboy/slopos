//! Kernel-internal task control block, parameterised over the stack handle
//! types so OSTD need not depend on the kernel-side stack-region machinery.
//! The kernel side aliases `TaskInner<K, U>` as `Task`.

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
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE,
    TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord, TaskFaultReason, TaskPriority,
    TaskStatus,
};

use crate::cpu::x86_64::pcr::KernelReturnContext;
use crate::sync::intrusive::Link;
use crate::sync::intrusive_dlist::{DLink, IntrusiveDList};
use crate::sync::{AtomicCell, LOCK_LEVEL_RESOURCE, RcuArcSlot, SpinLock};
use crate::task::abi::TaskAbi;
use crate::task::cell::{TaskExclusive, TaskOwnCell};
use crate::task::exit_info::ExitInfo;
use crate::task::fpu::{FPU_STATE_SIZE, FpuState, LEGACY_FCW_OFFSET, LEGACY_MXCSR_OFFSET};
use crate::task::fpu_owner::{
    FPU_CPU_NONE, fpu_owner_assert_may_take, fpu_owner_take, fpu_owner_yield_after_save,
};
use crate::task::job_control::ProcessGroup;
use crate::task::link_roles::{
    FutexRole, ReadyQueueRole, ReclaimRole, RemoteWakeRole, SiblingRole,
};
use crate::task::state::TaskState;
use crate::task::test_reports::TestReportRing;
use crate::user::context::UserContext;
use crate::{AllocError, Init, KBox, init_from_closure};

/// CPU register state saved during context switches.
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

/// Callee-saved snapshot consumed by the OSTD context-switch primitives in
/// [`crate::task::switch`]; its layout and the matching naked-asm offsets are
/// [`crate::task::TaskContext`]'s.
pub type SwitchContext = crate::task::TaskContext;

/// Capacity of a task's working-directory buffer, NUL terminator included.
pub const CWD_MAX: usize = 256;

/// Initialise an [`FpuState`] directly at `ptr`, avoiding the 2.6 KiB rvalue
/// on the caller's stack that [`FpuState::new`] would materialise.
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

/// Kernel-internal signal action: the per-task subset of `UserSigaction`.
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
/// sending while the owner rewrites the whole entry in `rt_sigaction`, on exec,
/// and through the spawn `sigdefault` mask.
///
/// Four independent atomics rather than a lock: senders want only `handler` and
/// the owner reads the fields in its own program order, so there is no group to
/// keep consistent. The layout matches the plain struct, so `Task`'s size is
/// unchanged.
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

    /// Read the whole disposition. Only the owning task may: it is the sole
    /// writer and so cannot observe a half-written entry, while a remote CPU
    /// reading the group can pair an old handler with a new mask and must use
    /// [`handler`](Self::handler) instead.
    ///
    /// `handler` must stay the **first** load: its Acquire pairs with the
    /// Release in [`store`](Self::store), which writes it last.
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
    /// reader that observes a new handler also observes the rest of the entry.
    #[inline]
    pub fn store(&self, action: SignalAction) {
        self.mask.store(action.mask, Ordering::Release);
        self.flags.store(action.flags, Ordering::Release);
        self.restorer.store(action.restorer, Ordering::Release);
        self.handler.store(action.handler, Ordering::Release);
    }

    #[inline]
    pub fn reset(&self) {
        self.store(SignalAction::default());
    }
}

/// Scheduler-owned placement for a runnable or physically running task.
///
/// `TaskStatus::Ready` is not enough to prove schedulability on SMP: a task
/// also needs exactly one scheduler owner.
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
    /// A publisher has reserved scheduler ownership but has not linked the task
    /// into its final queue/inbox yet, closing the Ready-with-no-owner
    /// publication window.
    Waking = 5,
    /// Allocated, possibly registered, never published.
    ///
    /// Distinct from `None`, which is also a blocked and a terminated task's
    /// placement: between `register_task` and `publish_new_task` a task is
    /// registry-visible with status `Blocked`, and without this state a
    /// process-group signal arriving in that window cannot be told from a
    /// legitimate wake target.
    ///
    /// Entered once, at allocation; left only by the two publication entry
    /// points (→ `Waking`, rolling back here if publication fails), the per-CPU
    /// idle installer (→ `OnCpu`, since an idle task is dispatched rather than
    /// published), and teardown (→ `None`). Holds no reference.
    Nascent = 6,
    /// Held off every scheduler container by a kernel-I/O hold, which owns it
    /// until it releases.
    Held = 7,
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
            7 => Self::Held,
            _ => Self::None,
        }
    }
}

// The `offset_of!`-based layout razors live in the kernel-side shim, where the
// concrete `Task` alias is in scope so `offset_of!` resolves.

/// Generic kernel task control block.
///
/// `K` is the kernel-mode stack handle and `U` the SafeStack data ("unsafe")
/// stack handle; the kernel side passes `slopos_sched::task_stack::KernelStack`
/// and `UnsafeStack`.
/// `Task::caps` before the authority model has computed one.
///
/// Not zero and not `u64::MAX`: either would be a *decision*, and a task whose
/// mask has never been derived has not had one made. Readers fall back to the
/// flag-derived value, so a task built on a path that predates the derivation
/// is neither omnipotent nor powerless.
pub const CAPS_UNSET: u64 = u64::MAX;

#[repr(C)]
pub struct TaskInner<K, U> {
    /// ABI sub-struct holding every field naked asm reads via a compile-time
    /// `const` offset operand. Must remain at offset 0; the kernel-side shim's
    /// `offset_of!(Task, abi) == 0` razor enforces it.
    pub abi: TaskAbi,
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    /// Fused (status, reason, epoch) atomic word, so a stale reason can never be
    /// observed outliving its status. Bit layout in `crate::task::state`.
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
    // Nothing below here is accessed by assembly; these fields may be reordered.
    /// Owning handle to the kernel-mode stack; `None` only on `invalid()` slots
    /// and after `free_task_stacks`. Dropping it unmaps the stack pages, returns
    /// their frames to the page allocator and releases the VA slot.
    pub kernel_stack: Option<K>,
    /// Owning handle to the SafeStack-sanitizer unsafe (data) stack.
    pub unsafe_stack: Option<U>,
    /// Packed handle to this task's address space; 0 means it has none. Low
    /// [`PROCESS_VM_SLOT_BITS`] bits are the slot index, the rest a generation
    /// stamped when the slot was bound.
    ///
    /// Not `process_id`: ids are recycled, so a task holding only an id can be
    /// handed the address space of whichever process holds that id *now* —
    /// which on a page fault means servicing it in a stranger's page tables.
    /// Resolving the handle fails on a rebound slot instead.
    ///
    /// [`PROCESS_VM_SLOT_BITS`]: crate::handle::PROCESS_VM_SLOT_BITS
    process_vm_handle: AtomicU64,
    /// Packed handle to the [`Process`] this task belongs to; 0 means none.
    ///
    /// Beside `process_vm_handle` rather than replacing it, because they name
    /// different objects: several tasks share one `Process`, and a `CLONE_VM`
    /// thread shares its parent's address space *and* its process, while a
    /// forked child shares neither.
    ///
    /// Every question once answered by scanning for a matching `process_id` —
    /// whose address space is this, whose descriptor table, is this the last
    /// task of its process — is answered from here instead, and answers *stale*
    /// rather than answering about whichever process holds that id now.
    ///
    /// [`Process`]: crate::process::Process
    process_handle: AtomicU64,
    /// Strong membership in this task's process group. The group (and, via
    /// it, the session) lives while any member holds this handle; dropping the
    /// task releases the membership. Empty for kernel-mode tasks.
    ///
    /// Published through an RCU slot because the writer is not the owner:
    /// `setpgid(pid, …)` re-homes a *different* task, so the store lands on a
    /// field a reader on another CPU may be cloning from at that instant. The
    /// displaced reference is released only once a grace period has elapsed, so
    /// the writer cannot drive to zero a count a reader is still raising, and no
    /// destructor runs on the writer's stack.
    pub process_group: RcuArcSlot<ProcessGroup>,
    /// Per-task ring of `SYSCALL_TEST_REPORT` payloads, `None` until the task's
    /// first report allocates one; the userland-test runner takes ownership once
    /// the task exits.
    ///
    /// Behind a `SpinLock` rather than an atomic: it owns a `KBox`, which no
    /// atomic can hold, and the owner allocates it while a *foreign* task drains
    /// it after exit (`task_drain_test_reports`), so a shared borrow has to be
    /// enough for both. `LOCK_LEVEL_RESOURCE`: neither side is on the switch path
    /// or runs with interrupts off, and the allocation happens before the store
    /// so the lock never covers one.
    pub test_reports: SpinLock<Option<KBox<TestReportRing>>>,
    /// Id of this task's parent, or `INVALID_TASK_ID` once reparented. Atomic
    /// because a dying parent stamps it into each of its live children while
    /// `getppid`, the task-list syscall and the pidfd layer read it elsewhere.
    pub parent_task_id: AtomicU32,
    /// FS segment base (TLS pointer), loaded into MSR FS_BASE before switching
    /// to user mode.
    ///
    /// Atomic because the owner writes it via `arch_prctl` while
    /// `prepare_switch_to` reads the *incoming* task's copy from whichever CPU
    /// is performing the switch. Release/Acquire rather than Relaxed: the value
    /// must be visible to the next switch on any CPU.
    pub fs_base: AtomicU64,
    pub tgid: u32,
    /// Process-group and session ids. Atomic because `setpgid`/`setsid` retarget
    /// a task from its own CPU while the process-group signal walk, the
    /// orphaned-group test and the TTY hangup sweep read them from CPUs that do
    /// not own it.
    ///
    /// Relaxed: they are identity scalars, not a publication protocol. Where
    /// one has to be seen together with the [`process_group`](Self::process_group)
    /// membership it mirrors, the ordering comes from that slot's own
    /// Release store, which every writer performs after stamping these.
    pub pgid: AtomicU32,
    pub sid: AtomicU32,
    /// Controlling terminal of this task's session, encoded per
    /// [`TTY_INDEX_NONE`]. Atomic because the session leader's exit walks every
    /// task in the session through a *shared* snapshot of the task table and
    /// clears the field on each, concurrently with those tasks reading it.
    pub controlling_tty: AtomicU16,
    /// Working-directory path, NUL-terminated. Only the owning task reads or
    /// writes it, so the cell's witness is always a `CurrentTask`.
    pub cwd: TaskOwnCell<[u8; CWD_MAX]>,
    /// Length of `cwd` up to but not including the NUL. Atomic so it can be
    /// published after the bytes.
    pub cwd_len: AtomicU16,
    /// User-space address to clear (and futex-wake) on thread exit. Atomic
    /// because the exit path runs on whichever task called `task_terminate` —
    /// not necessarily this one.
    pub clear_child_tid: AtomicU64,
    /// The task's full scheduling quantum, in timer ticks. Atomic because
    /// `reset_task_quantum` runs on the *waking* CPU against a task that may
    /// still be `on_cpu` elsewhere, while the timer ISR on that other CPU is
    /// decrementing [`time_slice_remaining`](Self::time_slice_remaining) below.
    pub time_slice: AtomicU64,
    /// Ticks left in the current quantum. Decremented by the timer ISR on the
    /// CPU running the task; reset cross-CPU with `time_slice` above.
    pub time_slice_remaining: AtomicU64,
    /// Accumulated on-CPU time. Atomic because the switch-out tail and the exit
    /// path both add to it from different CPUs while the task-list syscall reads
    /// it from a registry walk.
    pub total_runtime: AtomicU64,
    pub creation_time: u64,
    /// Voluntary-yield count. Atomic and Relaxed for the same reasons as
    /// `migration_count` below: a diagnostic tally with no publication
    /// relationship.
    pub yield_count: AtomicU32,
    /// Timestamp of this task's last dispatch. Atomic because the
    /// context-switch path writes it on the owning CPU while the stranded-task
    /// rescue sweep reads it from another.
    pub last_run_timestamp: AtomicU64,
    /// Whether this task has ever entered user mode, and whether its saved
    /// context came from a user-mode trap frame rather than a kernel switch.
    ///
    /// Atomic so the trap-save path can stamp them through the same witness
    /// that authorises the register write. Relaxed: each is a standalone flag
    /// written by the task's own CPU, ordered by the trap frame it accompanies.
    pub user_started: AtomicU8,
    pub context_from_user: AtomicU8,
    /// Why the task exited, as [`TaskExitReason::as_u16`].
    ///
    /// This trio is atomic because `stamp_exit_state` runs from
    /// `task_terminate(other_tid)` — *any* CPU terminating *any* task — so the
    /// writer is generally not the owner and no exclusivity witness is
    /// obtainable. Release/Acquire rather than Relaxed: the three are read back
    /// into `ExitInfo` before the status store publishes them.
    pub exit_reason: AtomicU16,
    /// The specific fault, as [`TaskFaultReason::as_u16`]. See `exit_reason`.
    pub fault_reason: AtomicU16,
    /// Exit code. See `exit_reason`.
    pub exit_code: AtomicU32,
    /// Pending Wheel-of-Fate outcome. Atomic because none of the publisher, the
    /// consumer and the exit path that clears it is guaranteed to be this task;
    /// `fate_pending` publishes the pair, so it is Release/Acquire while the
    /// values are Relaxed.
    pub fate_token: AtomicU32,
    pub fate_value: AtomicU32,
    pub fate_pending: AtomicU8,
    /// Bitmask of CPUs this task may run on. Atomic because
    /// `sched_setaffinity` stamps another task's mask while the work-stealer and
    /// the switch-out repatriation check read it on whichever CPU that task
    /// happens to be on.
    pub cpu_affinity: AtomicU32,
    /// CPU this task last ran on, a placement hint. Atomic because the enqueue
    /// paths stamp it on whichever CPU is publishing the task while
    /// `select_target_cpu` reads it from another.
    pub last_cpu: AtomicU8,
    /// The CPU whose register file last held this task's FPU/vector state, or
    /// [`FPU_CPU_NONE`] for none — the per-task half of the FPU owner tag.
    ///
    /// Not [`last_cpu`](Self::last_cpu) directly above, which is the
    /// *scheduler's* placement hint: this one is stamped by an `XSAVE`/`XRSTOR`
    /// pairing and is meaningful only in agreement with the per-CPU owner slot.
    /// See [`crate::task::fpu_owner`] for what each half of the tag catches.
    ///
    /// Signed rather than a `usize` with a max sentinel because
    /// [`FPU_CPU_NONE`] must not be a representable CPU index.
    fpu_last_cpu: AtomicI32,
    /// How many times this task has been migrated between CPUs. Atomic because
    /// the **thief** CPU increments it in `work_steal` while the task may be
    /// running on its victim.
    ///
    /// Relaxed: a counter nobody orders anything against.
    pub migration_count: AtomicU32,
    /// Effective capability mask — the authority model's record for this task.
    ///
    /// Atomic and separate from [`flags`](Self::flags) because it is *narrowed
    /// on exec*, in the task's own context, while other CPUs read it to answer
    /// "may this task be signalled" and "may it invoke that syscall". `flags`
    /// is a plain word written only before publication; authority is not, so it
    /// gets its own.
    ///
    /// `u64::MAX` is the "not yet computed" sentinel: a task built before the
    /// authority model derives its mask reads through to the flag-derived
    /// value, so no task is accidentally omnipotent and none is accidentally
    /// powerless.
    pub caps: AtomicU64,
    /// Bitmask of pending signals, written by `kill()`.
    pub signal_pending: AtomicU64,
    /// Bitmask of blocked signals. Atomic because `task_signal_post` reads it
    /// from whichever CPU is sending while the owner writes it in
    /// `rt_sigprocmask`, `rt_sigreturn`, and on exec.
    pub signal_blocked: AtomicU64,
    pub signal_actions: [SignalActionCell; NSIG],
    pub switch_ctx: TaskOwnCell<SwitchContext>,
    /// Set while a CPU is physically executing this task.
    pub on_cpu: AtomicBool,
    pub ready_link: Link<TaskInner<K, U>, ReadyQueueRole>,
    /// Intrusive link slot for per-CPU remote wake inboxes.
    ///
    /// The inbox is a lock-free Treiber stack whose one-element case has a null
    /// successor while still being a member, so membership needs its own bit. A
    /// role-typed `Link` carries both, which is what stops a duplicate remote
    /// wake self-cycling the inbox and lets diagnostics tell pending wake
    /// delivery from a genuinely stranded Ready task.
    pub remote_inbox_link: Link<TaskInner<K, U>, RemoteWakeRole>,
    /// Owning list of this task's live and zombie children. Linking a child
    /// pairs with `task_placement_retain` and unlinking with
    /// `task_placement_reclaim`, so membership parks a strong reference — which
    /// is what pins a zombie child until `waitpid` reaps it or this task's own
    /// teardown drains the list. There is no separate zombie list.
    pub children: IntrusiveDList<TaskInner<K, U>, SiblingRole>,
    /// This task's membership in the one owner list holding it. The role-typed
    /// slot rejects a double-link, and its owner back-pointer lets the task be
    /// unlinked without naming which list that is.
    pub sibling_link: DLink<TaskInner<K, U>, SiblingRole>,
    /// Intrusive link slot for the task graveyard — tasks awaiting destruction
    /// in a context where the allocator may run.
    ///
    /// Unlike every other link slot, membership here parks no strong reference:
    /// the pusher won the final release, so the count is already zero and the
    /// pusher owns the allocation outright. That is why it gets its own role.
    pub reclaim_link: Link<TaskInner<K, U>, ReclaimRole>,
    /// Membership in a futex wait bucket. Doubly linked so a waiter that is
    /// woken by a signal, a kill or a timeout unlinks itself in O(1) without
    /// naming which bucket holds it.
    pub futex_link: DLink<TaskInner<K, U>, FutexRole>,
    /// The futex word this task is parked on while `futex_link` is linked.
    pub futex_addr: AtomicU64,
    /// Explicit scheduler placement owner. The cross-role gate that keeps a task
    /// out of a ready queue and a remote wake inbox at the same time.
    pub sched_placement: AtomicU8,
    /// The `WaitQueue` this task is currently parked on, or null. Erased to
    /// `*mut c_void` because `WaitQueue` cannot name a `TaskInner`
    /// monomorphisation and this module cannot name `WaitQueue`.
    ///
    /// Read by teardown from any CPU, so a task torn down while parked has its
    /// stack-pinned wait node unlinked before that stack slot is recycled.
    pub(crate) parked_wait_queue: AtomicPtr<c_void>,
    /// Panic-recovery nesting depth while this task is not running; the live
    /// value is `PCR.recovery_depth`. Saved and restored across a switch so
    /// recovery scopes survive migration.
    pub recovery_depth: AtomicU32,
    /// Panic in-flight depth while this task is not running; the live value is
    /// `PCR.panic_in_flight`. An unwinding task runs interrupts-on and can be
    /// preempted or migrate mid-unwind, so the depth travels with the task like
    /// `recovery_depth`.
    pub panic_in_flight: AtomicU32,
    /// Idempotence bits for task/process teardown that may be split between
    /// `task_terminate` and post-switch cleanup of the current task.
    pub exit_cleanup_flags: AtomicU8,
    /// Whether this task currently holds one strong reference to *itself* — the
    /// reference it is handed at registration and that is taken back exactly
    /// once when it is reaped.
    ///
    /// Every other owner of a task is a container, so this is what keeps a task
    /// alive in the states where none holds it: a blocked kernel thread, a
    /// placement reservation short of its queue, a freshly created task before
    /// publication, a child mid-fork.
    ///
    /// The flag, not the count, is the witness that authorises taking the
    /// reference back, so the reclaim is exactly-once even under a race.
    pub existence_ref_parked: AtomicBool,
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

/// One class for every task's `test_reports`, shared by all three
/// constructors so a task's class does not depend on which one built it.
const TEST_REPORTS_CLASS: &crate::sync::lock_tracking::LockClassKey =
    crate::lock_class!("Task.test_reports", LOCK_LEVEL_RESOURCE);

impl<K, U> TaskInner<K, U> {
    /// This task's FS segment base (TLS pointer).
    ///
    /// Acquire/Release rather than Relaxed: the cross-CPU reader is the next
    /// `prepare_switch_to`, which reads the *incoming* task's copy.
    #[inline]
    pub fn fs_base(&self) -> u64 {
        self.fs_base.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_fs_base(&self, value: u64) {
        self.fs_base.store(value, Ordering::Release);
    }

    /// The per-task half of the FPU owner tag, or [`FPU_CPU_NONE`]; meaningful
    /// only in agreement with the per-CPU half, via
    /// [`fpu_state_valid`](crate::task::fpu_owner::fpu_state_valid).
    #[inline]
    pub fn fpu_last_cpu(&self) -> i32 {
        self.fpu_last_cpu.load(Ordering::Acquire)
    }

    /// Stamp the per-task half of the FPU owner tag.
    ///
    /// `pub(crate)`: a caller able to stamp one half alone could manufacture the
    /// agreement the tag exists to check. Outside OSTD the sanctioned moves are
    /// [`fpu_save_current`](Self::fpu_save_current) and
    /// [`fpu_restore_to_cpu`](Self::fpu_restore_to_cpu), which move both halves.
    #[inline]
    pub(crate) fn set_fpu_last_cpu(&self, cpu: i32) {
        self.fpu_last_cpu.store(cpu, Ordering::Release);
    }

    /// Capture this task's live FPU/vector registers into its save area and
    /// hand the register file back, stamping both halves of the owner tag so a
    /// call site cannot do one without the other.
    ///
    /// Eager by construction: no "save only if dirty" branch — lazy FPU
    /// switching is CVE-2018-3665.
    #[inline]
    pub fn fpu_save_current(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        fpu_owner_assert_may_take(self, cpu);
        // SAFETY: the witness proves exclusive access to this task's register
        // state, and `get_ptr` yields a `SharedReadWrite` derivation of the cell
        // — never a `&mut` — so a nested witness on the same task cannot
        // invalidate this pointer. XSAVE64's 64-byte alignment is pinned by the
        // razors beside `TaskOwnCell`.
        unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_ptr(witness), xcr0_mask) };
        fpu_owner_yield_after_save(self, cpu);
    }

    /// Load this task's saved FPU/vector state into the register file and take
    /// ownership of it. Mirror of [`fpu_save_current`](Self::fpu_save_current).
    ///
    /// Unconditional, and it must stay that way at this entry point: the owner
    /// tag records *which task* the register file belongs to, not whether the
    /// file still agrees with the save area. Signal delivery saves via
    /// [`fpu_save_in_place`](Self::fpu_save_in_place) and keeps ownership, so
    /// skipping the reload on a tag hit would discard the state `sigreturn`
    /// exists to reinstate.
    /// [`fpu_state_valid`](crate::task::fpu_owner::fpu_state_valid) is therefore
    /// an opt-in predicate, not a precondition of this function.
    ///
    /// Returns `false` if the hardware rejected the save area; it is reset to
    /// the init image and reloaded, so a later restore cannot fault the same way.
    #[must_use]
    #[inline]
    pub fn fpu_restore_to_cpu(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) -> bool {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        let slot = self.fpu_state.get_ptr(witness);
        // No precondition: a restore defines the register file's new contents,
        // and is not always preceded by a save on this CPU.
        // SAFETY: as `fpu_save_current`; XRSTOR64 only reads the buffer.
        let accepted = unsafe { crate::task::fpu::fpu_xrstor(slot.cast_const(), xcr0_mask) };
        if !accepted {
            // SAFETY: the witness proves exclusive access, and
            // `fpu_reset_in_place` writes a whole valid `FpuState`.
            let repaired = unsafe {
                fpu_reset_in_place(slot);
                crate::task::fpu::fpu_xrstor(slot.cast_const(), xcr0_mask)
            };
            // The init image satisfies every rule XRSTOR64 checks; a rejection
            // here leaves the register file in the undefined state the first
            // fault produced, which nothing downstream can repair.
            debug_assert!(repaired, "XRSTOR64 rejected the FPU init image");
        }
        fpu_owner_take(self, cpu);
        accepted
    }

    /// Capture the live registers into the save area **without** handing the
    /// register file back — the signal-frame save and the fork flush, both of
    /// which keep executing afterwards, so the owner tag must keep naming this
    /// task.
    ///
    /// Disables interrupts because the sequence — read this CPU's index,
    /// `XSAVE`, stamp both halves of the owner tag against that index — is not
    /// migration-atomic: a reschedule in the middle stamps the CPU the task
    /// left, and the next save on that CPU trips
    /// [`fpu_owner_assert_may_take`]. The switch-out saves need no guard,
    /// because the scheduler already holds interrupts off across the switch.
    #[inline]
    pub fn fpu_save_in_place(&self, witness: &impl TaskExclusive<K, U>, xcr0_mask: u64) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        crate::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
            let cpu = crate::task::fpu_owner::fpu_current_cpu();
            fpu_owner_assert_may_take(self, cpu);
            // SAFETY: as `fpu_save_current`.
            unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_ptr(witness), xcr0_mask) };
            fpu_owner_take(self, cpu);
        });
    }

    /// `&mut self`-authorised counterpart to
    /// [`fpu_save_in_place`](Self::fpu_save_in_place), for the paths that
    /// already hold `&mut TaskInner`: minting a `CurrentTask` witness alongside
    /// that borrow would alias it. Maintains the owner tag.
    #[inline]
    pub fn fpu_save_in_place_mut(&mut self, xcr0_mask: u64) {
        // Migration-atomic for the reason spelled out on `fpu_save_in_place`.
        crate::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
            let cpu = crate::task::fpu_owner::fpu_current_cpu();
            fpu_owner_assert_may_take(self, cpu);
            // SAFETY: `&mut self` is exclusive access to the whole task.
            unsafe { crate::task::fpu::fpu_xsave(self.fpu_state.get_mut(), xcr0_mask) };
            fpu_owner_take(self, cpu);
        });
    }

    /// See [`fpu_save_in_place_mut`](Self::fpu_save_in_place_mut). Repairs a
    /// rejected save area as [`fpu_restore_to_cpu`](Self::fpu_restore_to_cpu) does.
    #[must_use]
    #[inline]
    pub fn fpu_restore_to_cpu_mut(&mut self, xcr0_mask: u64) -> bool {
        let cpu = crate::task::fpu_owner::fpu_current_cpu();
        let slot: *mut FpuState = self.fpu_state.get_mut();
        // Restore side takes no precondition — see `fpu_restore_to_cpu`.
        // SAFETY: `&mut self` is exclusive access to the whole task.
        let accepted = unsafe { crate::task::fpu::fpu_xrstor(slot, xcr0_mask) };
        if !accepted {
            // SAFETY: as above; `fpu_reset_in_place` writes a whole valid
            // `FpuState` into the slot.
            let repaired = unsafe {
                fpu_reset_in_place(slot);
                crate::task::fpu::fpu_xrstor(slot, xcr0_mask)
            };
            debug_assert!(repaired, "XRSTOR64 rejected the FPU init image");
        }
        fpu_owner_take(self, cpu);
        accepted
    }

    /// Borrow this task's FPU save area as bytes. The signal-frame paths copy it
    /// to and from user memory and cannot stage 2.6 KiB through a stack buffer
    /// the 2 KiB frame gate would reject.
    ///
    /// `f` must not itself take a witness on this task and call back in, or the
    /// `&mut` handed out here would alias.
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
    /// masked, XSTATE header zeroed). The *save area* only.
    ///
    /// A caller that also means to redefine the live registers — execve, where
    /// the old image's vector state must not survive into the new one — must
    /// pair it with [`fpu_restore_to_cpu`](Self::fpu_restore_to_cpu) under one
    /// IRQ-off window, or a context switch can re-save the old image's live
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

    /// This task's user-mode register snapshot.
    ///
    /// Shared rather than exclusive: the state lives in a cell, so a shared
    /// borrow is all any writer needs, and two witnesses on one task — an
    /// interrupt handler above a syscall — may legitimately coexist.
    #[inline]
    pub fn user_ctx(&self, witness: &impl TaskExclusive<K, U>) -> &UserContext {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        // SAFETY: the cell lives inside `self`, so `&self` bounds the borrow to
        // the storage. The witness proves this CPU owns the task's register
        // state, and the pointer comes from an `UnsafeCell`, so it is non-null,
        // aligned and initialised, and shared derivations compose.
        unsafe { &*self.user_ctx.get_ptr(witness) }
    }

    /// Save `frame`'s register state into this task's context and stamp the
    /// user-mode entry flags. The preempt path takes the trap frame of the task
    /// it is already running, so its `CurrentTask` guard is the proof.
    pub fn save_from_interrupt_frame(
        &self,
        witness: &impl TaskExclusive<K, U>,
        frame: &crate::irq::interrupt_frame::InterruptFrame,
        mark_user_started: bool,
    ) {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        // SAFETY: the witness proves exclusive access to this task's register
        // state for the duration of the call.
        write_trap_frame(unsafe { &mut *self.context.get_ptr(witness) }, frame);
        self.mark_user_entry(mark_user_started);
    }

    /// [`save_from_interrupt_frame`](Self::save_from_interrupt_frame) proven by
    /// `&mut self` rather than by a witness — the sole-owner path.
    pub fn save_from_interrupt_frame_mut(
        &mut self,
        frame: &crate::irq::interrupt_frame::InterruptFrame,
        mark_user_started: bool,
    ) {
        write_trap_frame(self.context.get_mut(), frame);
        self.mark_user_entry(mark_user_started);
    }

    #[inline]
    fn mark_user_entry(&self, mark_user_started: bool) {
        self.context_from_user.store(1, Ordering::Relaxed);
        if mark_user_started {
            self.user_started.store(1, Ordering::Relaxed);
        }
    }

    /// This task's saved callee-saved register frame. Raw because it feeds
    /// [`switch_registers`](crate::task::switch_registers), whose asm takes
    /// both endpoints as pointers.
    #[inline]
    pub fn switch_ctx_ptr(&self, witness: &impl TaskExclusive<K, U>) -> *mut SwitchContext {
        debug_assert!(
            core::ptr::eq(witness.witnessed(), self),
            "witness names a different task"
        );
        self.switch_ctx.get_ptr(witness)
    }

    /// Replace the working directory; the length is published after the bytes.
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

    #[inline]
    pub fn signal_blocked(&self) -> SigSet {
        self.signal_blocked.load(Ordering::Acquire)
    }

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

    #[inline]
    pub fn set_controlling_tty(&self, tty: Option<TtyIndex>) {
        let raw = tty.map_or(TTY_INDEX_NONE, |t| u16::from(t.0));
        self.controlling_tty.store(raw, Ordering::Release);
    }

    /// Clear the controlling terminal only if it currently names `tty`,
    /// reporting whether this call did the clearing. Compare-and-clear rather
    /// than load-then-store so a session teardown racing a task that has already
    /// moved to a different terminal cannot clear the new one.
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

    /// Id of this task's parent, or `INVALID_TASK_ID` once reparented. Relaxed:
    /// an identity scalar, ordered against the children-list membership it
    /// mirrors by that list's own lock.
    #[inline]
    pub fn parent_task_id(&self) -> u32 {
        self.parent_task_id.load(Ordering::Relaxed)
    }

    /// See [`parent_task_id`](Self::parent_task_id).
    #[inline]
    pub fn set_parent_task_id(&self, id: u32) {
        self.parent_task_id.store(id, Ordering::Relaxed);
    }

    /// Packed handle to this task's address space, or 0 for none.
    /// Acquire/Release: published once, before the task is reachable, and read
    /// on another CPU's dispatch and fault paths.
    #[inline]
    pub fn process_vm_handle_raw(&self) -> u64 {
        self.process_vm_handle.load(Ordering::Acquire)
    }

    /// See [`process_vm_handle_raw`](Self::process_vm_handle_raw).
    #[inline]
    pub fn set_process_vm_handle_raw(&self, packed: u64) {
        self.process_vm_handle.store(packed, Ordering::Release);
    }

    /// Packed handle to this task's process, or 0 for none. Acquire/Release for
    /// the reason `process_vm_handle` is.
    #[inline]
    pub fn process_handle_raw(&self) -> u64 {
        self.process_handle.load(Ordering::Acquire)
    }

    /// See [`process_handle_raw`](Self::process_handle_raw).
    #[inline]
    pub fn set_process_handle_raw(&self, packed: u64) {
        self.process_handle.store(packed, Ordering::Release);
    }

    /// The [`Process`](crate::process::Process) this task belongs to. `None` for
    /// a kernel task, and for a user task whose process has been reaped out from
    /// under a stale handle — returned instead of a stranger's process.
    #[inline]
    pub fn process(&self) -> Option<crate::KArc<crate::process::Process>> {
        crate::process::process_for_handle(crate::process::unpack_process_handle(
            self.process_handle_raw(),
        )?)
    }

    /// Bitmask of CPUs this task may run on. Relaxed: an affinity change races
    /// dispatch by nature, and the loser of that race is repatriated on the
    /// task's next switch-out.
    #[inline]
    pub fn cpu_affinity(&self) -> u32 {
        self.cpu_affinity.load(Ordering::Relaxed)
    }

    /// See [`cpu_affinity`](Self::cpu_affinity).
    #[inline]
    pub fn set_cpu_affinity(&self, mask: u32) {
        self.cpu_affinity.store(mask, Ordering::Relaxed);
    }

    /// This task's full scheduling quantum, in timer ticks. Relaxed throughout:
    /// a tick lost or duplicated across the reset/decrement race costs one tick
    /// of scheduling fairness rather than correctness.
    #[inline]
    pub fn time_slice(&self) -> u64 {
        self.time_slice.load(Ordering::Relaxed)
    }

    /// See [`time_slice`](Self::time_slice).
    #[inline]
    pub fn set_time_slice(&self, ticks: u64) {
        self.time_slice.store(ticks, Ordering::Relaxed);
    }

    /// Ticks left in this task's current quantum.
    #[inline]
    pub fn time_slice_remaining(&self) -> u64 {
        self.time_slice_remaining.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_time_slice_remaining(&self, ticks: u64) {
        self.time_slice_remaining.store(ticks, Ordering::Relaxed);
    }

    #[inline]
    pub fn last_run_timestamp(&self) -> u64 {
        self.last_run_timestamp.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_last_run_timestamp(&self, timestamp: u64) {
        self.last_run_timestamp.store(timestamp, Ordering::Release);
    }

    /// CPU this task last ran on.
    #[inline]
    pub fn last_cpu(&self) -> u8 {
        self.last_cpu.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_last_cpu(&self, cpu: u8) {
        self.last_cpu.store(cpu, Ordering::Release);
    }

    /// Debug tripwire that nothing still claims to own this task at reclaim.
    ///
    /// The intrusive lists have no `Drop`, so a non-empty `children` list would
    /// silently leak every parked child reference and a still-linked
    /// `sibling_link` would leave a parent's list naming this task.
    ///
    /// Factored out of `Drop` so the destructor body carries no literal panic
    /// op; `debug_assert!` compiles out of release, so it is panic-free there.
    #[inline]
    fn assert_no_owner_holds_this_task(&self) {
        // The count cannot reach zero while the task still holds its own
        // reference: seeing this means a reap released it without clearing the
        // flag, or a copy inherited it.
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
        // A still-linked scheduler slot means an unbalanced retain/reclaim: the
        // container's parked reference leaked.
        debug_assert!(
            !self.ready_link.is_linked(),
            "task dropped while still linked into a ready queue"
        );
        debug_assert!(
            !self.remote_inbox_link.is_linked(),
            "task dropped while still linked into a remote wake inbox"
        );
        debug_assert!(
            !self.futex_link.is_linked(),
            "task dropped while still linked into a futex bucket"
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
            process_vm_handle: AtomicU64::new(0),
            process_handle: AtomicU64::new(crate::process::PROCESS_HANDLE_NONE),
            process_group: RcuArcSlot::empty(),
            test_reports: SpinLock::new(None, TEST_REPORTS_CLASS),
            parent_task_id: AtomicU32::new(INVALID_TASK_ID),
            fs_base: AtomicU64::new(0),
            tgid: INVALID_TASK_ID,
            pgid: AtomicU32::new(INVALID_TASK_ID),
            sid: AtomicU32::new(INVALID_TASK_ID),
            controlling_tty: AtomicU16::new(TTY_INDEX_NONE),
            cwd: TaskOwnCell::new({
                let mut c = [0u8; CWD_MAX];
                c[0] = b'/';
                c
            }),
            cwd_len: AtomicU16::new(1),
            clear_child_tid: AtomicU64::new(0),
            time_slice: AtomicU64::new(0),
            time_slice_remaining: AtomicU64::new(0),
            total_runtime: AtomicU64::new(0),
            creation_time: 0,
            yield_count: AtomicU32::new(0),
            last_run_timestamp: AtomicU64::new(0),
            user_started: AtomicU8::new(0),
            context_from_user: AtomicU8::new(0),
            exit_reason: AtomicU16::new(TaskExitReason::None.as_u16()),
            fault_reason: AtomicU16::new(TaskFaultReason::None.as_u16()),
            exit_code: AtomicU32::new(0),
            fate_token: AtomicU32::new(0),
            fate_value: AtomicU32::new(0),
            fate_pending: AtomicU8::new(0),
            cpu_affinity: AtomicU32::new(0),
            last_cpu: AtomicU8::new(0),
            fpu_last_cpu: AtomicI32::new(FPU_CPU_NONE),
            migration_count: AtomicU32::new(0),
            caps: AtomicU64::new(CAPS_UNSET),
            signal_pending: AtomicU64::new(0),
            signal_blocked: AtomicU64::new(SIG_EMPTY),
            signal_actions: [const { SignalActionCell::default() }; NSIG],
            switch_ctx: TaskOwnCell::new(SwitchContext::zero()),
            on_cpu: AtomicBool::new(false),
            ready_link: Link::new(),
            remote_inbox_link: Link::new(),
            children: IntrusiveDList::new(),
            sibling_link: DLink::new(),
            futex_link: DLink::new(),
            futex_addr: AtomicU64::new(0),
            reclaim_link: Link::new(),
            sched_placement: AtomicU8::new(SchedPlacement::Nascent.as_u8()),
            parked_wait_queue: AtomicPtr::new(ptr::null_mut()),
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

    /// In-place `Init` recipe equivalent to [`TaskInner::invalid`], built
    /// field-by-field at the destination slot so no 3.8 KiB rvalue lands on the
    /// caller's stack.
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
                addr_of_mut!((*slot).parent_task_id).write(AtomicU32::new(INVALID_TASK_ID));
                addr_of_mut!((*slot).tgid).write(INVALID_TASK_ID);
                addr_of_mut!((*slot).pgid).write(AtomicU32::new(INVALID_TASK_ID));
                addr_of_mut!((*slot).sid).write(AtomicU32::new(INVALID_TASK_ID));

                // Not all-zero: zero would name TTY 0, not "no controlling terminal".
                addr_of_mut!((*slot).controlling_tty).write(AtomicU16::new(TTY_INDEX_NONE));

                // Also not all-zero: zero would name CPU 0 as holding this
                // task's vector state, letting a never-run task pass the FPU
                // owner agreement check on the boot CPU.
                addr_of_mut!((*slot).fpu_last_cpu).write(AtomicI32::new(FPU_CPU_NONE));

                addr_of_mut!((*slot).cwd_len).write(AtomicU16::new(1));
                // `TaskOwnCell` is `repr(transparent)`, so the cell's address
                // is the array's.
                (addr_of_mut!((*slot).cwd) as *mut u8).write(b'/');

                fpu_reset_in_place(addr_of_mut!((*slot).fpu_state).cast::<FpuState>());

                addr_of_mut!((*slot).kernel_stack).write(None);
                addr_of_mut!((*slot).unsafe_stack).write(None);
                addr_of_mut!((*slot).process_group).write(RcuArcSlot::empty());
                addr_of_mut!((*slot).caps).write(AtomicU64::new(CAPS_UNSET));
                addr_of_mut!((*slot).test_reports).write(SpinLock::new(None, TEST_REPORTS_CLASS));
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

    /// Publish a new status, keeping the current block reason. Returns `false`
    /// if the task is already terminal, and the caller must then take its own
    /// terminal path.
    #[inline]
    #[must_use = "a refused publish means the task is already dead; take the terminal path"]
    pub fn set_status(&self, status: TaskStatus) -> bool {
        let reason = self.state.reason();
        self.state.set_status_checked(status, reason)
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

    #[inline]
    pub fn try_transition_from(&self, expected: TaskStatus, target: TaskStatus) -> bool {
        if !expected.can_transition_to(target) {
            return false;
        }
        self.state
            .try_transition_keep_reason(expected, target)
            .is_ok()
    }

    /// The fused state word's ABA epoch, bumped on every state transition. A
    /// frozen epoch across observation windows distinguishes a genuinely
    /// stranded task from one merely caught mid-park by a racing scan.
    #[inline]
    pub fn state_epoch(&self) -> u32 {
        self.state.snapshot().epoch
    }

    /// Block from a specific expected state, stamping the reason in the same CAS.
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

    #[inline]
    pub fn store_block_reason(&self, reason: BlockReason) {
        self.state.store_reason(reason);
    }

    #[inline]
    pub fn terminate(&self) -> bool {
        self.try_transition_to(TaskStatus::Terminated)
    }

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

    #[inline]
    pub fn is_exited(&self) -> bool {
        matches!(self.status(), TaskStatus::Zombie | TaskStatus::Terminated)
    }

    /// Reset the per-run runtime bookkeeping on a newly allocated task: timing,
    /// exit/fault disposition, fate tokens, scheduler placement, the intrusive
    /// scheduler links, and a fresh creation timestamp.
    ///
    /// Drives both the task-create path and the fork path (after the child is
    /// bulk-copied from its parent), so every task starts neutral.
    pub fn reset_runtime_state(&mut self) {
        *self.time_slice_remaining.get_mut() = *self.time_slice.get_mut();
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
        self.parked_wait_queue
            .store(ptr::null_mut(), Ordering::Release);
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
                SpinLock::new(None, TEST_REPORTS_CLASS),
            );
            self.abi.unsafe_stack_sp = 0;
            core::ptr::write(&mut self.exit_info as *mut _, AtomicCell::empty());
            core::ptr::write(&mut self.state as *mut _, TaskState::invalid());
            // The bytewise copy duplicated the parent's `children` head/tail.
            // `IntrusiveDList` has no `Drop`, so `ptr::write` over the copied
            // bits is correct — a destructor must not run on them. Leaving the
            // copied `owner` back-pointer would make the child claim membership
            // in its parent's list, and an unlink would then corrupt that list.
            core::ptr::write(&mut self.children as *mut _, IntrusiveDList::new());
        }
        self.ready_link.reset();
        self.remote_inbox_link.reset();
        self.sibling_link.reset();
        self.reclaim_link.reset();
        // A fresh child starts parentless; the spawn path publishes the real
        // parent edge via `link_child` after registration.
        self.set_parent_task_id(INVALID_TASK_ID);
        // A `CLONE_VM` thread joins the parent's process and a forked child gets
        // its own, so the copied value is right in one case and wrong in the
        // other. Clearing makes a forgetful call site produce a task with no
        // process, which fails visibly, rather than one silently uncounted by
        // the process whose teardown count it belongs to.
        self.process_handle = AtomicU64::new(crate::process::PROCESS_HANDLE_NONE);
        self.sched_placement = AtomicU8::new(SchedPlacement::Nascent.as_u8());
        // A fresh child is parked on nothing; inheriting the copied back-pointer
        // would aim the parent's teardown purge at a queue it never joined.
        self.parked_wait_queue = AtomicPtr::new(ptr::null_mut());
        self.recovery_depth = AtomicU32::new(0);
        self.exit_cleanup_flags = AtomicU8::new(0);
        // Authority copies — the child is the same principal — but written
        // explicitly, because an omission from this list is invisible in review
        // and is how an entitlement leaks into a child.
        self.caps = AtomicU64::new(other.caps.load(Ordering::Acquire));
        self.signal_pending = AtomicU64::new(0);
        // A child is handed its own existence reference at registration;
        // inheriting the parent's `true` would let its reap take back a
        // reference never given, dropping the count below what owners hold.
        self.existence_ref_parked = AtomicBool::new(false);
        // The child's vector state has never been live in any register file, so
        // inheriting a CPU index would let it agree with a slot that names the
        // *parent* and skip a restore it genuinely needs.
        self.fpu_last_cpu = AtomicI32::new(FPU_CPU_NONE);
    }
}

// `Linked<Role>` comes from OSTD's blanket
// `unsafe impl<T: LinkProvider<R>, R> Linked<R> for T`; only the safe
// `LinkProvider` impls live here, returning a distinct field per role.

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

impl<K, U> crate::task::DLinkProvider<FutexRole> for TaskInner<K, U> {
    fn dlink(&self) -> &DLink<Self, FutexRole> {
        &self.futex_link
    }
}

impl<K, U> crate::task::LinkProvider<ReclaimRole> for TaskInner<K, U> {
    fn link(&self) -> &Link<Self, ReclaimRole> {
        &self.reclaim_link
    }
}

/// Copy an interrupted user-mode frame into a saved task context, forcing the
/// segment selectors to the user-data descriptors the resume path expects.
fn write_trap_frame(ctx: &mut TaskContext, frame: &crate::irq::interrupt_frame::InterruptFrame) {
    use crate::arch::x86_64::gdt::SegmentSelector;
    ctx.rax = frame.rax;
    ctx.rbx = frame.rbx;
    ctx.rcx = frame.rcx;
    ctx.rdx = frame.rdx;
    ctx.rsi = frame.rsi;
    ctx.rdi = frame.rdi;
    ctx.rbp = frame.rbp;
    ctx.r8 = frame.r8;
    ctx.r9 = frame.r9;
    ctx.r10 = frame.r10;
    ctx.r11 = frame.r11;
    ctx.r12 = frame.r12;
    ctx.r13 = frame.r13;
    ctx.r14 = frame.r14;
    ctx.r15 = frame.r15;
    ctx.rip = frame.rip;
    ctx.rsp = frame.rsp;
    ctx.rflags = frame.rflags;
    ctx.cs = frame.cs;
    ctx.ss = frame.ss;
    ctx.ds = SegmentSelector::USER_DATA.bits() as u64;
    ctx.es = SegmentSelector::USER_DATA.bits() as u64;
    ctx.fs = 0;
    ctx.gs = 0;
}
