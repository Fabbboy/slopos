//! Task ABI types shared between kernel and userland.
//!
//! This module contains **only** the types, constants, and enums that form the
//! stable interface between kernel subsystems. Kernel-internal implementation
//! details (Task struct, register contexts, FPU state, scheduler linkage) live
//! in `slopos_core::scheduler::task_struct`.

// --- Task Configuration ---

pub const MAX_TASKS: usize = 256;
pub const TASK_STACK_SIZE: u64 = 0x8000; // 32 KiB
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000; // 32 KiB
pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

// --- TaskStatus ---

/// Type-safe task status with explicit state-machine semantics.
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
    /// Task has terminated and is awaiting cleanup.
    Terminated = 4,
    /// Task declared intent to block, but has not blocked yet.
    WillBlock = 5,
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
            5 => Self::WillBlock,
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
            Self::Ready => matches!(target, Self::Running | Self::Terminated | Self::WillBlock),
            Self::Running => {
                matches!(
                    target,
                    Self::Ready | Self::Blocked | Self::Terminated | Self::WillBlock
                )
            }
            Self::WillBlock => {
                matches!(
                    target,
                    Self::Blocked | Self::Running | Self::Ready | Self::Terminated
                )
            }
            Self::Blocked => matches!(target, Self::Ready | Self::Terminated),
            Self::Terminated => matches!(target, Self::Invalid | Self::Terminated),
        }
    }
}

// --- BlockReason ---

/// Reason why a task is in the Blocked state.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlockReason {
    #[default]
    None = 0,
    /// Target task ID stored in `waiting_on`.
    WaitingOnTask = 1,
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
            1 => Self::WaitingOnTask,
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

// --- Task Priority ---

pub const TASK_PRIORITY_HIGH: u8 = 0;
pub const TASK_PRIORITY_NORMAL: u8 = 1;
pub const TASK_PRIORITY_LOW: u8 = 2;
pub const TASK_PRIORITY_IDLE: u8 = 3;

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
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskExitReason {
    #[default]
    None = 0,
    Normal = 1,
    UserFault = 2,
    Kernel = 3,
}

/// Specific fault that caused task termination.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
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
