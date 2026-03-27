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
pub use slopos_abi::syscall::{Timespec, UserCpuInfo, UserPerCpuStats, UserSysInfo, UserTaskEntry};
pub use slopos_abi::{
    DamageRect, DisplayInfo, INPUT_FOCUS_KEYBOARD, INPUT_FOCUS_POINTER, InputEvent, InputEventData,
    InputEventType, MAX_WINDOW_DAMAGE_REGIONS, PixelFormat, SHM_ACCESS_RO, SHM_ACCESS_RW, ShmError,
    SockAddrIn, SurfaceRole, USER_NET_MAX_MEMBERS, UserFsEntry, UserFsList, UserFsStat,
    UserNetInfo, UserNetMember, WindowInfo,
};

pub use wrappers::shm::{CachedShmMapping, ShmBuffer, ShmBufferRef};

pub type UserWindowInfo = WindowInfo;
pub type RawFd = i32;

/// Owned file descriptor — closes automatically on drop.
///
/// This is the fd analog of `Box<T>`: it owns the resource and releases it
/// when it goes out of scope.  NOT `Copy`, NOT `Clone` — you can't
/// accidentally duplicate an fd or use one after close.
///
/// Use `.raw()` to borrow the fd number for syscalls that need `i32`.
/// Use `.into_raw()` to take ownership without closing (e.g. after `dup2`
/// to a well-known slot like stdin).
pub struct OwnedFd(RawFd);

impl OwnedFd {
    /// Wrap a raw fd number into an owned handle.
    ///
    /// # Safety contract
    /// The caller must ensure `fd` is a valid, open file descriptor that
    /// is not owned by any other `OwnedFd`.
    pub fn from_raw(fd: RawFd) -> Self {
        Self(fd)
    }

    /// Borrow the raw fd number for passing to syscalls.
    pub fn raw(&self) -> RawFd {
        self.0
    }

    /// Consume the `OwnedFd` WITHOUT closing.  The caller takes
    /// responsibility for the fd's lifetime.
    pub fn into_raw(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            let _ = super::syscall::fs::close_fd_raw(self.0);
        }
    }
}
