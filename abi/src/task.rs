//! Task-related types and constants shared between kernel subsystems.
//!
//! This module contains the canonical definitions for task management,
//! eliminating duplicate definitions across sched, drivers, boot, and mm crates.

use core::ffi::c_void;
use core::ptr;

// =============================================================================
// Task Configuration Constants
// =============================================================================

pub const MAX_TASKS: usize = 32;
pub const TASK_STACK_SIZE: u64 = 0x8000; // 32KB
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000; // 32KB
pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

// =============================================================================
// Task State Constants
// =============================================================================

pub const TASK_STATE_INVALID: u8 = 0;
pub const TASK_STATE_READY: u8 = 1;
pub const TASK_STATE_RUNNING: u8 = 2;
pub const TASK_STATE_BLOCKED: u8 = 3;
pub const TASK_STATE_TERMINATED: u8 = 4;

// =============================================================================
// Task Priority Constants
// =============================================================================

pub const TASK_PRIORITY_HIGH: u8 = 0;
pub const TASK_PRIORITY_NORMAL: u8 = 1;
pub const TASK_PRIORITY_LOW: u8 = 2;
pub const TASK_PRIORITY_IDLE: u8 = 3;

// =============================================================================
// Task Flag Constants
// =============================================================================

pub const TASK_FLAG_USER_MODE: u16 = 0x01;
pub const TASK_FLAG_FPU_INITIALIZED: u16 = 0x40;
pub const TASK_FLAG_KERNEL_MODE: u16 = 0x02;
pub const TASK_FLAG_NO_PREEMPT: u16 = 0x04;
pub const TASK_FLAG_SYSTEM: u16 = 0x08;
pub const TASK_FLAG_COMPOSITOR: u16 = 0x10;
pub const TASK_FLAG_DISPLAY_EXCLUSIVE: u16 = 0x20;

// =============================================================================
// TaskContext - CPU register state for context switching
// =============================================================================

/// CPU register state saved during context switches.
/// Size: 200 bytes (0xC8) - 25 x 8-byte registers
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
    /// Create a zeroed TaskContext.
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
// FpuState - FPU/SSE register state for context switching
// =============================================================================

pub const FPU_STATE_SIZE: usize = 512;
pub const MXCSR_DEFAULT: u32 = 0x1F80;

// FXSAVE area offsets (Intel SDM Vol. 1, Table 10-2)
const FXSAVE_FCW_OFFSET: usize = 0;
const FXSAVE_MXCSR_OFFSET: usize = 24;

/// FXSAVE area for x87/MMX/SSE state. Must be 16-byte aligned.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpuState {
    pub data: [u8; FPU_STATE_SIZE],
}

impl FpuState {
    pub const fn zero() -> Self {
        Self {
            data: [0u8; FPU_STATE_SIZE],
        }
    }

    /// Initialize with default FCW (0x037F) and MXCSR (0x1F80) - all exceptions masked.
    pub const fn new() -> Self {
        let mut state = Self::zero();
        // FCW: exceptions masked, 64-bit precision, round-to-nearest
        state.data[FXSAVE_FCW_OFFSET] = 0x7F;
        state.data[FXSAVE_FCW_OFFSET + 1] = 0x03;
        // MXCSR: SSE exceptions masked, round-to-nearest
        state.data[FXSAVE_MXCSR_OFFSET] = 0x80;
        state.data[FXSAVE_MXCSR_OFFSET + 1] = 0x1F;
        state
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
}

impl Default for FpuState {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Task Exit/Fault Reason Enums
// =============================================================================

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

// =============================================================================
// Task Struct Layout Constants (for assembly access)
// =============================================================================

// Offset from &Task.context to &Task.fpu_state
// TaskContext is 200 bytes (packed), FpuState needs 16-byte alignment
// So there's 8 bytes padding: 200 + 8 = 208 (0xD0)
pub const TASK_FPU_OFFSET_FROM_CONTEXT: usize = 0xD0;

// =============================================================================
// Task Struct
// =============================================================================

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    pub state: u8,
    pub priority: u8,
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
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub waiting_on_task_id: u32,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
    pub next_ready: *mut Task,
}

impl Task {
    /// Create an invalid (uninitialized) task.
    pub const fn invalid() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            name: [0; TASK_NAME_MAX_LEN],
            state: TASK_STATE_INVALID,
            priority: TASK_PRIORITY_NORMAL,
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
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: 0,
            creation_time: 0,
            yield_count: 0,
            last_run_timestamp: 0,
            waiting_on_task_id: INVALID_TASK_ID,
            user_started: 0,
            context_from_user: 0,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
            fate_token: 0,
            fate_value: 0,
            fate_pending: 0,
            next_ready: ptr::null_mut(),
        }
    }
}

// =============================================================================
// TaskExitRecord - for tracking task termination
// =============================================================================

/// Record of task termination for post-mortem inspection.
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

// =============================================================================
// IdtEntry - Interrupt Descriptor Table entry
// =============================================================================

/// x86-64 IDT (Interrupt Descriptor Table) entry.
/// Used for setting up interrupt and exception handlers.
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct IdtEntry {
    pub offset_low: u16,
    pub selector: u16,
    pub ist: u8,
    pub type_attr: u8,
    pub offset_mid: u16,
    pub offset_high: u32,
    pub zero: u32,
}

impl IdtEntry {
    /// Create a zeroed IDT entry.
    pub const fn zero() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            zero: 0,
        }
    }
}
