#![allow(clippy::too_many_arguments)]

use core::ffi::c_int;

use slopos_abi::Errno;
use slopos_abi::signal::SIGTTOU;
use slopos_abi::syscall::{
    FIONREAD, POLLIN, POLLOUT, TCFLSH, TCGETS, TCSBRK, TCSETS, TCSETSF, TCSETSW, TCXONC, TIOCEXCL,
    TIOCGETD, TIOCGEXCL, TIOCGPGRP, TIOCGPTLCK, TIOCGPTN, TIOCGPTPEER, TIOCGSID, TIOCGWINSZ,
    TIOCNOTTY, TIOCNXCL, TIOCOUTQ, TIOCPKT, TIOCSCTTY, TIOCSETD, TIOCSPGRP, TIOCSPTLCK, UserPollFd,
    UserTermios, UserTimeval, UserWinsize,
};

use slopos_fs::fileio::{
    file_get_tty_index, file_open_tty_fd, file_poll_fused, file_poll_unfused_by_idx,
};

use slopos_kernel_services::driver_runtime::{
    current_task_pgid, is_current_signal_blocked_or_ignored, is_pgrp_orphaned, signal_process_group,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};

use crate::syscall::args::{Fd, UserPtr};

const SELECT_MAX_FDS: usize = 256;

// ioctl helper return: Ok(value) → SyscallResult::Ok(value); Err(()) → EINVAL.
type IoctlResult = Result<u64, ()>;

#[inline(never)]
fn ioctl_termios(tty_idx: slopos_abi::syscall::TtyIndex, cmd: u64, arg: u64) -> IoctlResult {
    if arg == 0 {
        return Err(());
    }
    let ptr = MmUserPtr::<UserTermios>::try_new(arg).map_err(|_| ())?;
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
    let ptr = MmUserPtr::<UserWinsize>::try_new(arg).map_err(|_| ())?;
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
            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let pty_number = tty::get_pty_number(tty_idx).map_err(|_| ())?;
            copy_to_user(ptr, &pty_number).map_err(|_| ())?;
            Ok(0)
        }
        TIOCGPTPEER => {
            let (peer_tty, backing) = tty::open_pty_peer(tty_idx).map_err(|_| ())?;
            let new_fd = file_open_tty_fd(pid, peer_tty, arg as u32, backing);
            if new_fd < 0 {
                Err(())
            } else {
                Ok(new_fd as u64)
            }
        }
        TIOCSPTLCK => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let val = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_pty_lock(tty_idx, val != 0)
                .map(|_| 0)
                .map_err(|_| ())
        }
        TIOCGPTLCK => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let state = tty::get_pty_lock(tty_idx).map_err(|_| ())?;
            let state_i = i32::from(state);
            copy_to_user(ptr, &state_i).map_err(|_| ())?;
            Ok(0)
        }
        TIOCPKT => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
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
            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let ldisc_id = tty::get_ldisc(tty_idx).unwrap_or(slopos_abi::syscall::N_TTY);
            copy_to_user(ptr, &ldisc_id).map_err(|_| ())?;
            Ok(0)
        }
        TIOCSETD => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| ())?;
            let ldisc_id = copy_from_user(ptr).map_err(|_| ())?;
            tty::set_ldisc(tty_idx, ldisc_id).map(|_| 0).map_err(|_| ())
        }
        FIONREAD => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
            let count = tty::bytes_available(tty_idx).map_err(|_| ())? as i32;
            copy_to_user(ptr, &count).map_err(|_| ())?;
            Ok(0)
        }
        TIOCOUTQ => {
            if arg == 0 {
                return Err(());
            }
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
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
            let ptr = MmUserPtr::<i32>::try_new(arg).map_err(|_| ())?;
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

define_syscall!(syscall_poll
    (ctx, base_ptr: u64, nfds: u64, timeout_ms_raw: i64)
    requires(let task_id: task_id, let pid: process_id)
    -> Result<u64, Errno>
{
    let nfds = nfds as usize;
    let timeout_ms = timeout_ms_raw;

    if base_ptr == 0 || nfds > SELECT_MAX_FDS {
        return Err(Errno::EINVAL);
    }

    let start_ms = slopos_kernel_services::platform::get_time_ms();

    #[derive(slopos_ostd::Zeroable)]
    #[repr(C)]
    struct PollScratch {
        cached_revents: [u16; SELECT_MAX_FDS],
        poll_fds: [UserPollFd; SELECT_MAX_FDS],
        registered_ofis: [u64; SELECT_MAX_FDS],
    }
    let mut scratch_box = slopos_ostd::KBox::<PollScratch>::zeroed().map_err(|_| Errno::ENOMEM)?;
    let scratch: &mut PollScratch = &mut *scratch_box;
    let cached_revents = &mut scratch.cached_revents;
    let poll_fds: &mut [UserPollFd] = &mut scratch.poll_fds;
    let registered_ofis = &mut scratch.registered_ofis;

    loop {
        for idx in 0..nfds {
            let user_ptr = MmUserPtr::<UserPollFd>::try_new(
                base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64,
            )
            .map_err(|_| Errno::EFAULT)?;
            poll_fds[idx] = copy_from_user(user_ptr).map_err(|_| Errno::EFAULT)?;
        }

        let mut ready_count = 0u64;
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
                    registered_ofis[reg_count] = result.open_file_token;
                    reg_count += 1;
                }
            }
        }

        let cleanup = |reg_count: usize, ofis: &[u64]| {
            for &ofi in &ofis[..reg_count] {
                file_poll_unfused_by_idx(ofi);
            }
        };
        let writeback = |cached: &[u16], nfds: usize| -> Result<(), Errno> {
            const REVENTS_OFFSET: u64 = 6;
            for idx in 0..nfds {
                let revents_addr =
                    base_ptr + (idx * core::mem::size_of::<UserPollFd>()) as u64 + REVENTS_OFFSET;
                let revents_ptr =
                    MmUserBytes::try_new(revents_addr, 2).map_err(|_| Errno::EFAULT)?;
                copy_bytes_to_user(revents_ptr, &cached[idx].to_ne_bytes())
                    .map_err(|_| Errno::EFAULT)?;
            }
            Ok(())
        };

        if ready_count > 0 {
            cleanup(reg_count, registered_ofis);
            writeback(cached_revents, nfds)?;
            return Ok(ready_count);
        }

        if timeout_ms == 0 {
            cleanup(reg_count, registered_ofis);
            writeback(cached_revents, nfds)?;
            return Ok(0);
        }
        if timeout_ms > 0 {
            let now = slopos_kernel_services::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                cleanup(reg_count, registered_ofis);
                writeback(cached_revents, nfds)?;
                return Ok(0);
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
            // Record this iteration's registrations against the task so they
            // are released even if the task is killed while blocked (the
            // post-block `cleanup` below is skipped on SIGKILL). Cleared on
            // the normal wake path right after `cleanup`.
            slopos_fs::fileio::file_poll_track_registrations(
                task_id,
                &registered_ofis[..reg_count],
            );
            slopos_kernel_services::driver_runtime::block_current_task_with_timeout(sleep_ms);
        } else {
            slopos_kernel_services::platform::timer_poll_delay_ms(1);
        }

        cleanup(reg_count, registered_ofis);
        if reg_count > 0 {
            slopos_fs::fileio::file_poll_clear_registrations(task_id);
        }

        if slopos_kernel_services::driver_runtime::has_pending_signal() {
            return Err(Errno::EINTR);
        }
    }
});

define_syscall!(syscall_select
    (ctx, nfds_raw: u64, rd_ptr: u64, wr_ptr: u64, ex_ptr: u64, tv_ptr: u64)
    requires(let task_id: task_id, let pid: process_id)
    -> Result<u64, Errno>
{
    let nfds = nfds_raw as usize;
    if nfds > SELECT_MAX_FDS {
        return Err(Errno::EINVAL);
    }

    let bytes_len = fdset_bytes_len(nfds);
    const FDSET_BYTES: usize = SELECT_MAX_FDS / 8;
    #[derive(slopos_ostd::Zeroable)]
    #[repr(C)]
    struct SelectScratch {
        read_in: [u8; FDSET_BYTES],
        write_in: [u8; FDSET_BYTES],
        except_in: [u8; FDSET_BYTES],
        read_out: [u8; FDSET_BYTES],
        write_out: [u8; FDSET_BYTES],
        except_out: [u8; FDSET_BYTES],
        registered_ofis: [u64; SELECT_MAX_FDS],
    }
    let mut scratch_box = slopos_ostd::KBox::<SelectScratch>::zeroed().map_err(|_| Errno::ENOMEM)?;
    let scratch: &mut SelectScratch = &mut *scratch_box;

    if rd_ptr != 0 {
        let in_bytes = MmUserBytes::try_new(rd_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
        let copied = copy_bytes_from_user(in_bytes, &mut scratch.read_in[..bytes_len])
            .map_err(|_| Errno::EFAULT)?;
        if copied != bytes_len {
            return Err(Errno::EFAULT);
        }
    }
    if wr_ptr != 0 {
        let in_bytes = MmUserBytes::try_new(wr_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
        let copied = copy_bytes_from_user(in_bytes, &mut scratch.write_in[..bytes_len])
            .map_err(|_| Errno::EFAULT)?;
        if copied != bytes_len {
            return Err(Errno::EFAULT);
        }
    }
    if ex_ptr != 0 {
        let in_bytes = MmUserBytes::try_new(ex_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
        let copied = copy_bytes_from_user(in_bytes, &mut scratch.except_in[..bytes_len])
            .map_err(|_| Errno::EFAULT)?;
        if copied != bytes_len {
            return Err(Errno::EFAULT);
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

    let timeout_ms = if tv_ptr == 0 {
        -1i64
    } else {
        let tv_ptr_obj = MmUserPtr::<UserTimeval>::try_new(tv_ptr).map_err(|_| Errno::EFAULT)?;
        let tv = copy_from_user(tv_ptr_obj).map_err(|_| Errno::EFAULT)?;
        if tv.tv_sec < 0 || tv.tv_usec < 0 {
            return Err(Errno::EINVAL);
        }
        tv.tv_sec
            .saturating_mul(1000)
            .saturating_add(tv.tv_usec / 1000)
    };

    let start_ms = slopos_kernel_services::platform::get_time_ms();

    #[inline(never)]
    fn copy_out_select_results(
        read_ptr: u64,
        write_ptr: u64,
        except_ptr: u64,
        read_out: &[u8],
        write_out: &[u8],
        except_out: &[u8],
        bytes_len: usize,
    ) -> Result<(), Errno> {
        if read_ptr != 0 {
            let out = MmUserBytes::try_new(read_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
            copy_bytes_to_user(out, &read_out[..bytes_len]).map_err(|_| Errno::EFAULT)?;
        }
        if write_ptr != 0 {
            let out = MmUserBytes::try_new(write_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
            copy_bytes_to_user(out, &write_out[..bytes_len]).map_err(|_| Errno::EFAULT)?;
        }
        if except_ptr != 0 {
            let out = MmUserBytes::try_new(except_ptr, bytes_len).map_err(|_| Errno::EFAULT)?;
            copy_bytes_to_user(out, &except_out[..bytes_len]).map_err(|_| Errno::EFAULT)?;
        }
        Ok(())
    }

    loop {
        read_out[..bytes_len].fill(0);
        write_out[..bytes_len].fill(0);
        except_out[..bytes_len].fill(0);

        let mut ready = 0u64;
        let mut reg_count = 0usize;

        for fd in 0..nfds {
            let want_r = rd_ptr != 0 && fdset_test(&read_in[..bytes_len], fd);
            let want_w = wr_ptr != 0 && fdset_test(&write_in[..bytes_len], fd);
            let want_e = ex_ptr != 0 && fdset_test(&except_in[..bytes_len], fd);
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
                registered_ofis[reg_count] = result.open_file_token;
                reg_count += 1;
            }
        }

        let cleanup = |reg_count: usize, ofis: &[u64]| {
            for &ofi in &ofis[..reg_count] {
                file_poll_unfused_by_idx(ofi);
            }
        };

        if ready > 0 {
            cleanup(reg_count, registered_ofis);
            copy_out_select_results(rd_ptr, wr_ptr, ex_ptr, read_out, write_out, except_out, bytes_len)?;
            return Ok(ready);
        }

        if timeout_ms == 0 {
            cleanup(reg_count, registered_ofis);
            copy_out_select_results(rd_ptr, wr_ptr, ex_ptr, read_out, write_out, except_out, bytes_len)?;
            return Ok(0);
        }
        if timeout_ms > 0 {
            let now = slopos_kernel_services::platform::get_time_ms();
            if now.wrapping_sub(start_ms) as i64 >= timeout_ms {
                cleanup(reg_count, registered_ofis);
                copy_out_select_results(rd_ptr, wr_ptr, ex_ptr, read_out, write_out, except_out, bytes_len)?;
                return Ok(0);
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
            // See syscall_poll: track registrations so a SIGKILL while blocked
            // can't leak the per-fd poll incref.
            slopos_fs::fileio::file_poll_track_registrations(
                task_id,
                &registered_ofis[..reg_count],
            );
            slopos_kernel_services::driver_runtime::block_current_task_with_timeout(sleep_ms);
        } else {
            slopos_kernel_services::platform::timer_poll_delay_ms(1);
        }

        cleanup(reg_count, registered_ofis);
        if reg_count > 0 {
            slopos_fs::fileio::file_poll_clear_registrations(task_id);
        }

        if slopos_kernel_services::driver_runtime::has_pending_signal() {
            return Err(Errno::EINTR);
        }
    }
});

define_syscall!(syscall_ioctl
    (ctx, fd: Fd, cmd: u64, arg: u64)
    requires(let task_id: task_id, let pid: process_id)
    -> Result<u64, Errno>
{
    let tty_idx = file_get_tty_index(pid, fd.raw()).ok_or(Errno::EINVAL)?;

    let hangup_safe = matches!(cmd, TIOCGPGRP | TIOCSPGRP | TIOCGSID | TIOCNOTTY);
    if !hangup_safe && tty::is_hung_up(tty_idx) {
        return Err(Errno::EIO);
    }

    match cmd {
        TCGETS | TCSETS | TCSETSW | TCSETSF => ioctl_termios(tty_idx, cmd, arg).map_err(|_| Errno::EINVAL),
        TIOCGWINSZ | slopos_abi::syscall::TIOCSWINSZ => ioctl_winsize(tty_idx, cmd, arg).map_err(|_| Errno::EINVAL),
        TIOCGPTN | TIOCGPTPEER | TIOCSPTLCK | TIOCGPTLCK | TIOCPKT => {
            ioctl_pty(tty_idx, cmd, arg, pid).map_err(|_| Errno::EINVAL)
        }
        TIOCGETD | TIOCSETD | FIONREAD | TIOCOUTQ | TCFLSH | TCSBRK | TCXONC | TIOCEXCL
        | TIOCNXCL | TIOCGEXCL => ioctl_misc(tty_idx, cmd, arg).map_err(|_| Errno::EINVAL),
        TIOCGPGRP => {
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| Errno::EFAULT)?;
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx).unwrap_or(0);
            copy_to_user(ptr, &fg_pgrp).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        TIOCSPGRP => {
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            let task_ptr = slopos_sched::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return Err(Errno::EINVAL);
            }
            let task = slopos_sched::task::task_borrow(task_ptr).ok_or(Errno::EINVAL)?;
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return Err(Errno::EINVAL),
            }

            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| Errno::EFAULT)?;
            let pgrp = copy_from_user(ptr).map_err(|_| Errno::EFAULT)?;
            let caller_pgid = current_task_pgid();
            let caller_sid = slopos_sched::scheduler::current_task_sid();

            let tty_sid = tty::get_session_id(tty_idx).unwrap_or(0);
            let fg_pgrp = tty::get_foreground_pgrp(tty_idx).unwrap_or(0);
            if tty_sid != 0
                && tty_sid == caller_sid
                && caller_pgid != 0
                && fg_pgrp != 0
                && caller_pgid != fg_pgrp
                && !is_current_signal_blocked_or_ignored(SIGTTOU)
            {
                if is_pgrp_orphaned(caller_pgid, caller_sid) {
                    return Err(Errno::EINVAL);
                }
                let _ = signal_process_group(caller_pgid, SIGTTOU);
                return Err(Errno::EINVAL);
            }

            tty::set_foreground_pgrp_checked(tty_idx, pgrp, caller_sid)
                .map(|_| 0)
                .map_err(|_| Errno::EINVAL)
        }
        TIOCSCTTY => {
            if arg != 0 {
                return Err(Errno::EINVAL);
            }

            let task_ptr = slopos_sched::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return Err(Errno::EINVAL);
            }

            let task = slopos_sched::task::task_borrow_mut(task_ptr).ok_or(Errno::EINVAL)?;
            if task.sid == 0 || task.sid != task.task_id {
                return Err(Errno::EINVAL);
            }

            if let Some(current_tty) = task.controlling_tty {
                if current_tty == tty_idx {
                    return Ok(0);
                }
                return Err(Errno::EINVAL);
            }

            let tty_sid = tty::get_session_id(tty_idx).unwrap_or(0);
            if tty_sid != 0 && tty_sid != task.sid {
                return Err(Errno::EINVAL);
            }

            if tty::acquire_controlling_terminal(tty_idx, task.sid, task.pgid).is_err() {
                return Err(Errno::EINVAL);
            }
            task.controlling_tty = Some(tty_idx);
            Ok(0)
        }
        TIOCGSID => {
            if arg == 0 {
                return Err(Errno::EINVAL);
            }
            let task_ptr = slopos_sched::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return Err(Errno::EINVAL);
            }
            let task = slopos_sched::task::task_borrow(task_ptr).ok_or(Errno::EINVAL)?;
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return Err(Errno::EINVAL),
            }
            let ptr = MmUserPtr::<u32>::try_new(arg).map_err(|_| Errno::EFAULT)?;
            let sid = tty::get_session_id(tty_idx).unwrap_or(0);
            copy_to_user(ptr, &sid).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        TIOCNOTTY => {
            let task_ptr = slopos_sched::task::task_find_by_id(task_id);
            if task_ptr.is_null() {
                return Err(Errno::EINVAL);
            }

            let task = slopos_sched::task::task_borrow_mut(task_ptr).ok_or(Errno::EINVAL)?;
            match task.controlling_tty {
                Some(ctty) if ctty == tty_idx => {}
                _ => return Err(Errno::EINVAL),
            }

            let caller_sid = task.sid;
            let is_session_leader = task.sid != 0 && task.sid == task.task_id;
            task.controlling_tty = None;
            let _ = tty::detach_controlling_terminal(tty_idx, caller_sid, is_session_leader);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
});

// Suppress unused warning for UserPtr re-export.
#[allow(dead_code)]
type _Unused<T> = UserPtr<T>;
