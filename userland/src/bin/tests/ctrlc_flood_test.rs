#![feature(restricted_std)]

//! Ctrl-C-under-output-flood end-to-end regression test.
//!
//! Locks the full interrupt pipeline interactive Ctrl-C rides: PTY master write
//! of 0x03 -> slave line-discipline ISIG -> SIGINT to the slave's foreground
//! process group -> wake of a writer blocked on the full master read buffer.
//!
//! The scenario: a foreground job floods stdout while nothing drains the master,
//! the master read buffer fills, and the job blocks inside `write()`. Interrupting
//! it needs ISIG to fire independent of queue fullness, the blocked write's wait
//! predicate to observe the pending signal instead of re-blocking, and ISIG with
//! NOFLSH clear to flush the slave's undelivered output.

// Linked for its `_start` ELF entry point; without it the linker emits entry
// 0x0 and `do_exec` rejects the ELF.
use slopos_userland as _;

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::signal::{SIGINT, SIGKILL, SIGTSTP, SIGTTIN, SIGTTOU};
use slopos_abi::syscall::{InputFlags, LocalFlags};
use slopos_userland::syscall::{SyscallError, core as sys_core, fs, process};

/// One flood chunk; small enough that the child performs many writes (and
/// is virtually guaranteed to sit blocked mid-`write()` when 0x03 lands).
const FLOOD_CHUNK: &[u8] = b"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\n";

/// Bounded reap: a lost Ctrl-C must FAIL the case, not wedge the harness.
const REAP_SPINS: usize = 50_000;

/// Yields granted for the child to set up, start flooding, fill the master
/// read buffer, and block in `write()` before the interrupt is sent.
const FLOOD_SPINS: usize = 500;
const INPUT_FILL_CHUNK: &[u8] = &[b'x'; 512];

static GOT_SIGINT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: i32) {
    GOT_SIGINT.store(true, Ordering::SeqCst);
}

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

/// The session/foreground dance every job-control shell performs on its
/// controlling TTY.
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

fn kill_and_reap(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let pid_u32 = pid as u32;
    let _ = process::kill(pid_u32, SIGKILL);
    reap_bounded(pid_u32)
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
            break;
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
    // The parent never drains during the flood: the master read buffer must
    // fill so the child blocks inside write().
    let _ = fs::set_fd_nonblocking(master_fd);

    let pid = process::fork();
    if pid == 0 {
        child_become_fg(slave_fd);
        if noflsh {
            // NOFLSH: ISIG must still fire but must not flush the queues, so the
            // blocked writer can only unwind via the pending-signal check in its
            // wait predicate.
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

    // 0x03 travels master->slave, so a full master READ buffer must not impede
    // it.
    if fs::write_slice(master_fd, b"\x03").is_err() {
        eprintln!("ctrlc_flood_test: writing VINTR to master failed");
        let _ = kill_and_reap(pid);
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return None;
    }

    let code = reap_bounded(pid as u32);
    let drained = drain_master(master_fd);

    let result = match code {
        Some(code) => Some((code, drained)),
        None => {
            eprintln!("ctrlc_flood_test: child never exited — Ctrl-C was lost");
            let _ = kill_and_reap(pid);
            None
        }
    };
    let _ = fs::close_fd_raw(master_fd);
    let _ = fs::close_fd_raw(slave_fd);
    result
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
    // ISIG output flush: only bytes written after the flush (the ^C caret echo)
    // may remain.
    if drained >= 256 {
        eprintln!(
            "ctrlc_flood_test: master still held {drained} bytes — ISIG did not flush output"
        );
        return false;
    }
    true
}

/// Installed handler: SIGINT is delivered, not merely terminating, while the
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
/// predicate stays false forever and the blocked writer can only unwind by
/// observing its pending signal inside the wait.
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

/// Input-throttle regression: a full slave input queue must not block VINTR.
///
/// The child owns the slave foreground process group but never reads. The
/// parent fills the slave input path until ordinary writes hit EAGAIN, then
/// writes one Ctrl-C, which the kernel must admit through the throttled PTY.
fn test_ctrlc_kills_input_throttled_fg_child() -> bool {
    let (master_fd, slave_fd) = match open_pair() {
        Some(pair) => pair,
        None => {
            eprintln!("ctrlc_flood_test(input): openpty/fd setup failed");
            return false;
        }
    };

    let _ = fs::set_fd_nonblocking(master_fd);
    if let Ok(mut t) = fs::tcgetattr(slave_fd) {
        t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::NOFLSH);
        t.c_lflag |= LocalFlags::ISIG;
        t.c_iflag = InputFlags::empty();
        let _ = fs::tcsetattr(slave_fd, &t);
    }

    let pid = process::fork();
    if pid == 0 {
        child_become_fg(slave_fd);
        let _ = process::default_signal(SIGINT);
        loop {
            sys_core::yield_now();
        }
    }
    if pid < 0 {
        eprintln!("ctrlc_flood_test(input): fork failed");
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return false;
    }

    for _ in 0..FLOOD_SPINS {
        sys_core::yield_now();
    }

    let mut filled = 0usize;
    for _ in 0..64 {
        match fs::write_slice(master_fd, INPUT_FILL_CHUNK) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e == SyscallError::EAGAIN => break,
            Err(e) => {
                eprintln!("ctrlc_flood_test(input): fill write failed: {e:?}");
                let _ = kill_and_reap(pid);
                let _ = fs::close_fd_raw(master_fd);
                let _ = fs::close_fd_raw(slave_fd);
                return false;
            }
        }
    }

    if filled < 4096 {
        eprintln!("ctrlc_flood_test(input): filled only {filled} bytes before interrupt");
        let _ = kill_and_reap(pid);
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return false;
    }

    if fs::write_slice(master_fd, b"\x03") != Ok(1) {
        eprintln!("ctrlc_flood_test(input): VINTR write failed under input throttle");
        let _ = kill_and_reap(pid);
        let _ = fs::close_fd_raw(master_fd);
        let _ = fs::close_fd_raw(slave_fd);
        return false;
    }

    let code = reap_bounded(pid as u32);

    let result = match code {
        Some(code) if code == 128 + SIGINT as i32 => true,
        Some(code) => {
            eprintln!(
                "ctrlc_flood_test(input): expected exit {}, got {code}",
                128 + SIGINT as i32
            );
            false
        }
        None => {
            eprintln!("ctrlc_flood_test(input): child never exited");
            let _ = kill_and_reap(pid);
            false
        }
    };
    let _ = fs::close_fd_raw(master_fd);
    let _ = fs::close_fd_raw(slave_fd);
    result
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
    (
        "ctrlc_kills_input_throttled_fg_child",
        test_ctrlc_kills_input_throttled_fg_child,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
