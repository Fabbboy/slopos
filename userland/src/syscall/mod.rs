//! Unified syscall module for SlopOS userland.
//!
//! This module provides a clean, layered API for issuing system calls:
//!
//! - **Layer 1** (`raw`): Inline assembly primitives
//! - **Layer 2** (`error`): Error demultiplexing and `SyscallResult` type
//! - **Layer 3** (domain modules): Syscall wrappers organized by function
//!   - `fs`: Returns `SyscallResult<T>` for proper error handling
//!   - `tty`: Returns raw `i64` (fire-and-forget console I/O)
//!   - Others: Mix based on use case
//! - **Layer 4** (`wrappers`): RAII wrappers for resources
//!
//! # Module Organization
//!
//! | Module | Purpose |
//! |--------|---------|
//! | `raw` | Low-level inline asm syscall primitives |
//! | `error` | `SyscallError`, `SyscallResult`, `demux()` |
//! | `numbers` | Re-exports syscall numbers from `slopos_abi` |
//! | `core` | Yield, exit, sleep, time, CPU info |
//! | `tty` | TTY/console I/O (not file descriptors!) |
//! | `fs` | File descriptor operations |
//! | `memory` | brk, sbrk, shared memory |
//! | `process` | spawn by path, exec, fork, halt, reboot |
//! | `window` | Framebuffer, surface, window management |
//! | `input` | Input events, pointer, keyboard |
//! | `roulette` | Wheel of Fate syscalls |
//! | `wrappers` | RAII types (ShmBuffer) |

pub mod core;
pub mod error;
pub mod fs;
pub mod input;
pub mod memory;
pub mod net;
pub mod numbers;
pub mod process;
pub mod raw;
pub mod roulette;
pub mod tty;
pub mod window;
pub mod wrappers;

// Re-export commonly used items at the module root
pub use error::{SyscallError, SyscallResult};
pub use numbers::*;

// Re-export ABI types used by syscalls
pub use slopos_abi::syscall::{Timespec, UserSysInfo};
pub use slopos_abi::{
    DamageRect, DisplayInfo, INPUT_FOCUS_KEYBOARD, INPUT_FOCUS_POINTER, InputEvent, InputEventData,
    InputEventType, MAX_WINDOW_DAMAGE_REGIONS, PixelFormat, SHM_ACCESS_RO, SHM_ACCESS_RW, ShmError,
    SockAddrIn, SurfaceRole, USER_FS_OPEN_APPEND, USER_FS_OPEN_CREAT, USER_FS_OPEN_READ,
    USER_FS_OPEN_WRITE, USER_NET_MAX_MEMBERS, UserFsEntry, UserFsList, UserFsStat, UserNetInfo,
    UserNetMember, WindowInfo,
};

pub use wrappers::shm::{CachedShmMapping, ShmBuffer, ShmBufferRef};

pub type UserWindowInfo = WindowInfo;
pub type RawFd = i32;
