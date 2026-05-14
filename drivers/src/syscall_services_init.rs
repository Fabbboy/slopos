use slopos_abi::KernelErrno;
use slopos_abi::syscall::TtyIndex;
use slopos_kernel_services::syscall_services::input::{InputServices, register_input_services};
use slopos_kernel_services::syscall_services::tty::{TtyServices, register_tty_services};

use crate::{input_event, tty};

static INPUT_SERVICES: InputServices = InputServices {
    poll: input_event::input_poll,
    drain_batch: input_event::input_drain_batch,
    event_count: input_event::input_event_count,
    set_keyboard_focus: input_event::input_set_keyboard_focus,
    set_pointer_focus: input_event::input_set_pointer_focus,
    set_pointer_focus_with_offset: input_event::input_set_pointer_focus_with_offset,
    request_close: input_event::input_request_close,
    send_configure: input_event::input_send_configure,
    get_pointer_focus: input_event::input_get_pointer_focus,
    get_pointer_position: input_event::input_get_pointer_position,
    get_button_state: input_event::input_get_button_state,
    get_modifier_state: input_event::input_get_modifier_state,
    clipboard_copy: input_event::clipboard_copy,
    clipboard_paste: input_event::clipboard_paste,
    register_compositor: input_event::input_register_compositor,
};

fn tty_read_adapter(
    tty_index: TtyIndex,
    buf: *mut u8,
    max: usize,
    nonblock: bool,
) -> Result<usize, tty::TtyError> {
    tty_read_with_attach_adapter(tty_index, buf, max, nonblock, true)
}

fn tty_read_with_attach_adapter(
    tty_index: TtyIndex,
    buf: *mut u8,
    max: usize,
    nonblock: bool,
    auto_attach: bool,
) -> Result<usize, tty::TtyError> {
    if buf.is_null() || max == 0 {
        return Ok(0);
    }
    let slice = slopos_ostd::util::ptr_buf::borrow_buf_mut(buf, max);
    tty::read_with_attach(tty_index, slice, nonblock, auto_attach)
}

fn tty_release_controlling_terminal_adapter(
    tty_index: TtyIndex,
    session_id: u32,
) -> Result<bool, tty::TtyError> {
    tty::release_controlling_terminal(tty_index, session_id)
}

fn tty_grantpt_adapter(tty_index: TtyIndex) -> Result<(), tty::TtyError> {
    tty::set_pty_lock(tty_index, false)
}

fn tty_ptsname_adapter(tty_index: TtyIndex, buf: *mut u8, buflen: usize) -> i32 {
    if buf.is_null() || buflen == 0 {
        return tty::TtyError::InvalidArg.to_errno();
    }

    let pty_num = match tty::get_pty_number(tty_index) {
        Ok(n) => n,
        Err(e) => return e.to_errno(),
    };

    let mut path = [0u8; 32];
    let prefix = b"/dev/pts/";
    let mut len = 0usize;
    path[..prefix.len()].copy_from_slice(prefix);
    len += prefix.len();

    if pty_num == 0 {
        path[len] = b'0';
        len += 1;
    } else {
        let mut num = pty_num;
        let mut rev = [0u8; 10];
        let mut rev_len = 0usize;
        while num > 0 {
            rev[rev_len] = b'0' + (num % 10) as u8;
            rev_len += 1;
            num /= 10;
        }
        for i in (0..rev_len).rev() {
            path[len] = rev[i];
            len += 1;
        }
    }

    if len + 1 > buflen {
        return tty::TtyError::InvalidArg.to_errno();
    }

    slopos_ostd::util::ptr_buf::copy_with_nul_terminator(buf, &path[..len], len);
    0
}

fn tty_write_bytes_adapter(
    tty_index: TtyIndex,
    buf: *const u8,
    len: usize,
    nonblock: bool,
) -> Result<usize, tty::TtyError> {
    if buf.is_null() || len == 0 {
        return Ok(0);
    }
    let data = slopos_ostd::util::ptr_buf::borrow_buf(buf, len);
    tty::write(tty_index, data, nonblock)
}

fn tty_poll_sleep_on_adapter(slots: *const u8, count: usize) {
    if slots.is_null() || count == 0 {
        tty::poll_sleep();
        return;
    }
    let slot_slice = slopos_ostd::util::ptr_buf::borrow_buf(slots, count);
    tty::poll_sleep_on(slot_slice);
}

static TTY_SERVICES: TtyServices = TtyServices {
    read_cooked: tty_read_adapter,
    read_cooked_with_attach: tty_read_with_attach_adapter,
    has_cooked_data: tty::has_data,
    set_termios: tty::set_termios,
    set_termios_wait: tty::set_termios_wait,
    set_termios_flush: tty::set_termios_flush,
    get_termios: tty::get_termios,
    set_ldisc: tty::set_ldisc,
    get_ldisc: tty::get_ldisc,
    get_winsize: tty::get_winsize,
    set_winsize: tty::set_winsize,
    set_compositor_focus: tty::set_compositor_focus,
    get_compositor_focus: tty::get_compositor_focus,
    switch_active_tty: tty::switch_active_tty,
    set_foreground_pgrp: tty::set_foreground_pgrp,
    get_foreground_pgrp: tty::get_foreground_pgrp,
    get_session_id: tty::get_session_id,
    set_foreground_pgrp_checked: tty::set_foreground_pgrp_checked,
    write_bytes: tty_write_bytes_adapter,
    attach_session: tty::attach_session,
    acquire_controlling_terminal: tty::acquire_controlling_terminal,
    release_controlling_terminal: tty_release_controlling_terminal_adapter,
    default_console_tty: tty::default_console_tty,
    open_ref: tty::open_ref,
    close_ref: tty::close_ref,
    hangup: tty::hangup,
    is_hung_up: tty::is_hung_up,
    alloc_pty: tty::pty_alloc,
    grantpt: tty_grantpt_adapter,
    ptsname: tty_ptsname_adapter,
    get_pty_number: tty::get_pty_number,
    is_pty_slave: tty::is_pty_slave,
    open_pty_slave: tty::pty_open_slave,
    open_pty_peer: tty::pty_open_peer,
    detach_session_by_id: tty::detach_session_by_id,
    poll_events: tty::poll_events,
    poll_sleep: tty::poll_sleep,
    poll_sleep_on: tty_poll_sleep_on_adapter,
    poll_enqueue: tty::poll_enqueue,
    poll_dequeue: tty::poll_dequeue,
    detach_controlling_terminal: tty::detach_controlling_terminal,
    bytes_available: tty::bytes_available,
    set_pty_lock: tty::set_pty_lock,
    get_pty_lock: tty::get_pty_lock,
    set_packet_mode: tty::set_packet_mode,
    tcflush: tty::tcflush,
    tcsbrk: tty::tcsbrk,
    tcxonc: tty::tcxonc,
    output_queued_bytes: tty::output_queued_bytes,
    set_exclusive: tty::set_exclusive,
    get_exclusive: tty::get_exclusive,
};

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
}
