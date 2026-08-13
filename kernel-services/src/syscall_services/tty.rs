slopos_service_core::define_service! {
    tty => TtyServices {
        read_cooked(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, max: usize, nonblock: bool) -> Result<usize, slopos_abi::tty_error::TtyError>;
        read_cooked_with_attach(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, max: usize, nonblock: bool, auto_attach: bool) -> Result<usize, slopos_abi::tty_error::TtyError>;
        has_cooked_data(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        @no_wrapper set_termios(tty_index: slopos_abi::syscall::TtyIndex, t: &slopos_abi::syscall::UserTermios) -> Result<(), slopos_abi::tty_error::TtyError>;
        @no_wrapper set_termios_wait(tty_index: slopos_abi::syscall::TtyIndex, t: &slopos_abi::syscall::UserTermios) -> Result<(), slopos_abi::tty_error::TtyError>;
        @no_wrapper set_termios_flush(tty_index: slopos_abi::syscall::TtyIndex, t: &slopos_abi::syscall::UserTermios) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_termios(tty_index: slopos_abi::syscall::TtyIndex) -> Result<slopos_abi::syscall::UserTermios, slopos_abi::tty_error::TtyError>;
        set_ldisc(tty_index: slopos_abi::syscall::TtyIndex, ldisc_id: u32) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_ldisc(tty_index: slopos_abi::syscall::TtyIndex) -> Result<u32, slopos_abi::tty_error::TtyError>;
        get_winsize(tty_index: slopos_abi::syscall::TtyIndex) -> Result<slopos_abi::syscall::UserWinsize, slopos_abi::tty_error::TtyError>;
        @no_wrapper set_winsize(tty_index: slopos_abi::syscall::TtyIndex, ws: &slopos_abi::syscall::UserWinsize) -> Result<(), slopos_abi::tty_error::TtyError>;
        set_compositor_focus(target: u32) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_compositor_focus() -> Result<u32, slopos_abi::tty_error::TtyError>;
        switch_active_tty(tty_index: slopos_abi::syscall::TtyIndex) -> Result<(), slopos_abi::tty_error::TtyError>;
        set_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex) -> Result<u32, slopos_abi::tty_error::TtyError>;
        get_session_id(tty_index: slopos_abi::syscall::TtyIndex) -> Result<u32, slopos_abi::tty_error::TtyError>;
        set_foreground_pgrp_checked(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32, caller_sid: u32) -> Result<(), slopos_abi::tty_error::TtyError>;
        write_bytes(tty_index: slopos_abi::syscall::TtyIndex, buf: *const u8, len: usize, nonblock: bool) -> Result<usize, slopos_abi::tty_error::TtyError>;
        attach_session(tty_index: slopos_abi::syscall::TtyIndex, leader_pid: u32, leader_pgid: u32);
        acquire_controlling_terminal(tty_index: slopos_abi::syscall::TtyIndex, fg: slopos_ostd::KWeak<slopos_ostd::task::ProcessGroup>) -> Result<(), slopos_abi::tty_error::TtyError>;
        release_controlling_terminal(tty_index: slopos_abi::syscall::TtyIndex, session_id: u32) -> Result<bool, slopos_abi::tty_error::TtyError>;
        default_console_tty() -> slopos_abi::syscall::TtyIndex;
        open_tty(tty_index: slopos_abi::syscall::TtyIndex) -> Result<slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking>, slopos_abi::tty_error::TtyError>;
        hangup(tty_index: slopos_abi::syscall::TtyIndex);
        is_hung_up(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        alloc_pty(account: slopos_ostd::process::AccountId) -> Result<(slopos_abi::syscall::TtyIndex, slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking>), slopos_abi::tty_error::TtyError>;
        grantpt(tty_index: slopos_abi::syscall::TtyIndex) -> Result<(), slopos_abi::tty_error::TtyError>;
        ptsname(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, buflen: usize) -> i32;
        get_pty_number(tty_index: slopos_abi::syscall::TtyIndex) -> Result<u32, slopos_abi::tty_error::TtyError>;
        is_pty_slave(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        open_pty_slave(tty_index: slopos_abi::syscall::TtyIndex) -> Result<slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking>, slopos_abi::tty_error::TtyError>;
        open_pty_peer(tty_index: slopos_abi::syscall::TtyIndex) -> Result<(slopos_abi::syscall::TtyIndex, slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking>), slopos_abi::tty_error::TtyError>;
        detach_session_by_id(session_id: u32);
        poll_events(tty_index: slopos_abi::syscall::TtyIndex, requested: u16) -> u16;
        poll_sleep();
        poll_sleep_on(slots: *const u8, count: usize);
        poll_enqueue(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        poll_dequeue(tty_index: slopos_abi::syscall::TtyIndex);
        detach_controlling_terminal(tty_index: slopos_abi::syscall::TtyIndex, caller_sid: u32, caller_is_session_leader: bool) -> Result<bool, slopos_abi::tty_error::TtyError>;
        bytes_available(tty_index: slopos_abi::syscall::TtyIndex) -> Result<usize, slopos_abi::tty_error::TtyError>;
        set_pty_lock(tty_index: slopos_abi::syscall::TtyIndex, locked: bool) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_pty_lock(tty_index: slopos_abi::syscall::TtyIndex) -> Result<bool, slopos_abi::tty_error::TtyError>;
        set_packet_mode(tty_index: slopos_abi::syscall::TtyIndex, enable: bool) -> Result<(), slopos_abi::tty_error::TtyError>;
        tcflush(tty_index: slopos_abi::syscall::TtyIndex, queue: i32) -> Result<(), slopos_abi::tty_error::TtyError>;
        tcsbrk(tty_index: slopos_abi::syscall::TtyIndex, arg: i32) -> Result<(), slopos_abi::tty_error::TtyError>;
        tcxonc(tty_index: slopos_abi::syscall::TtyIndex, action: i32) -> Result<(), slopos_abi::tty_error::TtyError>;
        output_queued_bytes(tty_index: slopos_abi::syscall::TtyIndex) -> Result<usize, slopos_abi::tty_error::TtyError>;
        set_exclusive(tty_index: slopos_abi::syscall::TtyIndex, enable: bool) -> Result<(), slopos_abi::tty_error::TtyError>;
        get_exclusive(tty_index: slopos_abi::syscall::TtyIndex) -> Result<bool, slopos_abi::tty_error::TtyError>;
    }
}

#[inline(always)]
pub fn set_termios(
    tty_index: slopos_abi::syscall::TtyIndex,
    t: &slopos_abi::syscall::UserTermios,
) -> Result<(), slopos_abi::tty_error::TtyError> {
    (tty_services().set_termios)(tty_index, t)
}

#[inline(always)]
pub fn set_termios_wait(
    tty_index: slopos_abi::syscall::TtyIndex,
    t: &slopos_abi::syscall::UserTermios,
) -> Result<(), slopos_abi::tty_error::TtyError> {
    (tty_services().set_termios_wait)(tty_index, t)
}

#[inline(always)]
pub fn set_termios_flush(
    tty_index: slopos_abi::syscall::TtyIndex,
    t: &slopos_abi::syscall::UserTermios,
) -> Result<(), slopos_abi::tty_error::TtyError> {
    (tty_services().set_termios_flush)(tty_index, t)
}

#[inline(always)]
pub fn set_winsize(
    tty_index: slopos_abi::syscall::TtyIndex,
    ws: &slopos_abi::syscall::UserWinsize,
) -> Result<(), slopos_abi::tty_error::TtyError> {
    (tty_services().set_winsize)(tty_index, ws)
}
