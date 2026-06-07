#![feature(restricted_std)]

//! Ctrl-C-under-output-flood end-to-end regression test.
//!
//! Locks the FULL interrupt pipeline that interactive Ctrl-C rides:
//! PTY master write of 0x03 -> slave line-discipline ISIG -> SIGINT to the
//! slave's foreground process group -> wake of a writer blocked on the FULL
//! master read buffer -> delivery (default-kill or installed handler).
//!
//! The scenario it guards: a foreground job floods stdout while nothing
//! drains the master (the terminal is busy/wedged), the master read buffer
//! fills, and the job blocks inside `write()`. Ctrl-C must still interrupt
//! it — which requires three kernel behaviors working together:
//!   1. ISIG fires independent of queue fullness (signal char is processed
//!      before any buffer-space check),
//!   2. the blocked write's wait predicate observes the pending signal and
//!      unwinds with ERESTARTSYS instead of re-blocking forever,
//!   3. ISIG (NOFLSH clear) flushes the slave's undelivered output — the
//!      master read buffer — so the flood is discarded, writers wake, and
//!      the caret echo/prompt land in an empty queue.
//!
//! Historic regressions in this pipeline ping-ponged because no automated
//! test drove it; interactive-only verification cannot keep it locked.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point
// is linked into the binary (same requirement as the sibling test bins;
// without it the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::signal::{SIGINT, SIGTSTP, SIGTTIN, SIGTTOU};
use slopos_abi::syscall::LocalFlags;
use slopos_userland::syscall::{core as sys_core, fs, process};

/// One flood chunk; small enough that the child performs many writes (and
/// is virtually guaranteed to sit blocked mid-`write()` when 0x03 lands).
const FLOOD_CHUNK: &[u8] = b"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\n";

/// Bounded reap: a lost Ctrl-C must FAIL the case, not wedge the harness.
const REAP_SPINS: usize = 50_000;

/// Yields granted for the child to set up, start flooding, fill the master
/// read buffer, and block in `write()` before the interrupt is sent.
const FLOOD_SPINS: usize = 500;

static GOT_SIGINT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: i32) {
    GOT_SIGINT.store(true, Ordering::SeqCst);
}

fn open_pair() -> Option<(i32, i32)> {
    let (master_idx, _slave_idx) = process::openpty().ok()?;
    let master_fd = process::open_tty_fd(master_idx).ok()?.into_raw();
    let slave_fd = match fs::ioctl_tiocgptpeer(master_fd) {
        Ok(fd) => fd.into_raw(),
        Err(_) => {
            let _ = fs::close_fd_raw(master_fd);
            return None;
        }
    };
    Some((master_fd, slave_fd))
}

/// The session/foreground dance every job-control shell performs on its
/// controlling TTY (mirrors the shell's initialize_job_control()).
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

/// Drain whatever is readable from the (non-blocking) master right now.
fn drain_master(master_fd: i32) -> usize {
    let mut total = 0usize;
    let mut buf = [0u8; 512];
    while let Ok(n) = fs::read_slice(master_fd, &mut buf) {
        if n == 0 {
            break;
        }
        total += n;
        if total > 64 * 1024 {
            break; // defensive bound; the assert below fails anyway
        }
    }
    total
}

fn run_flood_case(install_handler: bool, noflsh: bool) -> Option<(i32, usize)> {
    let (master_fd, slave_fd) = match open_pair() {
        Some(pair) => pair,
        None => {
            eprintln!("ctrlc_flood_test: openpty/fd setup failed");
            return None;
        }
    };
    // The parent NEVER drains during the flood — that is the point: the
    // master read buffer must fill so the child blocks inside write().
    let _ = fs::set_fd_nonblocking(master_fd);

    let pid = process::fork();
    if pid == 0 {
        // CHILD: become the foreground job on the slave, then flood.
        child_become_fg(slave_fd);
        if noflsh {
            // NOFLSH: ISIG must still fire but must NOT flush the queues —
            // the blocked writer can then only unwind via the pending-signal
            // check in its wait predicate.
            if let Ok(mut t) = fs::tcgetattr(slave_fd) {
                t.c_lflag |= LocalFlags::NOFLSH;
                let _ = fs::tcsetattr(slave_fd, &t);
            }
        }
        if install_handler {
            let _ = process::set_signal_handler(SIGINT, on_sigint);
        } else {
            let _ = process::default_signal(SIGINT);
        }
        loop {
            let _ = fs::write_slice(slave_fd, FLOOD_CHUNK);
            if install_handler && GOT_SIGINT.load(Ordering::SeqCst) {
                std::process::exit(42);
            }
        }
    }
    if pid < 0 {
        eprintln!("ctrlc_flood_test: fork failed");
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return None;
    }

    // Let the child reach the blocked-in-write state.
    for _ in 0..FLOOD_SPINS {
        sys_core::yield_now();
    }

    // The interrupt: one 0x03 into the master. Travels master->slave, so a
    // full master READ buffer must not impede it.
    if fs::write_slice(master_fd, b"\x03").is_err() {
        eprintln!("ctrlc_flood_test: writing VINTR to master failed");
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return None;
    }

    let code = reap_bounded(pid as u32);
    let drained = drain_master(master_fd);
    let _ = fs::close_fd_raw(master_fd);
    let _ = fs::close_fd_raw(slave_fd);

    match code {
        Some(code) => Some((code, drained)),
        None => {
            eprintln!("ctrlc_flood_test: child never exited — Ctrl-C was lost");
            None
        }
    }
}

/// Default disposition: the flooding foreground child is killed by SIGINT
/// (exit 128+2) even while blocked in write() on a full master.
fn test_ctrlc_kills_flooding_fg_child() -> bool {
    let Some((code, drained)) = run_flood_case(false, false) else {
        return false;
    };
    if code != 128 + SIGINT as i32 {
        eprintln!(
            "ctrlc_flood_test: expected exit {}, got {code}",
            128 + SIGINT as i32
        );
        return false;
    }
    // ISIG output flush: the flood must have been discarded. Only bytes
    // written after the flush (the ^C caret echo) may remain.
    if drained >= 256 {
        eprintln!(
            "ctrlc_flood_test: master still held {drained} bytes — ISIG did not flush output"
        );
        return false;
    }
    true
}

/// Installed handler: SIGINT is DELIVERED (not just terminates) while the
/// child is blocked in write() — the EINTR/restart path a shell's
/// record-the-interrupt handler depends on.
fn test_ctrlc_handler_delivered_under_flood() -> bool {
    GOT_SIGINT.store(false, Ordering::SeqCst);
    let Some((code, _drained)) = run_flood_case(true, false) else {
        return false;
    };
    if code != 42 {
        eprintln!("ctrlc_flood_test: expected handler exit 42, got {code}");
        return false;
    }
    true
}

/// NOFLSH set: ISIG fires but nothing is flushed, so the full-master wait
/// predicate stays false forever — the blocked writer can ONLY unwind by
/// observing its pending signal inside the wait. Goes RED if the
/// pending-signal check in `wait_for_write_ready`'s master arm regresses,
/// independent of the ISIG output flush.
fn test_ctrlc_kills_flooding_fg_child_noflsh() -> bool {
    let Some((code, _drained)) = run_flood_case(false, true) else {
        return false;
    };
    if code != 128 + SIGINT as i32 {
        eprintln!(
            "ctrlc_flood_test(noflsh): expected exit {}, got {code}",
            128 + SIGINT as i32
        );
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "ctrlc_kills_flooding_fg_child",
        test_ctrlc_kills_flooding_fg_child,
    ),
    (
        "ctrlc_handler_delivered_under_flood",
        test_ctrlc_handler_delivered_under_flood,
    ),
    (
        "ctrlc_kills_flooding_fg_child_noflsh",
        test_ctrlc_kills_flooding_fg_child_noflsh,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
