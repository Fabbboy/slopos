#![feature(restricted_std)]

//! pidfd end-to-end test (process-exit fd).
//!
//! Verifies a child's `pidfd` signals `POLLIN` on exit through both `poll(2)`
//! and SlopRing `OP_POLL_ADD`, with a standalone fork/exit/waitpid case to
//! isolate the exit-code path.

use slopos_abi::syscall::POLLIN;
use slopos_userland as _;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::{UserPollFd, core as sys_core, fs, pidfd, process};

const CHILD_EXIT_CODE: i32 = 42;

/// Returns the child task id in the parent; never returns in the child.
fn fork_exiting_child() -> i32 {
    let pid = process::fork();
    if pid == 0 {
        // Child: raw exit — no std atexit/stdio flush in a forked child.
        sys_core::exit_with_code(CHILD_EXIT_CODE);
    }
    pid
}

/// Isolates the fork/exit/reap path from the pidfd behaviour proper.
fn test_fork_exit_waitpid() -> bool {
    let pid = fork_exiting_child();
    if pid <= 0 {
        return false;
    }
    process::waitpid(pid as u32) == CHILD_EXIT_CODE
}

/// `pidfd_open` succeeds for a child of the caller and yields a real fd.
fn test_pidfd_open() -> bool {
    let pid = fork_exiting_child();
    if pid <= 0 {
        return false;
    }
    let fd = pidfd::pidfd_open(pid as u32);
    let _ = process::waitpid(pid as u32);
    if fd < 0 {
        return false;
    }
    let _ = slopos_slibc::ffi::close(fd);
    true
}

/// pidfd + `poll(2)`: the fd becomes `POLLIN`-ready when the child exits.
fn test_pidfd_poll() -> bool {
    let pid = fork_exiting_child();
    if pid <= 0 {
        return false;
    }
    let child = pid as u32;
    let fd = pidfd::pidfd_open(child);
    if fd < 0 {
        let _ = process::waitpid(child);
        return false;
    }

    // The child's exit SIGCHLD interrupts poll(2) with EINTR; the retry's
    // readiness check runs before poll's own signal check, so it observes the
    // now-exited child as POLLIN.
    let mut pfds = [UserPollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    let mut ready = false;
    for _ in 0..10 {
        pfds[0].revents = 0;
        match fs::poll(&mut pfds, 5000) {
            Ok(n) if n >= 1 => {
                ready = (pfds[0].revents & POLLIN) != 0;
                break;
            }
            Ok(_) => break,     // genuine timeout: child never exited
            Err(_) => continue, // EINTR (SIGCHLD): child just exited — retry
        }
    }

    let _ = process::waitpid(child);
    let _ = slopos_slibc::ffi::close(fd);
    ready
}

/// pidfd + SlopRing `OP_POLL_ADD`: the ring completes with `POLLIN` when the
/// child exits.
fn test_pidfd_ring_poll_add() -> bool {
    let pid = fork_exiting_child();
    if pid <= 0 {
        return false;
    }
    let child = pid as u32;
    let fd = pidfd::pidfd_open(child);
    if fd < 0 {
        let _ = process::waitpid(child);
        return false;
    }

    let revents = match Ring::setup(8) {
        Ok(ring) => slopfut::block_on(ring, slopfut::poll_add(fd, POLLIN)),
        Err(_) => {
            let _ = process::waitpid(child);
            let _ = slopos_slibc::ffi::close(fd);
            return false;
        }
    };

    let _ = process::waitpid(child);
    let _ = slopos_slibc::ffi::close(fd);
    revents >= 0 && (revents as u16 & POLLIN) != 0
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("fork_exit_waitpid", test_fork_exit_waitpid),
    ("pidfd_open", test_pidfd_open),
    ("pidfd_poll", test_pidfd_poll),
    ("pidfd_ring_poll_add", test_pidfd_ring_poll_add),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
