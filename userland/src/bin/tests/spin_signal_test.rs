#![feature(restricted_std)]

//! Signal-on-IRQ-exit end-to-end test.
//!
//! A child enters a pure userspace spin loop that issues NO syscalls,
//! so the only way it can ever leave userspace is a timer/IRQ. The
//! parent kills it with SIGINT (default disposition: terminate) and
//! waits. Before signals were delivered on IRQ return-to-user, such a
//! child was unkillable — it would spin forever because the kernel only
//! checked pending signals on syscall exit, which the child never
//! reached. With IRQ-exit delivery, the next timer tick redirects the
//! child into the default-terminate path and the parent's `waitpid`
//! reaps it with exit code `128 + SIGINT`.

use slopos_abi::signal::SIGINT;
use slopos_userland as _;
use slopos_userland::syscall::{core as sys_core, process};

/// Default-terminate encodes the exit code as `128 + signum`.
const EXPECTED_EXIT_CODE: i32 = 128 + SIGINT as i32;

/// Fork a child that spins purely in userspace forever (no syscalls).
/// Returns the child task id in the parent; never returns in the child.
fn fork_spinning_child() -> i32 {
    let pid = process::fork();
    if pid == 0 {
        // Child: tight userspace loop with no syscalls. Only a timer
        // IRQ can ever pull this task out of user mode.
        loop {
            core::hint::spin_loop();
        }
    }
    pid
}

/// A pure-spin child must be killable via SIGINT delivered on IRQ exit,
/// and `waitpid` must reap it with the default-terminate exit code.
fn test_spin_child_killed_by_sigint() -> bool {
    let pid = fork_spinning_child();
    if pid <= 0 {
        eprintln!("spin_signal_test: fork failed (pid={pid})");
        return false;
    }

    // Give the child a chance to start spinning in user mode before we
    // signal it (the kill is still correct if the child is mid-spawn —
    // the pending bit just gets acted on at the first IRQ exit).
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
