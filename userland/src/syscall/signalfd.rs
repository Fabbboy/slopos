//! signalfd syscall wrapper + the signal-blocking helper a ring reactor uses.
//!
//! Together these turn signal delivery from an out-of-band `EINTR` into an
//! in-band ring/poll event: [`block_signals`] keeps the signals pending
//! (so `(pending & !blocked)` excludes them from the harvest's EINTR check)
//! while [`signalfd`] exposes them as a `POLLIN`-able fd to drain.

use super::raw::{syscall2, syscall4};
use slopos_abi::signal::SIG_BLOCK;
use slopos_abi::syscall::{SYSCALL_RT_SIGPROCMASK, SYSCALL_SIGNALFD};

/// `signalfd(mask, flags)` — create a `FileKind::Signalfd` watching the
/// signals in `mask`. Returns the fd (`>= 0`) or a negated errno. `read`
/// drains one `SignalfdSiginfo`; `poll`/`OP_POLL_ADD` report `POLLIN` while
/// a masked signal is pending.
#[inline(always)]
pub fn signalfd(mask: u64, flags: u32) -> i32 {
    unsafe { syscall2(SYSCALL_SIGNALFD, mask, flags as u64) as i32 }
}

/// Block the signals in `mask` (`SIG_BLOCK`) so they queue (drainable via a
/// signalfd) instead of interrupting blocking waits with `EINTR`. A reactor
/// calls this for every signal it intends to harvest as a completion.
#[inline(always)]
pub fn block_signals(mask: u64) -> i32 {
    let set = mask;
    unsafe {
        syscall4(
            SYSCALL_RT_SIGPROCMASK,
            SIG_BLOCK as u64,
            &set as *const u64 as u64,
            0,
            core::mem::size_of::<u64>() as u64,
        ) as i32
    }
}
