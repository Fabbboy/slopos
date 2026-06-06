//! signalfd syscall wrapper + the signal-blocking helper a ring reactor uses.
//!
//! Together these turn signal delivery from an out-of-band `EINTR` into an
//! in-band ring/poll event: [`block_signals`] keeps the signals pending
//! (so `(pending & !blocked)` excludes them from the harvest's EINTR check)
//! while [`signalfd`] exposes them as a `POLLIN`-able fd to drain.

use slopos_abi::signal::{SIG_BLOCK, SIG_UNBLOCK};
use slopos_abi::syscall::{SYSCALL_RT_SIGPROCMASK, SYSCALL_SIGNALFD};
use slopos_slibc::pal::raw::{syscall2, syscall4};

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
///
/// Returns the PREVIOUS blocked mask on success (`Err` carries the negated
/// errno), so a caller that needs to roll back can restore exactly the bits
/// it changed — unblocking the whole mask blindly would clear blocks the
/// process held before this call.
#[inline(always)]
pub fn block_signals(mask: u64) -> Result<u64, i32> {
    let set = mask;
    let mut old: u64 = 0;
    let rc = unsafe {
        syscall4(
            SYSCALL_RT_SIGPROCMASK,
            SIG_BLOCK as u64,
            &set as *const u64 as u64,
            &mut old as *mut u64 as u64,
            core::mem::size_of::<u64>() as u64,
        ) as i32
    };
    if rc < 0 { Err(rc) } else { Ok(old) }
}

/// Undo [`block_signals`] (`SIG_UNBLOCK`) so the signals in `mask` resume
/// their normal (handler / default-action) delivery. Used to roll back when
/// signalfd creation fails after the mask was already blocked — leaving the
/// mask blocked with no fd to drain it would make those signals silently
/// undeliverable for the rest of the process lifetime.
#[inline(always)]
pub fn unblock_signals(mask: u64) -> i32 {
    let set = mask;
    unsafe {
        syscall4(
            SYSCALL_RT_SIGPROCMASK,
            SIG_UNBLOCK as u64,
            &set as *const u64 as u64,
            0,
            core::mem::size_of::<u64>() as u64,
        ) as i32
    }
}
