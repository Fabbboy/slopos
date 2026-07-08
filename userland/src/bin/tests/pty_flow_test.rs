#![feature(restricted_std)]

//! PTY output-flow regression test.
//!
//! Guards the slave→master wakeup. A program that floods its stdout (e.g.
//! `cat /dev/kmsg`) fills the PTY master's 4 KiB read buffer and blocks inside
//! `write()`; when the reader (the terminal) drains the master it MUST wake
//! that blocked writer so the next bufferful flows. Before the fix the master
//! *read* path published no wakeup — only the master *write* (keystroke) and
//! Ctrl-C flush paths did — so a large stream advanced exactly one bufferful
//! per keypress and otherwise stalled ("press a key to see the next screenful
//! of `cat` output").
//!
//! The test forks a flooding child onto a PTY slave and drains the master from
//! the parent, asserting the whole payload arrives with no progress stall. It
//! goes RED if the master-read wakeup regresses: the parent collects roughly
//! one bufferful and then the writer is never rewoken.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point is
// linked into the binary (same requirement as the sibling test bins; without
// it the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use slopos_abi::signal::{SIGKILL, SIGTSTP, SIGTTIN, SIGTTOU};
use slopos_userland::syscall::{SyscallError, core as sys_core, fs, process};

/// Far larger than the kernel's 4 KiB master `RawDisc` buffer, so the child
/// must block-and-be-rewoken dozens of times for the whole payload to arrive.
const PAYLOAD: usize = 256 * 1024;

/// Bounded reap so a stalled child FAILS the case instead of wedging the harness.
const REAP_SPINS: usize = 200_000;

/// Consecutive zero-progress parent reads that mean "the writer was never
/// rewoken" — the stall this test exists to catch. Reset on every byte
/// received, so the success path (steady progress) never approaches it.
const MAX_IDLE_READS: usize = 20_000;

fn open_pair() -> Option<(i32, i32)> {
    let (master_owned, _slave_num) = process::openpty().ok()?;
    let master_fd = master_owned.into_raw();
    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = fs::close_fd_raw(master_fd);
            return None;
        }
    };
    Some((master_fd, slave_fd))
}

/// The session/foreground dance a job-control shell performs on its controlling
/// TTY, so the flooding child's writes are foreground (not SIGTTOU'd).
fn child_become_fg(slave_fd: i32) {
    let _ = process::ignore_signal(SIGTTOU);
    let _ = process::ignore_signal(SIGTTIN);
    let _ = process::ignore_signal(SIGTSTP);
    let _ = process::setsid();
    let _ = fs::tiocsctty(slave_fd);
    let _ = process::setpgid(0, 0);
    let pgid = process::getpgid(0);
    if pgid > 0 {
        let _ = fs::tcsetpgrp(slave_fd, pgid as u32);
    }
}

fn reap_bounded(pid: u32) -> Option<i32> {
    for _ in 0..REAP_SPINS {
        if let Some(code) = process::waitpid_nohang(pid) {
            return Some(code);
        }
        sys_core::yield_now();
    }
    None
}

fn kill_and_reap(pid: i32) {
    if pid > 0 {
        let _ = process::kill(pid as u32, SIGKILL);
        let _ = reap_bounded(pid as u32);
    }
}

/// Fork a flooding child, drain the master from the parent, and assert the full
/// payload arrives. RED without the master-read wakeup (parent collects ~one
/// bufferful then stalls), GREEN with it.
fn test_master_drain_wakes_blocked_slave_writer() -> bool {
    let (master_fd, slave_fd) = match open_pair() {
        Some(pair) => pair,
        None => {
            eprintln!("pty_flow_test: openpty/fd setup failed");
            return false;
        }
    };
    // The terminal owns a non-blocking master and drains it; mirror that.
    let _ = fs::set_fd_nonblocking(master_fd);

    let pid = process::fork();
    if pid == 0 {
        // CHILD: own the slave foreground group, then flood stdout with a known
        // payload via *blocking* writes (exactly what `cat` does). 'Z' carries
        // no newline, so no ONLCR expansion perturbs the byte count, and the
        // child never reads, so ECHO produces nothing on the master.
        child_become_fg(slave_fd);
        let chunk = [b'Z'; 1024];
        let mut sent = 0usize;
        while sent < PAYLOAD {
            let want = core::cmp::min(chunk.len(), PAYLOAD - sent);
            match fs::write_slice(slave_fd, &chunk[..want]) {
                Ok(n) if n > 0 => sent += n,
                // Parent gave up and closed the master => unblock and bail.
                _ => break,
            }
        }
        std::process::exit(if sent == PAYLOAD { 0 } else { 1 });
    }
    if pid < 0 {
        eprintln!("pty_flow_test: fork failed");
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return false;
    }

    // Parent is the sole reader. Close our slave ref so the child's exit is the
    // last slave close (a clean EOF backstop on the master).
    let _ = fs::close_fd_raw(slave_fd);

    let mut received = 0usize;
    let mut buf = [0u8; 4096];
    let mut idle = 0usize;
    let mut stalled = false;
    while received < PAYLOAD {
        match fs::read_slice(master_fd, &mut buf) {
            Ok(0) => break, // EOF: child closed the slave before sending all.
            Ok(n) => {
                received += n;
                idle = 0;
            }
            Err(e) if e == SyscallError::EAGAIN => {
                idle += 1;
                if idle > MAX_IDLE_READS {
                    stalled = true;
                    break;
                }
                sys_core::yield_now();
            }
            Err(_) => break,
        }
    }

    // Unblock/clean up the child regardless of outcome (closing the master
    // releases any write the child is blocked in), then reap.
    let _ = fs::close_fd_raw(master_fd);
    let code = reap_bounded(pid as u32);
    if code.is_none() {
        kill_and_reap(pid);
    }

    if stalled {
        eprintln!(
            "pty_flow_test: stalled after {received}/{PAYLOAD} bytes — master drain did NOT \
             wake the blocked slave writer (output would advance only on a keystroke)"
        );
        return false;
    }
    if received != PAYLOAD {
        eprintln!("pty_flow_test: received {received}/{PAYLOAD} bytes");
        return false;
    }
    if code != Some(0) {
        eprintln!("pty_flow_test: child exit {code:?}, expected 0 (full payload sent)");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[(
    "master_drain_wakes_blocked_slave_writer",
    test_master_drain_wakes_blocked_slave_writer,
)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
