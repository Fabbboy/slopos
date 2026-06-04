#![feature(restricted_std)]

//! libc signal()/sigaction() handler-install end-to-end test.
//!
//! Regression guard for the EINVAL-on-install footgun: slibc's `signal()`
//! and `sigaction()` used to leave `sa_restorer` at 0, which the kernel
//! rejects for any catchable handler (it requires a nonzero restorer and
//! bails out of delivery when it is 0). libc must inject its own restorer
//! trampoline — exactly what glibc does. These cases install a real handler
//! through libc, `raise()` the signal, and prove the handler actually ran
//! by observing a volatile flag it sets.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point
// is linked into the binary (same requirement as the sibling test bins;
// without it the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::signal::UserSigaction;
use slopos_slibc::signal::{self, SIG_DFL, SIGUSR1, SIGUSR2};

static SIGUSR1_COUNT: AtomicU32 = AtomicU32::new(0);
static SIGUSR2_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_sigusr1(_sig: i32) {
    SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_sigusr2(_sig: i32) {
    SIGUSR2_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// `signal()` must install a real handler (not fail with SIG_ERR) and the
/// handler must run when the signal is raised.
fn test_signal_installs_and_delivers() -> bool {
    SIGUSR1_COUNT.store(0, Ordering::SeqCst);

    let prev = unsafe { signal::signal(SIGUSR1, on_sigusr1 as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: signal() returned SIG_ERR (install rejected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR1) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR1) failed");
        return false;
    }

    let count = SIGUSR1_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: handler ran {count} times, expected 1");
        return false;
    }

    // Restore default so a stray later signal doesn't re-enter the handler.
    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };
    true
}

/// `sigaction()` with `sa_restorer == 0` on a real handler must have libc
/// substitute its own restorer (glibc behavior), so the install succeeds and
/// the handler runs.
fn test_sigaction_injects_restorer() -> bool {
    SIGUSR2_COUNT.store(0, Ordering::SeqCst);

    let act = UserSigaction {
        sa_handler: on_sigusr2 as *const () as usize as u64,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };

    let rc = unsafe { signal::sigaction(SIGUSR2, &act, core::ptr::null_mut()) };
    if rc != 0 {
        eprintln!("signal_handler_test: sigaction() returned {rc} (restorer not injected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR2) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR2) failed");
        return false;
    }

    let count = SIGUSR2_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: sigaction handler ran {count} times, expected 1");
        return false;
    }

    let _ = unsafe { signal::signal(SIGUSR2, SIG_DFL) };
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "signal_installs_and_delivers",
        test_signal_installs_and_delivers,
    ),
    (
        "sigaction_injects_restorer",
        test_sigaction_injects_restorer,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
