#![allow(clippy::too_many_arguments)]

use core::ffi::c_int;

use slopos_abi::signal::SIGTTOU;
use slopos_abi::syscall::{
    FIONREAD, POLLIN, POLLOUT, TCFLSH, TCGETS, TCSBRK, TCSETS, TCSETSF, TCSETSW, TCXONC, TIOCEXCL,
    TIOCGETD, TIOCGEXCL, TIOCGPGRP, TIOCGPTLCK, TIOCGPTN, TIOCGPTPEER, TIOCGSID, TIOCGWINSZ,
    TIOCNOTTY, TIOCNXCL, TIOCOUTQ, TIOCPKT, TIOCSCTTY, TIOCSETD, TIOCSPGRP, TIOCSPTLCK, UserPollFd,
    UserTermios, UserTimeval, UserWinsize,
};

use slopos_fs::fileio::{
    file_get_tty_index, file_open_tty_fd, file_poll_fd, file_poll_register_pipes,
    file_poll_unregister_pipes,
};

use slopos_lib::kernel_services::driver_runtime::{
    current_task_pgid, is_current_signal_blocked_or_ignored, is_pgrp_orphaned, signal_process_group,
};
use slopos_lib::kernel_services::syscall_services::tty;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

const SELECT_MAX_FDS: usize = 256;

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

define_syscall!(syscall_poll(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg1_usize();
    let timeout_ms = args.arg2 as i64;

    if args.arg0 == 0 || nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let base_ptr = args.arg0;
    let start_ms = crate::platform::get_time_ms();

    let mut pipe_fds = [(0i32, 0u16); SELECT_MAX_FDS];

    loop {
        let mut ready_count = 0u64;
        let mut poll_tty_slots = [0u8; 32];
        let mut poll_tty_count = 0usize;
        let mut pipe_fd_count = 0usize;

        for idx in 0..nfds {
            let user_ptr = try_or_err!(
                ctx,
                UserPtr::<UserPollFd>::try_new(
                    base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64
                )
            );
            let mut pfd = try_or_err!(ctx, copy_from_user(user_ptr));
            if pfd.fd < 0 {
                pfd.revents = 0;
            } else {
                pfd.revents = file_poll_fd(pid, pfd.fd as c_int, pfd.events);
                if pfd.revents != 0 {
                    ready_count += 1;
                }
                if let Some(tty_idx) = file_get_tty_index(pid, pfd.fd as c_int) {
                    let s = tty_idx.0;
                    if poll_tty_count < poll_tty_slots.len()
                        && !poll_tty_slots[..poll_tty_count].contains(&s)
                    {
                        poll_tty_slots[poll_tty_count] = s;
                        poll_tty_count += 1;
                    }
                } else if pipe_fd_count < pipe_fds.len() {
                    pipe_fds[pipe_fd_count] = (pfd.fd as i32, pfd.events);
                    pipe_fd_count += 1;
                }
            }
            try_or_err!(ctx, copy_to_user(user_ptr, &pfd));
        }

        if ready_count > 0 {
            return ctx.ok(ready_count);
        }

        if timeout_ms == 0 {
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = crate::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                return ctx.ok(0);
            }
        }

        let pipe_registered = if pipe_fd_count > 0 {
            file_poll_register_pipes(pid, &pipe_fds[..pipe_fd_count])
        } else {
            0
        };

        if poll_tty_count > 0 {
            tty::poll_sleep_on(
                poll_tty_slots[..poll_tty_count].as_ptr(),
                poll_tty_count,
            );
        } else if pipe_registered > 0 {
            slopos_lib::kernel_services::driver_runtime::block_current_task();
        } else {
            tty::poll_sleep();
        }

        if pipe_registered > 0 {
            file_poll_unregister_pipes(pid, &pipe_fds[..pipe_fd_count]);
        }
    }
});

define_syscall!(syscall_select(ctx, args) requires(let pid: process_id) {
    let nfds = args.arg0_usize();
    if nfds > SELECT_MAX_FDS {
        return ctx.err();
    }

    let bytes_len = fdset_bytes_len(nfds);
    let mut read_in = [0u8; SELECT_MAX_FDS / 8];
    let mut write_in = [0u8; SELECT_MAX_FDS / 8];
    let mut except_in = [0u8; SELECT_MAX_FDS / 8];
    let mut read_out = [0u8; SELECT_MAX_FDS / 8];
    let mut write_out = [0u8; SELECT_MAX_FDS / 8];
    let mut except_out = [0u8; SELECT_MAX_FDS / 8];

    if args.arg1 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut read_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg2 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut write_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }
    if args.arg3 != 0 {
        let in_bytes = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
        let copied = try_or_err!(ctx, copy_bytes_from_user(in_bytes, &mut except_in[..bytes_len]));
        if copied != bytes_len {
            return ctx.err();
        }
    }

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

    let start_ms = crate::platform::get_time_ms();
    loop {
        read_out[..bytes_len].fill(0);
        write_out[..bytes_len].fill(0);
        except_out[..bytes_len].fill(0);
        let mut ready = 0u64;
        // Collect TTY slot indices for per-slot poll sleep.
        let mut poll_tty_slots = [0u8; 32];
        let mut poll_tty_count = 0usize;

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

            let revents = file_poll_fd(pid, fd as c_int, mask);
            let (rdy_r, rdy_w, rdy_e) = poll_to_select_mask(revents, want_r, want_w, want_e);
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

            // Track TTY indices for per-slot poll sleep.
            if let Some(tty_idx) = file_get_tty_index(pid, fd as c_int) {
                let s = tty_idx.0;
                if poll_tty_count < poll_tty_slots.len()
                    && !poll_tty_slots[..poll_tty_count].contains(&s)
                {
                    poll_tty_slots[poll_tty_count] = s;
                    poll_tty_count += 1;
                }
            }
        }

        if ready > 0 {
            if args.arg1 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
            }
            if args.arg2 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
            }
            if args.arg3 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
            }
            return ctx.ok(ready);
        }

        if timeout_ms == 0 {
            if args.arg1 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
            }
            if args.arg2 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
            }
            if args.arg3 != 0 {
                let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
            }
            return ctx.ok(0);
        }
        if timeout_ms > 0 {
            let now = crate::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                if args.arg1 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg1, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &read_out[..bytes_len]));
                }
                if args.arg2 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg2, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &write_out[..bytes_len]));
                }
                if args.arg3 != 0 {
                    let out = try_or_err!(ctx, UserBytes::try_new(args.arg3, bytes_len));
                    try_or_err!(ctx, copy_bytes_to_user(out, &except_out[..bytes_len]));
                }
                return ctx.ok(0);
            }
        }

        // Per-slot poll sleep instead of global wake.
        if poll_tty_count > 0 {
            tty::poll_sleep_on(
                poll_tty_slots[..poll_tty_count].as_ptr(),
                poll_tty_count,
            );
        } else {
            tty::poll_sleep();
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
        TCGETS => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let mut t = UserTermios::default();
            tty::get_termios(tty_idx, &mut t as *mut UserTermios);
            try_or_err!(ctx, copy_to_user(ptr, &t));
            ctx.ok(0)
        }
        TCSETS => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            tty::set_termios(tty_idx, &val as *const UserTermios);
            ctx.ok(0)
        }
        TCSETSW => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            if tty::set_termios_wait(tty_idx, &val as *const UserTermios) == 0 {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        TCSETSF => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserTermios>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            if tty::set_termios_flush(tty_idx, &val as *const UserTermios) == 0 {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        TIOCGETD => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let ldisc_id = tty::get_ldisc(tty_idx);
            try_or_err!(ctx, copy_to_user(ptr, &ldisc_id));
            ctx.ok(0)
        }
        TIOCGPTN => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let pty_number = tty::get_pty_number(tty_idx);
            if pty_number < 0 {
                ctx.err()
            } else {
                let pty_number = pty_number as u32;
                try_or_err!(ctx, copy_to_user(ptr, &pty_number));
                ctx.ok(0)
            }
        }
        TIOCGPTPEER => {
            let peer_tty = tty::open_pty_peer(tty_idx);
            if peer_tty < 0 {
                ctx.err()
            } else {
                let slave_idx = slopos_abi::syscall::TtyIndex(peer_tty as u8);
                let open_flags = arg as u32;
                let new_fd = file_open_tty_fd(pid, slave_idx, open_flags);
                if new_fd < 0 {
                    tty::close_ref(slave_idx);
                    ctx.err()
                } else {
                    ctx.ok(new_fd as u64)
                }
            }
        }
        TIOCSETD => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let ldisc_id = try_or_err!(ctx, copy_from_user(ptr));
            if tty::set_ldisc(tty_idx, ldisc_id) == 0 {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        TIOCGWINSZ => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserWinsize>::try_new(arg));
            let mut ws = UserWinsize::default();
            tty::get_winsize(tty_idx, &mut ws as *mut UserWinsize);
            try_or_err!(ctx, copy_to_user(ptr, &ws));
            ctx.ok(0)
        }
        slopos_abi::syscall::TIOCSWINSZ => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<UserWinsize>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            tty::set_winsize(tty_idx, &val as *const UserWinsize);
            ctx.ok(0)
        }
        TIOCGPGRP => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx);
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
            let task = unsafe { &*task_ptr };
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return ctx.err(), // ENOTTY
            }

            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let pgrp = try_or_err!(ctx, copy_from_user(ptr));
            let caller_pgid = current_task_pgid();
            let caller_sid = crate::sched::current_task_sid();

            let tty_sid = tty::get_session_id(tty_idx);
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx);
            if tty_sid != 0 && tty_sid == caller_sid && caller_pgid != 0 && fg_pgrp != 0 && caller_pgid != fg_pgrp {
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    if is_pgrp_orphaned(caller_pgid, caller_sid) {
                        return ctx.err();
                    }
                    let _ = signal_process_group(caller_pgid, SIGTTOU);
                    return ctx.err();
                }
            }

            let rc = tty::set_foreground_pgrp_checked(tty_idx, pgrp, caller_sid);
            if rc == 0 {
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

            let task = unsafe { &mut *task_ptr };
            if task.sid == 0 || task.sid != task.task_id {
                return ctx.err();
            }

            if let Some(current_tty) = task.controlling_tty {
                if current_tty == tty_idx {
                    return ctx.ok(0);
                }
                return ctx.err();
            }

            let tty_sid = tty::get_session_id(tty_idx);
            if tty_sid != 0 && tty_sid != task.sid {
                return ctx.err();
            }

            if tty::acquire_controlling_terminal(tty_idx, task.sid, task.pgid) != 0 {
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
            let task = unsafe { &*task_ptr };
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return ctx.err(), // ENOTTY
            }
            let ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(arg));
            let sid = tty::get_session_id(tty_idx);
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

            let task = unsafe { &mut *task_ptr };
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
        FIONREAD => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let count = tty::bytes_available(tty_idx);
            if count < 0 {
                ctx.err()
            } else {
                let count = count as i32;
                try_or_err!(ctx, copy_to_user(ptr, &count));
                ctx.ok(0)
            }
        }
        // PTY slave lock ioctls.
        TIOCSPTLCK => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            let locked = val != 0;
            if tty::set_pty_lock(tty_idx, locked) == 0 {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        TIOCGPTLCK => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let state = tty::get_pty_lock(tty_idx);
            if state < 0 {
                ctx.err()
            } else {
                let state = state as i32;
                try_or_err!(ctx, copy_to_user(ptr, &state));
                ctx.ok(0)
            }
        }
        // PTY packet mode.
        TIOCPKT => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let val = try_or_err!(ctx, copy_from_user(ptr));
            let enable = val != 0;
            if tty::set_packet_mode(tty_idx, enable) == 0 {
                ctx.ok(0)
            } else {
                ctx.err()
            }
        }
        // Missing ioctls.
        TCFLSH => {
            let queue = arg as i32;
            let rc = tty::tcflush(tty_idx, queue);
            if rc == 0 { ctx.ok(0) } else { ctx.err() }
        }
        TCSBRK => {
            let brk_arg = arg as i32;
            let rc = tty::tcsbrk(tty_idx, brk_arg);
            if rc == 0 { ctx.ok(0) } else { ctx.err() }
        }
        TCXONC => {
            let action = arg as i32;
            let rc = tty::tcxonc(tty_idx, action);
            if rc == 0 { ctx.ok(0) } else { ctx.err() }
        }
        // Output queue visibility.
        TIOCOUTQ => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let count = tty::output_queued_bytes(tty_idx);
            if count < 0 {
                ctx.err()
            } else {
                let count = count as i32;
                try_or_err!(ctx, copy_to_user(ptr, &count));
                ctx.ok(0)
            }
        }
        TIOCEXCL => {
            if tty::set_exclusive(tty_idx, true) == 0 { ctx.ok(0) } else { ctx.err() }
        }
        TIOCNXCL => {
            if tty::set_exclusive(tty_idx, false) == 0 { ctx.ok(0) } else { ctx.err() }
        }
        TIOCGEXCL => {
            require_nonzero!(ctx, arg);
            let ptr = try_or_err!(ctx, UserPtr::<i32>::try_new(arg));
            let state = tty::get_exclusive(tty_idx);
            if state < 0 {
                ctx.err()
            } else {
                let state = state as i32;
                try_or_err!(ctx, copy_to_user(ptr, &state));
                ctx.ok(0)
            }
        }
        _ => ctx.err(),
    }
});
