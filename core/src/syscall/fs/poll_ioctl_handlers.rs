#![allow(clippy::too_many_arguments)]

use core::ffi::c_int;

use slopos_abi::signal::SIGTTOU;
use slopos_abi::syscall::{
    ERRNO_EINTR, FIONREAD, POLLIN, POLLOUT, TCFLSH, TCGETS, TCSBRK, TCSETS, TCSETSF, TCSETSW,
    TCXONC, TIOCEXCL, TIOCGETD, TIOCGEXCL, TIOCGPGRP, TIOCGPTLCK, TIOCGPTN, TIOCGPTPEER, TIOCGSID,
    TIOCGWINSZ, TIOCNOTTY, TIOCNXCL, TIOCOUTQ, TIOCPKT, TIOCSCTTY, TIOCSETD, TIOCSPGRP, TIOCSPTLCK,
    UserPollFd, UserTermios, UserTimeval, UserWinsize,
};

use slopos_fs::fileio::{
    file_get_tty_index, file_open_tty_fd, file_poll_fused, file_poll_unfused_by_idx,
};

use slopos_kernel_services::driver_runtime::{
    current_task_pgid, is_current_signal_blocked_or_ignored, is_pgrp_orphaned, run_bottom_halves,
    signal_process_group,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::user_copy::{copy_bytes_from_user, copy_from_user, copy_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

const SELECT_MAX_FDS: usize = 256;

// ioctl helper return: Ok(value) → ctx.ok(value); Err(()) → ctx.err().
type IoctlResult = Result<u64, ()>;

#[inline(never)]
fn ioctl_termios(tty_idx: slopos_abi::syscall::TtyIndex, cmd: u64, arg: u64) -> IoctlResult {
    if arg == 0 {
        return Err(());
    }
    let ptr = UserPtr::<UserTermios>::try_new(arg).map_err(|_| ())?;
    match cmd {
        TCGETS => {
            let t = tty::get_termios(tty_idx).map_err(|_| ())?;
            copy_to_user(ptr, &t).map_err(|_| ())?;
            Ok(0)
        }
        TCSETS => {
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_termios(tty_idx, &val).map(|_| 0).map_err(|_| ())
        }
        TCSETSW => {
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_termios_wait(tty_idx, &val)
                .map(|_| 0)
                .map_err(|_| ())
        }
        TCSETSF => {
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_termios_flush(tty_idx, &val)
                .map(|_| 0)
                .map_err(|_| ())
        }
        _ => Err(()),
    }
}

#[inline(never)]
fn ioctl_winsize(tty_idx: slopos_abi::syscall::TtyIndex, cmd: u64, arg: u64) -> IoctlResult {
    if arg == 0 {
        return Err(());
    }
    let ptr = UserPtr::<UserWinsize>::try_new(arg).map_err(|_| ())?;
    match cmd {
        TIOCGWINSZ => {
            let ws = tty::get_winsize(tty_idx).map_err(|_| ())?;
            copy_to_user(ptr, &ws).map_err(|_| ())?;
            Ok(0)
        }
        slopos_abi::syscall::TIOCSWINSZ => {
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_winsize(tty_idx, &val).map(|_| 0).map_err(|_| ())
        }
        _ => Err(()),
    }
}

#[inline(never)]
fn ioctl_pty(tty_idx: slopos_abi::syscall::TtyIndex, cmd: u64, arg: u64, pid: u32) -> IoctlResult {
    match cmd {
        TIOCGPTN => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let pty_number = tty::get_pty_number(tty_idx).map_err(|_| ())?;
            copy_to_user(ptr, &pty_number).map_err(|_| ())?;
            Ok(0)
        }
        TIOCGPTPEER => {
            let peer_tty = tty::open_pty_peer(tty_idx).map_err(|_| ())?;
            let new_fd = file_open_tty_fd(pid, peer_tty, arg as u32);
            if new_fd < 0 {
                let _ = tty::close_ref(peer_tty);
                Err(())
            } else {
                Ok(new_fd as u64)
            }
        }
        TIOCSPTLCK => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_pty_lock(tty_idx, val != 0)
                .map(|_| 0)
                .map_err(|_| ())
        }
        TIOCGPTLCK => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let state = tty::get_pty_lock(tty_idx).map_err(|_| ())?;
            let state_i = i32::from(state);
            copy_to_user(ptr, &state_i).map_err(|_| ())?;
            Ok(0)
        }
        TIOCPKT => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_packet_mode(tty_idx, val != 0)
                .map(|_| 0)
                .map_err(|_| ())
        }
        _ => Err(()),
    }
}

#[inline(never)]
fn ioctl_misc(tty_idx: slopos_abi::syscall::TtyIndex, cmd: u64, arg: u64) -> IoctlResult {
    match cmd {
        TIOCGETD => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let ldisc_id = tty::get_ldisc(tty_idx).unwrap_or(slopos_abi::syscall::N_TTY);
            copy_to_user(ptr, &ldisc_id).map_err(|_| ())?;
            Ok(0)
        }
        TIOCSETD => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let ldisc_id = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_ldisc(tty_idx, ldisc_id).map(|_| 0).map_err(|_| ())
        }
        FIONREAD => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let count = tty::bytes_available(tty_idx).map_err(|_| ())? as i32;
            copy_to_user(ptr, &count).map_err(|_| ())?;
            Ok(0)
        }
        TIOCOUTQ => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let count = tty::output_queued_bytes(tty_idx).map_err(|_| ())? as i32;
            copy_to_user(ptr, &count).map_err(|_| ())?;
            Ok(0)
        }
        TCFLSH => tty::tcflush(tty_idx, arg as i32).map(|_| 0).map_err(|_| ()),
        TCSBRK => tty::tcsbrk(tty_idx, arg as i32).map(|_| 0).map_err(|_| ()),
        TCXONC => tty::tcxonc(tty_idx, arg as i32).map(|_| 0).map_err(|_| ()),
        TIOCEXCL => tty::set_exclusive(tty_idx, true).map(|_| 0).map_err(|_| ()),
        TIOCNXCL => tty::set_exclusive(tty_idx, false)
            .map(|_| 0)
            .map_err(|_| ()),
        TIOCGEXCL => {
            if arg == 0 {
                return Err(());
            }
            let ptr = UserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let state = tty::get_exclusive(tty_idx).map_err(|_| ())?;
            let state_i = i32::from(state);
            copy_to_user(ptr, &state_i).map_err(|_| ())?;
            Ok(0)
        }
        _ => Err(()),
    }
}

#[inline]
fn fdset_bytes_len(nfds: usize) -> usize {
    nfds.div_ceil(8)
}

fn fdset_test(buf: &[u8], fd: usize) -> bool {
    let byte = fd / 8;
    let bit = fd % 8;
    if byte >= buf.len() {
        return false;
    }
    (buf[byte] & (1u8 << bit)) != 0
}

fn fdset_set(buf: &mut [u8], fd: usize) {
    let byte = fd / 8;
    let bit = fd % 8;
    if byte < buf.len() {
        buf[byte] |= 1u8 << bit;
    }
}

fn poll_to_select_mask(
    revents: u16,
    read_set: bool,
    write_set: bool,
    except_set: bool,
) -> (bool, bool, bool) {
    let read_ready = read_set
        && (revents & (POLLIN | slopos_abi::syscall::POLLHUP | slopos_abi::syscall::POLLERR)) != 0;
    let write_ready = write_set && (revents & (POLLOUT | slopos_abi::syscall::POLLERR)) != 0;
    let except_ready = except_set && (revents & slopos_abi::syscall::POLLPRI) != 0;
    (read_ready, write_ready, except_ready)
}

// AUDIT 2D: poll/select wait/wake — fan-in correct, hygiene to fix later.
//
// poll/select fan into multiple WaitQueues (one per polled FD: pipe
// reader/writer WQ, TTY WQ, socket WQ, ...) and back via a shared timeout.
// Each per-FD WQ correctly notifies on its own readiness event under the
// SpinLock-pair contract documented in `slopos_ostd::sync::wait_queue`,
// so the fan-in IS race-free with respect to LOST wakes — a producer
// flipping readiness then calling `wake_*` will always wake an enqueued
// poller.
//
// KNOWN ISSUE (Phase 7 cleanup target, not a correctness blocker):
// when one FD becomes ready and `wake_one` dequeues this task from that
// FD's WQ, the task is still enqueued on the OTHER polled FDs' WQs until
// the loop body falls through to `cleanup!()`. The fast path here
// (`block_current_task_with_timeout` returns -> `cleanup!()` runs ->
// `file_poll_unfused_by_idx` -> `poll_unwait` -> `WaitQueue::remove_current`)
// drains them, but a producer firing `wake_one` on one of those queues
// in the narrow window between wakeup and cleanup is a SPURIOUS wake of
// an already-Running task — benign, not a lost-wake.
//
// The structural fix (Linux's `poll_wait` registers a poll_table entry
// with a remove-on-wake hook so a wake on ANY queue eagerly drains the
// others) belongs to Phase 7's poll cleanup.
define_syscall!(syscall_poll(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg1_usize();
    let timeout_ms = args.arg2 as i64;

    if args.arg0 == 0 || nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let base_ptr = args.arg0;
    let start_ms = slopos_kernel_services::platform::get_time_ms();

    // Bundle the three scratch arrays into one heap struct so the
    // function frame stays small. A stack-resident
    // `[u16 + UserPollFd + u32; SELECT_MAX_FDS]` triplet would cost
    // ~3.5 KiB per poll(2) call.
    #[repr(C)]
    struct PollScratch {
        cached_revents: [u16; SELECT_MAX_FDS],
        poll_fds: [UserPollFd; SELECT_MAX_FDS],
        registered_ofis: [u32; SELECT_MAX_FDS],
    }
    // SAFETY: every field is a primitive integer/struct of integers;
    // the all-zero bit pattern is a valid value.
    unsafe impl slopos_ostd::Zeroable for PollScratch {}
    let mut scratch_box = match slopos_ostd::KBox::<PollScratch>::zeroed() {
        Ok(b) => b,
        Err(_) => return ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM),
    };
    let scratch: &mut PollScratch = &mut *scratch_box;
    let cached_revents = &mut scratch.cached_revents;
    let poll_fds: &mut [UserPollFd] = &mut scratch.poll_fds;
    let registered_ofis = &mut scratch.registered_ofis;

    loop {
        run_bottom_halves();

        // Pre-copy all poll FDs from userspace before registering waiters
        // so a user-copy failure cannot leak fused refs.
        for idx in 0..nfds {
            let user_ptr = try_or_err!(ctx, UserPtr::<UserPollFd>::try_new(
                base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64,
            ));
            poll_fds[idx] = try_or_err!(ctx, copy_from_user(user_ptr));
        }

        // ── SINGLE PASS: fused register + readiness check ──────────
        let mut ready_count = 0u64;
        // Reset per-iteration count; storage is reused across iterations.
        let mut reg_count = 0usize;

        for idx in 0..nfds {
            let pfd = &poll_fds[idx];
            if pfd.fd < 0 {
                cached_revents[idx] = 0;
            } else {
                let result = file_poll_fused(pid, pfd.fd as c_int, pfd.events);
                cached_revents[idx] = result.revents;
                if result.revents != 0 {
                    ready_count += 1;
                }
                if result.registered && reg_count < registered_ofis.len() {
                    registered_ofis[reg_count] = result.open_file_idx;
                    reg_count += 1;
                }
            }
        }

        // ── Cleanup + writeback helpers ────────────────────────────
        macro_rules! cleanup {
            () => {
                for &ofi in &registered_ofis[..reg_count] {
                    file_poll_unfused_by_idx(ofi);
                }
            };
        }
        macro_rules! writeback_revents {
            () => {
                // Write only the 2-byte revents field at its offset within
                // each UserPollFd (offset 6: after i32 fd + u16 events).
                // Avoids re-reading the entire struct from userspace.
                const REVENTS_OFFSET: u64 = 6;
                for idx in 0..nfds {
                    let revents_addr = base_ptr
                        + (idx * core::mem::size_of::<UserPollFd>()) as u64
                        + REVENTS_OFFSET;
                    let revents_ptr = try_or_err!(ctx,
                        slopos_mm::user_ptr::UserBytes::try_new(revents_addr, 2)
                    );
                    try_or_err!(ctx,
                        slopos_mm::user_copy::copy_bytes_to_user(revents_ptr, &cached_revents[idx].to_ne_bytes())
                    );
                }
            };
        }

        if ready_count > 0 {
            cleanup!();
            writeback_revents!();
            return ctx.ok(ready_count);
        }

        if timeout_ms == 0 {
            cleanup!();
            writeback_revents!();
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = slopos_kernel_services::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                cleanup!();
                writeback_revents!();
                return ctx.ok(0);
            }
        }

        // ── Sleep ──────────────────────────────────────────────────
        let sleep_ms = if timeout_ms < 0 {
            500u32
        } else {
            let remaining = timeout_ms
                - (slopos_kernel_services::platform::get_time_ms()
                    .wrapping_sub(start_ms) as i64);
            (remaining.max(0) as u32).min(500)
        };

        if reg_count > 0 {
            slopos_kernel_services::driver_runtime::block_current_task_with_timeout(sleep_ms);
        } else {
            slopos_kernel_services::platform::timer_poll_delay_ms(1);
        }

        cleanup!();

        if slopos_kernel_services::driver_runtime::has_pending_signal() {
            return ctx.err_with(ERRNO_EINTR as u64);
        }
    }
});

define_syscall!(syscall_select(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg0_usize();
    if nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let bytes_len = fdset_bytes_len(nfds);
    // Hoist the six fd-sets and the per-iteration registered_ofis array
    // into a single heap struct so this function fits the stack-sizes
    // gate. Inline `[u8; FDSET_BYTES]` × 6 + `[u32; SELECT_MAX_FDS]`
    // would put ~1.2 KiB on the stack on every call.
    const FDSET_BYTES: usize = SELECT_MAX_FDS / 8;
    #[repr(C)]
    struct SelectScratch {
        read_in: [u8; FDSET_BYTES],
        write_in: [u8; FDSET_BYTES],
        except_in: [u8; FDSET_BYTES],
        read_out: [u8; FDSET_BYTES],
        write_out: [u8; FDSET_BYTES],
        except_out: [u8; FDSET_BYTES],
        registered_ofis: [u32; SELECT_MAX_FDS],
    }
    // SAFETY: `SelectScratch` is composed entirely of byte/integer arrays;
    // the all-zero bit pattern is a valid value.
    unsafe impl slopos_ostd::Zeroable for SelectScratch {}
    let mut scratch_box = match slopos_ostd::KBox::<SelectScratch>::zeroed() {
        Ok(b) => b,
        Err(_) => return ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM as u64),
    };
    let scratch: &mut SelectScratch = &mut *scratch_box;

    if args.arg1 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
        let copied = try_or_err!(
            ctx,
            copy_bytes_from_user(in_bytes, &mut scratch.read_in[..bytes_len])
        );
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg2 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
        let copied = try_or_err!(
            ctx,
            copy_bytes_from_user(in_bytes, &mut scratch.write_in[..bytes_len])
        );
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg3 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
        let copied = try_or_err!(
            ctx,
            copy_bytes_from_user(in_bytes, &mut scratch.except_in[..bytes_len])
        );
        if copied != bytes_len {
            return ctx.err();
        }
    }
    let SelectScratch {
        read_in,
        write_in,
        except_in,
        read_out,
        write_out,
        except_out,
        registered_ofis,
    } = scratch;

    let timeout_ms = if args.arg4 == 0 {
        -1i64
    } else {
        let tv_ptr = try_or_err!(ctx, UserPtr::<UserTimeval>::try_new(args.arg4));
        let tv = try_or_err!(ctx, copy_from_user(tv_ptr));
        if tv.tv_sec < 0 || tv.tv_usec < 0 {
            return ctx.err();
        }
        tv.tv_sec
            .saturating_mul(1000)
            .saturating_add(tv.tv_usec / 1000)
    };

    let start_ms = slopos_kernel_services::platform::get_time_ms();

    /// Copy the three result fd-sets back to userspace. `#[inline(never)]`
    /// so each call reuses one stack frame for the three
    /// `UserBytes::try_new` + `copy_bytes_to_user` pairs — the previous
    /// macro inlined the same scratch four times in the loop below.
    #[inline(never)]
    fn copy_out_select_results(
        read_ptr: u64,
        write_ptr: u64,
        except_ptr: u64,
        read_out: &[u8],
        write_out: &[u8],
        except_out: &[u8],
        bytes_len: usize,
    ) -> Result<(), u32> {
        use slopos_mm::user_copy::copy_bytes_to_user;
        use slopos_mm::user_ptr::UserBytes;
        const EFAULT: u32 = slopos_abi::syscall::ERRNO_EFAULT as u32;
        if read_ptr != 0 {
            let out = UserBytes::try_new(read_ptr, bytes_len).map_err(|_| EFAULT)?;
            copy_bytes_to_user(out, &read_out[..bytes_len]).map_err(|_| EFAULT)?;
        }
        if write_ptr != 0 {
            let out = UserBytes::try_new(write_ptr, bytes_len).map_err(|_| EFAULT)?;
            copy_bytes_to_user(out, &write_out[..bytes_len]).map_err(|_| EFAULT)?;
        }
        if except_ptr != 0 {
            let out = UserBytes::try_new(except_ptr, bytes_len).map_err(|_| EFAULT)?;
            copy_bytes_to_user(out, &except_out[..bytes_len]).map_err(|_| EFAULT)?;
        }
        Ok(())
    }

    macro_rules! copy_out_sets {
        () => {{
            if let Err(errno) = copy_out_select_results(
                args.arg1,
                args.arg2,
                args.arg3,
                read_out,
                write_out,
                except_out,
                bytes_len,
            ) {
                return ctx.err_with(errno as u64);
            }
        }};
    }

    loop {
        read_out[..bytes_len].fill(0);
        write_out[..bytes_len].fill(0);
        except_out[..bytes_len].fill(0);

        // ── SINGLE PASS: fused register + readiness check ──────────
        let mut ready = 0u64;
        // Store open_file_idx for cleanup instead of FD numbers to avoid
        // the TOCTOU where an FD is closed and reassigned between
        // registration and cleanup. `registered_ofis` is the heap-resident
        // array borrowed from `scratch` above; reset its first reg_count
        // entries each iteration.
        let mut reg_count = 0usize;

        for fd in 0..nfds {
            let want_r = args.arg1 != 0 && fdset_test(&read_in[..bytes_len], fd);
            let want_w = args.arg2 != 0 && fdset_test(&write_in[..bytes_len], fd);
            let want_e = args.arg3 != 0 && fdset_test(&except_in[..bytes_len], fd);
            if !(want_r || want_w || want_e) {
                continue;
            }

            let mut mask = 0u16;
            if want_r {
                mask |= POLLIN;
            }
            if want_w {
                mask |= POLLOUT;
            }
            if want_e {
                mask |= slopos_abi::syscall::POLLPRI;
            }

            let result = file_poll_fused(pid, fd as c_int, mask);
            let (rdy_r, rdy_w, rdy_e) =
                poll_to_select_mask(result.revents, want_r, want_w, want_e);
            if rdy_r {
                fdset_set(&mut read_out[..bytes_len], fd);
                ready += 1;
            }
            if rdy_w {
                fdset_set(&mut write_out[..bytes_len], fd);
                ready += 1;
            }
            if rdy_e {
                fdset_set(&mut except_out[..bytes_len], fd);
                ready += 1;
            }
            if result.registered && reg_count < registered_ofis.len() {
                registered_ofis[reg_count] = result.open_file_idx;
                reg_count += 1;
            }
        }

        macro_rules! cleanup {
            () => {
                for &ofi in &registered_ofis[..reg_count] {
                    file_poll_unfused_by_idx(ofi);
                }
            };
        }

        if ready > 0 {
            cleanup!();
            copy_out_sets!();
            return ctx.ok(ready);
        }

        if timeout_ms == 0 {
            cleanup!();
            copy_out_sets!();
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = slopos_kernel_services::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                cleanup!();
                copy_out_sets!();
                return ctx.ok(0);
            }
        }

        let sleep_ms = if timeout_ms < 0 {
            500u32
        } else {
            let remaining = timeout_ms
                - (slopos_kernel_services::platform::get_time_ms()
                    .wrapping_sub(start_ms) as i64);
            (remaining.max(0) as u32).min(500)
        };

        if reg_count > 0 {
            slopos_kernel_services::driver_runtime::block_current_task_with_timeout(sleep_ms);
        } else {
            slopos_kernel_services::platform::timer_poll_delay_ms(1);
        }

        cleanup!();

        if slopos_kernel_services::driver_runtime::has_pending_signal() {
            return ctx.err_with(ERRNO_EINTR as u64);
        }
    }
});

define_syscall!(syscall_ioctl(ctx, args) requires(let task_id, let pid: process_id) {
    let fd = args.arg0 as c_int;
    let cmd = args.arg1;
    let arg = args.arg2;

    // Resolve the TTY index from the file descriptor.  If the FD is not a TTY,
    // ioctl is not supported.
    let tty_idx = match file_get_tty_index(pid, fd) {
        Some(idx) => idx,
        None => return ctx.err(), // ENOTTY
    };

    // Post-hangup I/O hardening — most ioctls return EIO on a
    // hung-up TTY.  Exceptions (POSIX-mandated): TIOCGPGRP, TIOCSPGRP,
    // TIOCGSID must remain functional for job control cleanup after hangup.
    let hangup_safe = matches!(cmd, TIOCGPGRP | TIOCSPGRP | TIOCGSID | TIOCNOTTY);
    if !hangup_safe && tty::is_hung_up(tty_idx) {
        return ctx.err(); // EIO
    }

    match cmd {
        TCGETS | TCSETS | TCSETSW | TCSETSF => match ioctl_termios(tty_idx, cmd, arg) {
            Ok(v) => ctx.ok(v),
            Err(_) => ctx.err(),
        },
        TIOCGWINSZ | slopos_abi::syscall::TIOCSWINSZ => match ioctl_winsize(tty_idx, cmd, arg) {
            Ok(v) => ctx.ok(v),
            Err(_) => ctx.err(),
        },
        TIOCGPTN | TIOCGPTPEER | TIOCSPTLCK | TIOCGPTLCK | TIOCPKT => {
            match ioctl_pty(tty_idx, cmd, arg, pid) {
                Ok(v) => ctx.ok(v),
                Err(_) => ctx.err(),
            }
        }
        TIOCGETD | TIOCSETD | FIONREAD | TIOCOUTQ | TCFLSH | TCSBRK | TCXONC | TIOCEXCL
        | TIOCNXCL | TIOCGEXCL => match ioctl_misc(tty_idx, cmd, arg) {
            Ok(v) => ctx.ok(v),
            Err(_) => ctx.err(),
        },
        TIOCGPGRP => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx).unwrap_or(0);
            try_or_err!(ctx, copy_to_user(ptr, &fg_pgrp));
            ctx.ok(0)
        }
        TIOCSPGRP => {
            require_nonzero!(ctx, arg);

            // POSIX: tcsetpgrp() only works on the caller's controlling terminal.
            let task_ptr = crate::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return ctx.err();
            }
            let Some(task) = crate::scheduler::task::task_borrow(task_ptr) else {
                return ctx.err();
            };
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return ctx.err(), // ENOTTY
            }

            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let pgrp = try_or_err!(ctx, copy_from_user(ptr));
            let caller_pgid = current_task_pgid();
            let caller_sid = crate::sched::current_task_sid();

            let tty_sid = tty::get_session_id(tty_idx).unwrap_or(0);
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx).unwrap_or(0);
            if tty_sid != 0 && tty_sid == caller_sid && caller_pgid != 0 && fg_pgrp != 0 && caller_pgid != fg_pgrp {
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    if is_pgrp_orphaned(caller_pgid, caller_sid) {
                        return ctx.err();
                    }
                    let _ = signal_process_group(caller_pgid, SIGTTOU);
                    return ctx.err();
                }
            }

            if tty::set_foreground_pgrp_checked(tty_idx, pgrp, caller_sid).is_ok() {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        TIOCSCTTY => {
            if arg != 0 {
                return ctx.err();
            }

            let task_ptr = crate::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return ctx.err();
            }

            let Some(task) = crate::scheduler::task::task_borrow_mut(task_ptr) else {
                return ctx.err();
            };
            if task.sid == 0 || task.sid != task.task_id {
                return ctx.err();
            }

            if let Some(current_tty) = task.controlling_tty {
                if current_tty == tty_idx {
                    return ctx.ok(0);
                }
                return ctx.err();
            }

            let tty_sid = tty::get_session_id(tty_idx).unwrap_or(0);
            if tty_sid != 0 && tty_sid != task.sid {
                return ctx.err();
            }

            if tty::acquire_controlling_terminal(tty_idx, task.sid, task.pgid).is_err() {
                return ctx.err();
            }
            task.controlling_tty = Some(tty_idx);
            ctx.ok(0)
        }
        TIOCGSID => {
            require_nonzero!(ctx, arg);
            // POSIX: TIOCGSID only works on the caller's controlling terminal.
            let task_ptr = crate::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return ctx.err();
            }
            let Some(task) = crate::scheduler::task::task_borrow(task_ptr) else {
                return ctx.err();
            };
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return ctx.err(), // ENOTTY
            }
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let sid = tty::get_session_id(tty_idx).unwrap_or(0);
            try_or_err!(ctx, copy_to_user(ptr, &sid));
            ctx.ok(0)
        }
        TIOCNOTTY => {
            // Detach calling process from its controlling terminal.
            //
            // If the caller has no controlling TTY, or the TTY doesn't match,
            // return ENOTTY.
            let task_ptr = crate::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return ctx.err();
            }

            let Some(task) = crate::scheduler::task::task_borrow_mut(task_ptr) else {
                return ctx.err();
            };
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return ctx.err(), // ENOTTY — not our controlling terminal
            }

            let caller_sid = task.sid;
            let is_session_leader = task.sid != 0 && task.sid == task.task_id;

            // Always clear the caller's controlling_tty.
            task.controlling_tty = None;

            // If session leader, detach the entire session from the TTY
            // and signal the foreground pgrp with SIGHUP + SIGCONT.
            let _ = tty::detach_controlling_terminal(tty_idx, caller_sid, is_session_leader);

            ctx.ok(0)
        }
        _ => ctx.err(),
    }
});
