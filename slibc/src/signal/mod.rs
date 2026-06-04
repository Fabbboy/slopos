//! Signal handling — taming the chaos of asynchronous fate.

pub mod tests;

use core::mem;

use crate::pal::slopos::signal_restorer_addr;
use crate::pal::{Pal, Sys};
use slopos_abi::signal::UserSigaction;

/// True when `handler` is a real function pointer (not `SIG_DFL`/`SIG_IGN`).
///
/// The kernel rejects (`EINVAL`) a real handler whose `sa_restorer` is 0,
/// so libc must inject its own restorer for exactly these handlers.
#[inline]
fn is_catchable_handler(handler: u64) -> bool {
    handler != slopos_abi::signal::SIG_DFL && handler != slopos_abi::signal::SIG_IGN
}

// ---------------------------------------------------------------------------
// Signal number constants (re-exported from abi for C consumers)
// ---------------------------------------------------------------------------

pub const SIGHUP: i32 = slopos_abi::signal::SIGHUP as i32;
pub const SIGINT: i32 = slopos_abi::signal::SIGINT as i32;
pub const SIGQUIT: i32 = slopos_abi::signal::SIGQUIT as i32;
pub const SIGILL: i32 = slopos_abi::signal::SIGILL as i32;
pub const SIGTRAP: i32 = slopos_abi::signal::SIGTRAP as i32;
pub const SIGABRT: i32 = slopos_abi::signal::SIGABRT as i32;
pub const SIGBUS: i32 = slopos_abi::signal::SIGBUS as i32;
pub const SIGFPE: i32 = slopos_abi::signal::SIGFPE as i32;
pub const SIGKILL: i32 = slopos_abi::signal::SIGKILL as i32;
pub const SIGUSR1: i32 = slopos_abi::signal::SIGUSR1 as i32;
pub const SIGSEGV: i32 = slopos_abi::signal::SIGSEGV as i32;
pub const SIGUSR2: i32 = slopos_abi::signal::SIGUSR2 as i32;
pub const SIGPIPE: i32 = slopos_abi::signal::SIGPIPE as i32;
pub const SIGALRM: i32 = slopos_abi::signal::SIGALRM as i32;
pub const SIGTERM: i32 = slopos_abi::signal::SIGTERM as i32;
pub const SIGCHLD: i32 = slopos_abi::signal::SIGCHLD as i32;
pub const SIGCONT: i32 = slopos_abi::signal::SIGCONT as i32;
pub const SIGSTOP: i32 = slopos_abi::signal::SIGSTOP as i32;
pub const SIGTSTP: i32 = slopos_abi::signal::SIGTSTP as i32;
pub const SIGTTIN: i32 = slopos_abi::signal::SIGTTIN as i32;
pub const SIGTTOU: i32 = slopos_abi::signal::SIGTTOU as i32;
pub const SIGWINCH: i32 = slopos_abi::signal::SIGWINCH as i32;

pub const SIG_DFL: usize = slopos_abi::signal::SIG_DFL as usize;
pub const SIG_IGN: usize = slopos_abi::signal::SIG_IGN as usize;

pub type SigHandler = unsafe extern "C" fn(i32);

const SIGSET_SIZE: usize = mem::size_of::<u64>();

// ---------------------------------------------------------------------------
// signal()
// ---------------------------------------------------------------------------

/// Install a signal handler (simplified BSD-style interface).
///
/// Returns the previous handler, or `SIG_ERR` (usize::MAX cast) on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal(signum: i32, handler: usize) -> usize {
    let mut act: UserSigaction = mem::zeroed();
    act.sa_handler = handler as u64;
    act.sa_flags = slopos_abi::signal::SA_RESTART;
    act.sa_mask = 0;
    // The kernel requires a nonzero restorer for catchable handlers; inject
    // ours. SIG_DFL/SIG_IGN need no restorer, so leave it 0 for those.
    act.sa_restorer = if is_catchable_handler(act.sa_handler) {
        signal_restorer_addr()
    } else {
        0
    };

    let mut old_act: UserSigaction = mem::zeroed();

    match Sys::rt_sigaction(
        signum,
        &act as *const UserSigaction as *const u8,
        &mut old_act as *mut UserSigaction as *mut u8,
        SIGSET_SIZE,
    ) {
        Ok(()) => old_act.sa_handler as usize,
        Err(_) => usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// sigaction()
// ---------------------------------------------------------------------------

/// Examine or change a signal action (POSIX interface).
///
/// Returns 0 on success, -1 on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigaction(
    signum: i32,
    act: *const UserSigaction,
    oldact: *mut UserSigaction,
) -> i32 {
    // When installing a catchable handler with no caller-supplied restorer,
    // substitute libc's trampoline (glibc behavior). A nonzero restorer or a
    // SIG_DFL/SIG_IGN install passes through untouched. A NULL `act` is a
    // query-only call and is forwarded as-is.
    let mut patched: UserSigaction;
    let act_ptr: *const u8 = if !act.is_null() {
        let a = &*act;
        if a.sa_restorer == 0 && is_catchable_handler(a.sa_handler) {
            patched = *a;
            patched.sa_restorer = signal_restorer_addr();
            &patched as *const UserSigaction as *const u8
        } else {
            act as *const u8
        }
    } else {
        core::ptr::null()
    };

    match Sys::rt_sigaction(signum, act_ptr, oldact as *mut u8, SIGSET_SIZE) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// sigprocmask()
// ---------------------------------------------------------------------------

/// Examine or change the blocked signal mask.
///
/// `how`: `SIG_BLOCK`, `SIG_UNBLOCK`, or `SIG_SETMASK`.
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigprocmask(how: i32, set: *const u64, oldset: *mut u64) -> i32 {
    match Sys::rt_sigprocmask(how, set, oldset, SIGSET_SIZE) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// kill()
// ---------------------------------------------------------------------------

/// Send a signal to a process.
///
/// Returns 0 on success, -1 on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kill(pid: i32, sig: i32) -> i32 {
    match Sys::kill(pid, sig) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// raise()
// ---------------------------------------------------------------------------

/// Send a signal to the calling process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn raise(sig: i32) -> i32 {
    kill(Sys::getpid(), sig)
}

// ---------------------------------------------------------------------------
// abort()
// ---------------------------------------------------------------------------

/// Abort the process — sends SIGABRT, then force-exits if the handler returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn abort() -> ! {
    let _ = raise(SIGABRT);
    crate::process::_exit(134)
}
