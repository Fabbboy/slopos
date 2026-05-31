//! pidfd syscall wrapper.
//!
//! `pidfd_open(pid)` returns a `FileKind::Pidfd` fd that becomes
//! `POLLIN`-ready once the target child task exits — pollable via
//! [`fs::poll`](super::fs::poll) or SlopRing `OP_POLL_ADD`, then reaped
//! with `waitpid`.

use slopos_abi::syscall::SYSCALL_PIDFD_OPEN;
use slopos_slibc::pal::raw::syscall1;

/// Open a process-exit fd for child task `pid`. Returns the fd (`>= 0`) or
/// a negated errno (`-ESRCH` if no such task, `-EPERM` if not a child).
#[inline(always)]
pub fn pidfd_open(pid: u32) -> i32 {
    unsafe { syscall1(SYSCALL_PIDFD_OPEN, pid as u64) as i32 }
}
