//! Memory syscalls the `Ring` wrapper's `Drop` needs: `munmap` and `close`.

use slopos_abi::syscall::{SYSCALL_FS_CLOSE, SYSCALL_MUNMAP};
use slopos_slibc::pal::raw::{syscall1, syscall2};

#[inline(always)]
pub fn munmap(addr: u64, length: u64) -> i32 {
    unsafe { syscall2(SYSCALL_MUNMAP, addr, length) as i32 }
}

#[inline(always)]
pub fn close(fd: i32) -> i32 {
    unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) as i32 }
}
