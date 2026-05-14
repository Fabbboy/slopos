//! Task ABI types shared between kernel and userland.
//!
//! This module contains **only** the types, constants, and enums that form the
//! stable interface between kernel subsystems. Kernel-internal implementation
//! details (Task struct, register contexts, FPU state, scheduler linkage) live
//! in `slopos_core::scheduler::task_struct`.

// --- Task Configuration ---

/// Maximum number of concurrently live tasks.
///
/// Aligned with the kernel-stack VA region cap
/// (`mm::memory_layout_defs::KSTACK_MAX_SLOTS`): every live task
/// requires a KSTACK slot, so the task pool cannot usefully exceed
/// that number. Growing beyond this requires expanding the KSTACK VA
/// window.
///
/// The kernel's task pool (`core/scheduler/task/task_table.rs`) is
/// heap-backed and grows lazily — this constant is the upper bound,
/// not the initial resident set. Idle systems hold only a handful of
/// KBoxes regardless of this value.
pub const MAX_TASKS: usize = 8192;
/// Task kernel-mode stack size.
///
/// 32 KiB usable, backed by a 64 KiB slot (4 KiB guard + 32 KiB usable
/// + 28 KiB reserve).  The guard page turns kernel-stack overflow into
/// a deterministic page fault instead of silently corrupting adjacent
/// memory.
pub const TASK_STACK_SIZE: u64 = 0x8000; // 32 KiB
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000; // 32 KiB

/// SafeStack-sanitizer data stack size — 16 KiB.
///
/// LLVM's SafeStack pass moves address-taken locals and dynamic allocas
/// onto this stack at every instrumented function prologue. The
/// zero-`unsafe`-keyword refactors push more kernel-side primitives behind
/// `&mut`-passing safe helpers (`with_mut`, `for_each`, `frame_for_phys`,
/// `hhdm_*_bytes`, …); LLVM lowers each `&mut local` to an address-take
/// and the local migrates to the data stack. Cumulative depth on
/// long syscall paths (fork → COW → exec → load_segment_pages → …)
/// approaches the prior 8 KiB ceiling on slow TCG/CI hosts where dev
/// builds can't inline these helpers — the watchdog catches it as a
/// kernel-mode write past the mapped region. 16 KiB matches Linux's
/// x86_64 `THREAD_SIZE` and gives 2× headroom; the 8192-task ceiling
/// costs 128 MiB at peak (vs. 64 MiB at 8 KiB, vs. 256 MiB if we
/// sized identically to the safe kernel stack).
pub const TASK_UNSAFE_STACK_SIZE: u64 = 0x4000; // 16 KiB

pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

// --- TaskStatus ---

/// Type-safe task status with explicit state-machine semantics.
///
/// The pre-Phase-5 `WillBlock` variant — introduced as the
/// intermediate step in a `Running → WillBlock → Blocked` race-close
/// protocol — was deleted: the wait-queue protocol now CAS
/// `Running → Blocked` directly under the queue's SpinLock, and the
/// lock-pair against `wake_*` provides the same race-close guarantee
/// at lower complexity.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskStatus {
    /// Task slot is not in use.
    #[default]
    Invalid = 0,
    /// Task is ready to run, waiting in a run queue.
    Ready = 1,
    /// Task is currently executing on a CPU.
    Running = 2,
    /// Task is blocked waiting for some event.
    Blocked = 3,
    /// Task has terminated and is reapable. Slot is eligible for tier-2
    /// reuse once `ref_count == 0`.
    Terminated = 4,
    /// Task has exited but still holds its exit info awaiting a `waitpid`
    /// from a live parent. Tier-2 slot reuse skips Zombie slots so the
    /// parent's reaper observes a stable `Task::exit_info`.
    Zombie = 5,
}

impl TaskStatus {
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Invalid,
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Blocked,
            4 => Self::Terminated,
            5 => Self::Zombie,
            _ => Self::Invalid,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn can_transition_to(self, target: Self) -> bool {
        match self {
            Self::Invalid => matches!(target, Self::Ready),
            Self::Ready => matches!(target, Self::Running | Self::Terminated | Self::Zombie),
            Self::Running => matches!(
                target,
                Self::Ready | Self::Blocked | Self::Terminated | Self::Zombie
            ),
            Self::Blocked => matches!(target, Self::Ready | Self::Terminated | Self::Zombie),
            Self::Terminated => matches!(target, Self::Invalid | Self::Terminated),
            Self::Zombie => matches!(target, Self::Terminated | Self::Zombie),
        }
    }
}

// --- BlockReason ---

/// Reason why a task is in the Blocked state.
///
/// The pre-Phase-1 `WaitingOnTask` variant — paired with the now-deleted
/// per-task `waiting_on: AtomicU32` field — was retired when
/// `task_wait_for` migrated to the per-task `waiters: WaitQueue` +
/// durable `exit_info` cell. Phase 5 finished the cleanup by removing
/// the `waiting_on` field; the `BlockReason` discriminant for
/// `WaitingOnTask` is gone, and value `1` is reserved for future
/// reuse.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockReason {
    #[default]
    None = 0,
    Sleep = 2,
    IoWait = 3,
    MutexWait = 4,
    KeyboardWait = 5,
    IpcWait = 6,
    Generic = 7,
    FutexWait = 8,
}

impl BlockReason {
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::None,
            2 => Self::Sleep,
            3 => Self::IoWait,
            4 => Self::MutexWait,
            5 => Self::KeyboardWait,
            6 => Self::IpcWait,
            7 => Self::Generic,
            8 => Self::FutexWait,
            _ => Self::None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

// --- TaskPriority ---

/// Scheduler priority class. Lower numeric value = higher priority,
/// matching the order used by `dequeue_highest_priority`. The repr value
/// is the index into the per-CPU `ready_queues` array; adding a variant
/// requires bumping `NUM_PRIORITY_LEVELS` in the scheduler.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskPriority {
    /// Latency-critical work: compositor, RT kernel paths.
    High = 0,
    /// Default class for ordinary user tasks and kernel threads.
    #[default]
    Normal = 1,
    /// Background work that should yield to anything else.
    Low = 2,
    /// Per-CPU idle loop only — never used by user-spawned tasks.
    Idle = 3,
}

impl TaskPriority {
    /// Total decoder: out-of-range values coerce to `Normal`. Used for
    /// trusted kernel-internal reads (e.g. unmarshalling a `u8` field
    /// that the kernel itself wrote).
    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::High,
            1 => Self::Normal,
            2 => Self::Low,
            3 => Self::Idle,
            _ => Self::Normal,
        }
    }

    /// Strict decoder: returns `None` on out-of-range. Use at the
    /// syscall boundary to reject untrusted input instead of silently
    /// coercing it.
    #[inline]
    pub const fn try_from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::High),
            1 => Some(Self::Normal),
            2 => Some(Self::Low),
            3 => Some(Self::Idle),
            _ => None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn queue_index(self) -> usize {
        self as usize
    }
}

// --- Task Flags ---

pub const TASK_FLAG_USER_MODE: u16 = 0x01;
pub const TASK_FLAG_KERNEL_MODE: u16 = 0x02;
pub const TASK_FLAG_NO_PREEMPT: u16 = 0x04;
pub const TASK_FLAG_SYSTEM: u16 = 0x08;
pub const TASK_FLAG_COMPOSITOR: u16 = 0x10;
pub const TASK_FLAG_DISPLAY_EXCLUSIVE: u16 = 0x20;
pub const TASK_FLAG_FPU_INITIALIZED: u16 = 0x40;
/// Place the spawned task into its own process group (`pgid = task_id`)
/// instead of inheriting the parent's pgid.  Eliminates the SMP race
/// between spawn and the parent's `setpgid` + `tcsetpgrp` calls.
pub const TASK_FLAG_NEW_PGRP: u16 = 0x80;

// --- Task Exit/Fault Reason ---

/// Reason for task termination.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskExitReason {
    #[default]
    None = 0,
    Normal = 1,
    UserFault = 2,
    Kernel = 3,
}

/// Specific fault that caused task termination.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskFaultReason {
    #[default]
    None = 0,
    UserPage = 1,
    UserGp = 2,
    UserUd = 3,
    UserDeviceNa = 4,
}

// --- TaskExitRecord ---

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskExitRecord {
    pub task_id: u32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
}

impl TaskExitRecord {
    /// Create an empty exit record.
    pub const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
        }
    }
}
