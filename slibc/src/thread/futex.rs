//! Shared futex-wait wrapper for the pthread primitives.
//!
//! Every blocking primitive here loops on a futex word, so an errno the loop
//! does not understand must not be swallowed: discarding it turns a wait that
//! cannot block into a full-core busy-spin that makes no progress and never
//! reports why.

use crate::errno::{EAGAIN, EINTR, ETIMEDOUT};
use crate::pal::{Pal, Sys};

/// Block on `addr` until woken.
///
/// `EAGAIN` (the word changed before the kernel queued us), `EINTR` (a signal)
/// and `ETIMEDOUT` are the outcomes a retry loop is entitled to ignore — the
/// caller re-tests its own condition. Anything else means the wait cannot be
/// performed at all, and spinning on it would hide a kernel-side failure
/// behind a pegged CPU.
#[inline]
pub(crate) fn futex_wait_or_abort(addr: *const u32, val: u32, timeout_ms: u64) {
    match Sys::futex_wait(addr, val, timeout_ms) {
        Ok(()) => {}
        Err(e) if e == EAGAIN || e == EINTR || e == ETIMEDOUT => {}
        Err(_) => abort_unexpected(),
    }
}

#[cold]
fn abort_unexpected() -> ! {
    // SAFETY: `abort` takes no arguments, touches no caller memory, and
    // diverges.
    unsafe { crate::signal::abort() }
}
