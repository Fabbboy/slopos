//! Unified syscall module for SlopOS userland: `raw` inline-asm primitives,
//! `error` demultiplexing into `SyscallResult`, domain modules on top (`fs`
//! returns `SyscallResult`, `tty` raw `i64`), and `wrappers` for RAII types.

pub mod core;
pub mod error;
pub mod fs;
pub mod input;
pub mod keymap;
pub mod memory;
pub mod net;
pub mod numbers;
pub mod pidfd;
pub mod process;
pub mod raw;
pub mod ring;
pub mod roulette;
pub mod signalfd;
pub mod tty;
pub mod window;
pub mod wrappers;

pub use error::{SyscallError, SyscallResult};
pub use numbers::*;

pub use slopos_abi::syscall::{Timespec, UserCpuInfo, UserPerCpuStats, UserSysInfo, UserTaskEntry};
pub use slopos_abi::{
    DamageRect, DisplayInfo, InputEvent, InputEventData, InputEventType, MAX_WINDOW_DAMAGE_REGIONS,
    MemfdError, PixelFormat, SockAddrIn, UserFsEntry, UserFsList, UserFsStat, WindowInfo,
};

pub use wrappers::memfd_buf::{CachedShmMapping, ShmBuffer};

pub type UserWindowInfo = WindowInfo;
pub type RawFd = i32;

/// Owned file descriptor — closes automatically on drop.
pub struct OwnedFd(RawFd);

impl OwnedFd {
    /// # Safety contract
    /// The caller must ensure `fd` is a valid, open file descriptor that
    /// is not owned by any other `OwnedFd`.
    pub unsafe fn from_raw(fd: RawFd) -> Self {
        Self(fd)
    }

    pub fn raw(&self) -> RawFd {
        self.0
    }

    /// Consumes the `OwnedFd` WITHOUT closing; the caller takes over the fd's
    /// lifetime.
    pub fn into_raw(self) -> RawFd {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            let _ = fs::close_fd_raw(self.0);
        }
    }
}
