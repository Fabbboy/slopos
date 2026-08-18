#![feature(restricted_std)]

//! Signal-on-IRQ-exit end-to-end test.
//!
//! The child spins in userspace issuing no syscalls, so only a timer IRQ
//! can pull it out of user mode; the parent's SIGINT must still terminate
//! it and `waitpid` reap `128 + SIGINT`.

use slopos_abi::signal::SIGINT;
use slopos_userland as _;
use slopos_userland::syscall::{core as sys_core, process};

/// Default-terminate encodes the exit code as `128 + signum`.
const EXPECTED_EXIT_CODE: i32 = 128 + SIGINT as i32;

/// Returns the child task id in the parent; never returns in the child.
fn fork_spinning_child() -> i32 {
    let pid = process::fork();
    if pid == 0 {
        loop {
            core::hint::spin_loop();
        }
    }
    pid
}

fn test_spin_child_killed_by_sigint() -> bool {
    let pid = fork_spinning_child();
    if pid <= 0 {
        eprintln!("spin_signal_test: fork failed (pid={pid})");
        return false;
    }

    // Let the child reach its spin loop first; a kill landing mid-spawn is
    // still correct, the pending bit is acted on at the first IRQ exit.
    sys_core::yield_now();

    let rc = process::kill_pid(pid, SIGINT);
    if rc != 0 {
        eprintln!("spin_signal_test: kill(SIGINT) failed (rc={rc})");
        let _ = process::waitpid(pid as u32);
        return false;
    }

    let status = process::waitpid(pid as u32);
    if status != EXPECTED_EXIT_CODE {
        eprintln!("spin_signal_test: child exit code {status}, expected {EXPECTED_EXIT_CODE}");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[(
    "spin_child_killed_by_sigint",
    test_spin_child_killed_by_sigint,
)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
