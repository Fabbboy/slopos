//! Syscall number definitions (kernel-userland ABI)
//!
//! Single source of truth for all syscall numbers. Both kernel and userland
//! should import from this module to ensure ABI consistency.

// =============================================================================
// Core syscalls
// =============================================================================

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_FB_INFO: u64 = 6;

// =============================================================================
// Random / Roulette
// =============================================================================

pub const SYSCALL_RANDOM_NEXT: u64 = 12;
pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
pub const SYSCALL_ROULETTE_DRAW: u64 = 24;

// =============================================================================
// Filesystem
// =============================================================================

pub const SYSCALL_FS_OPEN: u64 = 14;
pub const SYSCALL_FS_CLOSE: u64 = 15;
pub const SYSCALL_FS_READ: u64 = 16;
pub const SYSCALL_FS_WRITE: u64 = 17;
pub const SYSCALL_FS_STAT: u64 = 18;
pub const SYSCALL_FS_MKDIR: u64 = 19;
pub const SYSCALL_FS_UNLINK: u64 = 20;
pub const SYSCALL_FS_LIST: u64 = 21;

// =============================================================================
// System
// =============================================================================

pub const SYSCALL_SYS_INFO: u64 = 22;
pub const SYSCALL_HALT: u64 = 23;
pub const SYSCALL_READ_CHAR: u64 = 25;
pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
pub const SYSCALL_GET_TIME_MS: u64 = 39;

// =============================================================================
// Window management
// =============================================================================

pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;
pub const SYSCALL_SET_WINDOW_POSITION: u64 = 31;
pub const SYSCALL_SET_WINDOW_STATE: u64 = 32;
pub const SYSCALL_RAISE_WINDOW: u64 = 33;

// =============================================================================
// Input events
// =============================================================================

pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_INPUT_POLL: u64 = 60;
pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;
pub const SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET: u64 = 65;
pub const SYSCALL_INPUT_GET_POINTER_POS: u64 = 66;
pub const SYSCALL_INPUT_GET_BUTTON_STATE: u64 = 67;

// =============================================================================
// Surface / Compositor
// =============================================================================

pub const SYSCALL_SURFACE_COMMIT: u64 = 38;
pub const SYSCALL_SURFACE_ATTACH: u64 = 44;
pub const SYSCALL_SURFACE_FRAME: u64 = 50;
pub const SYSCALL_POLL_FRAME_DONE: u64 = 51;
pub const SYSCALL_MARK_FRAMES_DONE: u64 = 52;
pub const SYSCALL_SURFACE_DAMAGE: u64 = 55;
pub const SYSCALL_BUFFER_AGE: u64 = 56;
pub const SYSCALL_SURFACE_SET_ROLE: u64 = 57;
pub const SYSCALL_SURFACE_SET_PARENT: u64 = 58;
pub const SYSCALL_SURFACE_SET_REL_POS: u64 = 59;
pub const SYSCALL_SURFACE_SET_TITLE: u64 = 63;

// =============================================================================
// Shared memory
// =============================================================================

pub const SYSCALL_SHM_CREATE: u64 = 40;
pub const SYSCALL_SHM_MAP: u64 = 41;
pub const SYSCALL_SHM_UNMAP: u64 = 42;
pub const SYSCALL_SHM_DESTROY: u64 = 43;
pub const SYSCALL_FB_FLIP: u64 = 45;
pub const SYSCALL_DRAIN_QUEUE: u64 = 46;
pub const SYSCALL_SHM_ACQUIRE: u64 = 47;
pub const SYSCALL_SHM_RELEASE: u64 = 48;
pub const SYSCALL_SHM_POLL_RELEASED: u64 = 49;
pub const SYSCALL_SHM_GET_FORMATS: u64 = 53;
pub const SYSCALL_SHM_CREATE_WITH_FORMAT: u64 = 54;

// =============================================================================
// Task management
// =============================================================================

pub const SYSCALL_SPAWN_TASK: u64 = 64;

// =============================================================================
// Process execution
// =============================================================================

/// Execute an ELF binary from the filesystem, replacing the current process.
///
/// # Arguments (via registers)
/// * rdi (arg0): Pointer to null-terminated path string
/// * rsi (arg1): Pointer to null-terminated argv array (or NULL)
/// * rdx (arg2): Pointer to null-terminated envp array (or NULL)
///
/// # Returns
/// * Does not return on success (process image is replaced)
/// * -ENOENT: File not found
/// * -ENOEXEC: Not a valid ELF executable
/// * -ENOMEM: Insufficient memory
/// * -EFAULT: Invalid pointer
pub const SYSCALL_EXEC: u64 = 70;

// =============================================================================
// Memory management
// =============================================================================

pub const SYSCALL_BRK: u64 = 71;

// =============================================================================
// Syscall data structures
// =============================================================================

/// System information returned by SYSCALL_SYS_INFO
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
}
