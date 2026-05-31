#![feature(restricted_std)]

//! signalfd end-to-end test — the EINTR-footgun regression test.
//!
//! With SIGCHLD blocked, a child's exit is delivered as an **in-band**
//! `POLLIN` event on a signalfd, not as an out-of-band `EINTR` that aborts
//! the wait. The proof: a *single* `poll(2)` call (no EINTR-retry loop)
//! observes the child-exit signal. Unblocked, that same `poll` would return
//! `EINTR` when SIGCHLD lands (the footgun documented earlier). Draining the
//! signalfd then yields a `SignalfdSiginfo` whose `ssi_signo` is SIGCHLD.

use slopos_abi::signal::{SIGCHLD, sig_bit};
use slopos_abi::syscall::POLLIN;
use slopos_userland as _;
use slopos_userland::syscall::{UserPollFd, core as sys_core, fs, process, signalfd};

fn test_sigchld_inband() -> bool {
    let mask = sig_bit(SIGCHLD);

    // Block SIGCHLD: it now queues (signalfd-drainable) instead of EINTR-ing
    // a blocked wait — `(pending & !blocked)` excludes it from poll's EINTR
    // check, while the signalfd (which tests raw `pending`) still reports it.
    signalfd::block_signals(mask);

    let sfd = signalfd::signalfd(mask, 0);
    if sfd < 0 {
        eprintln!("signalfd_test: signalfd create failed ({sfd})");
        return false;
    }

    let pid = process::fork();
    if pid == 0 {
        // Child: exit immediately; our SIGCHLD will land on the parent.
        sys_core::exit_with_code(0);
    }
    if pid < 0 {
        let _ = slopos_slibc::ffi::close(sfd);
        eprintln!("signalfd_test: fork failed ({pid})");
        return false;
    }
    let child = pid as u32;

    // SINGLE poll, NO EINTR-retry loop. With SIGCHLD blocked this returns
    // POLLIN once the child exits; unblocked it would return EINTR.
    let mut pfds = [UserPollFd {
        fd: sfd,
        events: POLLIN,
        revents: 0,
    }];
    let ready =
        matches!(fs::poll(&mut pfds, 5000), Ok(n) if n >= 1) && (pfds[0].revents & POLLIN) != 0;

    // Drain the siginfo; ssi_signo (LE u32 at offset 0) must be SIGCHLD.
    let mut buf = [0u8; 16];
    let signo_ok = matches!(fs::read_slice(sfd, &mut buf), Ok(n) if n >= 4)
        && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == SIGCHLD as u32;

    let _ = process::waitpid(child);
    let _ = slopos_slibc::ffi::close(sfd);

    if !ready {
        eprintln!("signalfd_test: poll did not report POLLIN (footgun not fixed?)");
        return false;
    }
    if !signo_ok {
        eprintln!("signalfd_test: drained siginfo ssi_signo != SIGCHLD");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[("sigchld_inband", test_sigchld_inband)];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
