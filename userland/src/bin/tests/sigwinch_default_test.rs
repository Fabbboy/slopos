#![feature(restricted_std)]

//! SIGWINCH default-disposition end-to-end test.
//!
//! The default must be ignore — a process with no handler installed survives
//! the signal — while an installed handler still receives it.

// Links the `slopos-userland` lib for its `_start` ELF entry point; without it
// the linker emits entry 0x0 and `do_exec` rejects the binary.
use slopos_userland as _;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::signal::{SIGCHLD, SIGWINCH};
use slopos_userland::syscall::process;

static SIGWINCH_COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_sigwinch(_sig: i32) {
    SIGWINCH_COUNT.fetch_add(1, Ordering::SeqCst);
}

fn test_sigwinch_default_survives() -> bool {
    let pid = process::getpid();
    if process::default_signal(SIGWINCH) != 0 {
        eprintln!("sigwinch_default_test: resetting SIGWINCH to SIG_DFL failed");
        return false;
    }
    if process::kill(pid, SIGWINCH) != 0 {
        eprintln!("sigwinch_default_test: kill(self, SIGWINCH) failed");
        return false;
    }
    // Forward progress past the raise is what proves survival.
    process::getpid() == pid
}

fn test_sigchld_default_survives() -> bool {
    let pid = process::getpid();
    if process::default_signal(SIGCHLD) != 0 {
        eprintln!("sigwinch_default_test: resetting SIGCHLD to SIG_DFL failed");
        return false;
    }
    if process::kill(pid, SIGCHLD) != 0 {
        eprintln!("sigwinch_default_test: kill(self, SIGCHLD) failed");
        return false;
    }
    process::getpid() == pid
}

/// The signal must reach an installed handler, not be discarded at the send site.
fn test_sigwinch_handler_still_delivers() -> bool {
    SIGWINCH_COUNT.store(0, Ordering::SeqCst);

    if process::set_signal_handler(SIGWINCH, on_sigwinch) != 0 {
        eprintln!("sigwinch_default_test: installing SIGWINCH handler failed");
        return false;
    }
    let pid = process::getpid();
    if process::kill(pid, SIGWINCH) != 0 {
        eprintln!("sigwinch_default_test: kill(self, SIGWINCH) with handler failed");
        return false;
    }

    let count = SIGWINCH_COUNT.load(Ordering::SeqCst);
    let _ = process::default_signal(SIGWINCH);
    if count != 1 {
        eprintln!("sigwinch_default_test: handler ran {count} times, expected 1");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("sigwinch_default_survives", test_sigwinch_default_survives),
    ("sigchld_default_survives", test_sigchld_default_survives),
    (
        "sigwinch_handler_still_delivers",
        test_sigwinch_handler_still_delivers,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
