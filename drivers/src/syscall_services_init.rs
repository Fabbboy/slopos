use slopos_lib::kernel_services::syscall_services::input::{
    InputServices, register_input_services,
};
use slopos_lib::kernel_services::syscall_services::tty::{TtyServices, register_tty_services};

use crate::{input_event, tty};

// =============================================================================
// Input services
// =============================================================================
//
// Most fields point directly at the driver implementation.  The three adapters
// below exist only because the driver returns a different type than the service
// interface requires.

/// Adapter: driver returns `u32`, service expects `usize`.
fn input_event_count_adapter(task_id: u32) -> usize {
    input_event::input_event_count(task_id) as usize
}

/// Adapter: driver returns `bool`, service expects `i32` (0 = ok, -1 = fail).
fn input_request_close_adapter(task_id: u32, timestamp_ms: u64) -> i32 {
    if input_event::input_request_close(task_id, timestamp_ms) {
        0
    } else {
        -1
    }
}

/// Adapter: driver returns `u8`, service expects `u32`.
fn input_get_button_state_adapter() -> u32 {
    input_event::input_get_button_state() as u32
}

static INPUT_SERVICES: InputServices = InputServices {
    poll: input_event::input_poll,
    drain_batch: input_event::input_drain_batch,
    event_count: input_event_count_adapter,
    set_keyboard_focus: input_event::input_set_keyboard_focus,
    set_pointer_focus: input_event::input_set_pointer_focus,
    set_pointer_focus_with_offset: input_event::input_set_pointer_focus_with_offset,
    request_close: input_request_close_adapter,
    get_pointer_focus: input_event::input_get_pointer_focus,
    get_pointer_position: input_event::input_get_pointer_position,
    get_button_state: input_get_button_state_adapter,
    clipboard_copy: input_event::clipboard_copy,
    clipboard_paste: input_event::clipboard_paste,
};

// =============================================================================
// TTY services — adapters bridging TtyServices (TtyIndex) to per-TTY API.
// =============================================================================

use slopos_abi::syscall::TtyIndex;

fn tty_read_adapter(tty_index: TtyIndex, buf: *mut u8, max: usize, nonblock: bool) -> isize {
    tty_read_with_attach_adapter(tty_index, buf, max, nonblock, true)
}

fn tty_read_with_attach_adapter(
    tty_index: TtyIndex,
    buf: *mut u8,
    max: usize,
    nonblock: bool,
    auto_attach: bool,
) -> isize {
    if buf.is_null() || max == 0 {
        return 0;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, max) };
    match tty::read_with_attach(tty_index, slice, nonblock, auto_attach) {
        Ok(n) => n as isize,
        Err(tty::TtyError::WouldBlock) => -11,
        Err(tty::TtyError::HungUp) => -5,
        Err(tty::TtyError::CrossSessionDenied) => -5, // EIO
        Err(tty::TtyError::Restart) => -512,          // ERESTARTSYS (internal)
        Err(_) => -1,
    }
}

fn tty_has_cooked_data_adapter(tty_index: TtyIndex) -> bool {
    tty::has_data(tty_index)
}

fn tty_set_termios_adapter(tty_index: TtyIndex, t: *const slopos_abi::syscall::UserTermios) {
    if t.is_null() {
        return;
    }
    let val = unsafe { &*t };
    let _ = tty::set_termios(tty_index, val);
}

fn tty_set_termios_wait_adapter(
    tty_index: TtyIndex,
    t: *const slopos_abi::syscall::UserTermios,
) -> i32 {
    if t.is_null() {
        return -1;
    }
    let val = unsafe { &*t };
    match tty::set_termios_wait(tty_index, val) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_set_termios_flush_adapter(
    tty_index: TtyIndex,
    t: *const slopos_abi::syscall::UserTermios,
) -> i32 {
    if t.is_null() {
        return -1;
    }
    let val = unsafe { &*t };
    match tty::set_termios_flush(tty_index, val) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_termios_adapter(tty_index: TtyIndex, t: *mut slopos_abi::syscall::UserTermios) {
    if t.is_null() {
        return;
    }
    if let Ok(val) = tty::get_termios(tty_index) {
        unsafe { *t = val };
    }
}

fn tty_set_ldisc_adapter(tty_index: TtyIndex, ldisc_id: u32) -> i32 {
    match tty::set_ldisc(tty_index, ldisc_id) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_ldisc_adapter(tty_index: TtyIndex) -> u32 {
    tty::get_ldisc(tty_index).unwrap_or(slopos_abi::syscall::N_TTY)
}

fn tty_get_winsize_adapter(tty_index: TtyIndex, ws: *mut slopos_abi::syscall::UserWinsize) {
    if ws.is_null() {
        return;
    }
    if let Ok(val) = tty::get_winsize(tty_index) {
        unsafe { *ws = val };
    }
}

fn tty_set_winsize_adapter(tty_index: TtyIndex, ws: *const slopos_abi::syscall::UserWinsize) {
    if ws.is_null() {
        return;
    }
    let val = unsafe { &*ws };
    let _ = tty::set_winsize(tty_index, val);
}

fn tty_set_compositor_focus_adapter(target: u32) -> i32 {
    match tty::set_compositor_focus(target) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_compositor_focus_adapter() -> u32 {
    tty::get_compositor_focus().unwrap_or(0)
}

fn tty_switch_active_tty_adapter(tty_index: TtyIndex) -> i32 {
    match tty::switch_active_tty(tty_index) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_set_foreground_pgrp_adapter(tty_index: TtyIndex, pgid: u32) -> i32 {
    match tty::set_foreground_pgrp(tty_index, pgid) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_foreground_pgrp_adapter(tty_index: TtyIndex) -> u32 {
    tty::get_foreground_pgrp(tty_index).unwrap_or(0)
}

fn tty_get_session_id_adapter(tty_index: TtyIndex) -> u32 {
    tty::get_session_id(tty_index).unwrap_or(0)
}

fn tty_set_foreground_pgrp_checked_adapter(tty_index: TtyIndex, pgid: u32, caller_sid: u32) -> i32 {
    match tty::set_foreground_pgrp_checked(tty_index, pgid, caller_sid) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_detach_session_by_id_adapter(session_id: u32) {
    tty::detach_session_by_id(session_id)
}

fn tty_attach_session_adapter(tty_index: TtyIndex, leader_pid: u32, leader_pgid: u32) {
    tty::attach_session(tty_index, leader_pid, leader_pgid)
}

fn tty_acquire_controlling_terminal_adapter(
    tty_index: TtyIndex,
    session_leader: u32,
    session_pgid: u32,
) -> i32 {
    match tty::acquire_controlling_terminal(tty_index, session_leader, session_pgid) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_release_controlling_terminal_adapter(tty_index: TtyIndex, session_id: u32) -> i32 {
    match tty::release_controlling_terminal(tty_index, session_id) {
        Ok(true) => 0,
        Ok(false) => -1,
        Err(_) => -1,
    }
}

fn tty_default_console_tty_adapter() -> TtyIndex {
    tty::default_console_tty()
}

fn tty_open_ref_adapter(tty_index: TtyIndex) -> i32 {
    match tty::open_ref(tty_index) {
        Ok(n) => n as i32,
        Err(_) => -1,
    }
}

fn tty_close_ref_adapter(tty_index: TtyIndex) -> i32 {
    match tty::close_ref(tty_index) {
        Ok(n) => n as i32,
        Err(_) => -1,
    }
}

fn tty_hangup_adapter(tty_index: TtyIndex) {
    tty::hangup(tty_index)
}

fn tty_is_hung_up_adapter(tty_index: TtyIndex) -> bool {
    tty::is_hung_up(tty_index)
}

fn tty_alloc_pty_adapter() -> i32 {
    match tty::pty_alloc() {
        Ok(idx) => idx.0 as i32,
        Err(_) => -1,
    }
}

fn tty_grantpt_adapter(tty_index: TtyIndex) -> i32 {
    match tty::set_pty_lock(tty_index, false) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_ptsname_adapter(tty_index: TtyIndex, buf: *mut u8, buflen: usize) -> i32 {
    if buf.is_null() || buflen == 0 {
        return -1;
    }

    let pty_num = match tty::get_pty_number(tty_index) {
        Ok(n) => n,
        Err(_) => return -1,
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
        return -1;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf, len);
        *buf.add(len) = 0;
    }
    0
}

fn tty_get_pty_number_adapter(tty_index: TtyIndex) -> i32 {
    match tty::get_pty_number(tty_index) {
        Ok(number) => number as i32,
        Err(_) => -1,
    }
}

fn tty_is_pty_slave_adapter(tty_index: TtyIndex) -> bool {
    tty::is_pty_slave(tty_index)
}

fn tty_open_pty_slave_adapter(tty_index: TtyIndex) -> i32 {
    match tty::pty_open_slave(tty_index) {
        Ok(count) => count as i32,
        Err(_) => -1,
    }
}

fn tty_open_pty_peer_adapter(tty_index: TtyIndex) -> i32 {
    match tty::pty_open_peer(tty_index) {
        Ok(idx) => idx.0 as i32,
        Err(_) => -1,
    }
}

fn tty_write_bytes_adapter(
    tty_index: TtyIndex,
    buf: *const u8,
    len: usize,
    nonblock: bool,
) -> isize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    match tty::write(tty_index, data, nonblock) {
        Ok(n) => n as isize,
        Err(tty::TtyError::WouldBlock) => -11, // EAGAIN
        Err(tty::TtyError::HungUp) => -5,      // EIO
        Err(tty::TtyError::Restart) => -512,   // ERESTARTSYS (internal)
        Err(_) => -1,
    }
}

fn tty_poll_events_adapter(tty_index: TtyIndex, requested: u16) -> u16 {
    tty::poll_events(tty_index, requested)
}

fn tty_poll_sleep_adapter() {
    tty::poll_sleep()
}

fn tty_poll_sleep_on_adapter(slots: *const u8, count: usize) {
    if slots.is_null() || count == 0 {
        tty::poll_sleep();
        return;
    }
    // SAFETY: The caller guarantees `slots` points to `count` valid bytes.
    let slot_slice = unsafe { core::slice::from_raw_parts(slots, count) };
    tty::poll_sleep_on(slot_slice);
}

fn tty_poll_enqueue_adapter(tty_index: TtyIndex) -> bool {
    tty::poll_enqueue(tty_index)
}

fn tty_poll_dequeue_adapter(tty_index: TtyIndex) {
    tty::poll_dequeue(tty_index);
}

fn tty_detach_controlling_terminal_adapter(
    tty_index: TtyIndex,
    caller_sid: u32,
    caller_is_session_leader: bool,
) -> i32 {
    match tty::detach_controlling_terminal(tty_index, caller_sid, caller_is_session_leader) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

fn tty_bytes_available_adapter(tty_index: TtyIndex) -> i32 {
    match tty::bytes_available(tty_index) {
        Ok(n) => n as i32,
        Err(_) => -1,
    }
}

fn tty_set_pty_lock_adapter(tty_index: TtyIndex, locked: bool) -> i32 {
    match tty::set_pty_lock(tty_index, locked) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_pty_lock_adapter(tty_index: TtyIndex) -> i32 {
    match tty::get_pty_lock(tty_index) {
        Ok(locked) => i32::from(locked),
        Err(_) => -1,
    }
}

fn tty_set_packet_mode_adapter(tty_index: TtyIndex, enable: bool) -> i32 {
    match tty::set_packet_mode(tty_index, enable) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// Missing ioctls (TCFLSH, TCSBRK, TCXONC) adapters.
fn tty_tcflush_adapter(tty_index: TtyIndex, queue: i32) -> i32 {
    match tty::tcflush(tty_index, queue) {
        Ok(()) => 0,
        Err(e) => e.to_errno(),
    }
}

fn tty_tcsbrk_adapter(tty_index: TtyIndex, arg: i32) -> i32 {
    match tty::tcsbrk(tty_index, arg) {
        Ok(()) => 0,
        Err(e) => e.to_errno(),
    }
}

fn tty_tcxonc_adapter(tty_index: TtyIndex, action: i32) -> i32 {
    match tty::tcxonc(tty_index, action) {
        Ok(()) => 0,
        Err(e) => e.to_errno(),
    }
}

// Output queue visibility (TIOCOUTQ) adapter.
fn tty_output_queued_bytes_adapter(tty_index: TtyIndex) -> i32 {
    match tty::output_queued_bytes(tty_index) {
        Ok(n) => n as i32,
        Err(_) => -1,
    }
}

fn tty_set_exclusive_adapter(tty_index: TtyIndex, enable: bool) -> i32 {
    match tty::set_exclusive(tty_index, enable) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn tty_get_exclusive_adapter(tty_index: TtyIndex) -> i32 {
    match tty::get_exclusive(tty_index) {
        Ok(v) => i32::from(v),
        Err(_) => -1,
    }
}

static TTY_SERVICES: TtyServices = TtyServices {
    read_cooked: tty_read_adapter,
    read_cooked_with_attach: tty_read_with_attach_adapter,
    has_cooked_data: tty_has_cooked_data_adapter,
    set_termios: tty_set_termios_adapter,
    set_termios_wait: tty_set_termios_wait_adapter,
    set_termios_flush: tty_set_termios_flush_adapter,
    get_termios: tty_get_termios_adapter,
    set_ldisc: tty_set_ldisc_adapter,
    get_ldisc: tty_get_ldisc_adapter,
    get_winsize: tty_get_winsize_adapter,
    set_winsize: tty_set_winsize_adapter,
    set_compositor_focus: tty_set_compositor_focus_adapter,
    get_compositor_focus: tty_get_compositor_focus_adapter,
    switch_active_tty: tty_switch_active_tty_adapter,
    set_foreground_pgrp: tty_set_foreground_pgrp_adapter,
    get_foreground_pgrp: tty_get_foreground_pgrp_adapter,
    get_session_id: tty_get_session_id_adapter,
    set_foreground_pgrp_checked: tty_set_foreground_pgrp_checked_adapter,
    detach_session_by_id: tty_detach_session_by_id_adapter,
    write_bytes: tty_write_bytes_adapter,
    attach_session: tty_attach_session_adapter,
    acquire_controlling_terminal: tty_acquire_controlling_terminal_adapter,
    release_controlling_terminal: tty_release_controlling_terminal_adapter,
    default_console_tty: tty_default_console_tty_adapter,
    open_ref: tty_open_ref_adapter,
    close_ref: tty_close_ref_adapter,
    hangup: tty_hangup_adapter,
    is_hung_up: tty_is_hung_up_adapter,
    alloc_pty: tty_alloc_pty_adapter,
    grantpt: tty_grantpt_adapter,
    ptsname: tty_ptsname_adapter,
    get_pty_number: tty_get_pty_number_adapter,
    is_pty_slave: tty_is_pty_slave_adapter,
    open_pty_slave: tty_open_pty_slave_adapter,
    open_pty_peer: tty_open_pty_peer_adapter,
    poll_events: tty_poll_events_adapter,
    poll_sleep: tty_poll_sleep_adapter,
    poll_sleep_on: tty_poll_sleep_on_adapter,
    poll_enqueue: tty_poll_enqueue_adapter,
    poll_dequeue: tty_poll_dequeue_adapter,
    detach_controlling_terminal: tty_detach_controlling_terminal_adapter,
    bytes_available: tty_bytes_available_adapter,
    set_pty_lock: tty_set_pty_lock_adapter,
    get_pty_lock: tty_get_pty_lock_adapter,
    set_packet_mode: tty_set_packet_mode_adapter,
    tcflush: tty_tcflush_adapter,
    tcsbrk: tty_tcsbrk_adapter,
    tcxonc: tty_tcxonc_adapter,
    output_queued_bytes: tty_output_queued_bytes_adapter,
    set_exclusive: tty_set_exclusive_adapter,
    get_exclusive: tty_get_exclusive_adapter,
};

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
}
