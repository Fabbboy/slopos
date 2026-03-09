crate::define_service! {
    tty => TtyServices {
        read_cooked(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, max: usize, nonblock: bool) -> isize;
        read_cooked_with_attach(tty_index: slopos_abi::syscall::TtyIndex, buf: *mut u8, max: usize, nonblock: bool, auto_attach: bool) -> isize;
        has_cooked_data(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        set_termios(tty_index: slopos_abi::syscall::TtyIndex, t: *const slopos_abi::syscall::UserTermios);
        set_termios_wait(tty_index: slopos_abi::syscall::TtyIndex, t: *const slopos_abi::syscall::UserTermios) -> i32;
        set_termios_flush(tty_index: slopos_abi::syscall::TtyIndex, t: *const slopos_abi::syscall::UserTermios) -> i32;
        get_termios(tty_index: slopos_abi::syscall::TtyIndex, t: *mut slopos_abi::syscall::UserTermios);
        set_ldisc(tty_index: slopos_abi::syscall::TtyIndex, ldisc_id: u32) -> i32;
        get_ldisc(tty_index: slopos_abi::syscall::TtyIndex) -> u32;
        get_winsize(tty_index: slopos_abi::syscall::TtyIndex, ws: *mut slopos_abi::syscall::UserWinsize);
        set_winsize(tty_index: slopos_abi::syscall::TtyIndex, ws: *const slopos_abi::syscall::UserWinsize);
        set_compositor_focus(target: u32) -> i32;
        get_compositor_focus() -> u32;
        set_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32) -> i32;
        get_foreground_pgrp(tty_index: slopos_abi::syscall::TtyIndex) -> u32;
        get_session_id(tty_index: slopos_abi::syscall::TtyIndex) -> u32;
        set_foreground_pgrp_checked(tty_index: slopos_abi::syscall::TtyIndex, pgid: u32, caller_sid: u32) -> i32;
        write_bytes(tty_index: slopos_abi::syscall::TtyIndex, buf: *const u8, len: usize) -> usize;
        attach_session(tty_index: slopos_abi::syscall::TtyIndex, leader_pid: u32, leader_pgid: u32);
        acquire_controlling_terminal(tty_index: slopos_abi::syscall::TtyIndex, session_leader: u32, session_pgid: u32) -> i32;
        release_controlling_terminal(tty_index: slopos_abi::syscall::TtyIndex, session_id: u32) -> i32;
        open_ref(tty_index: slopos_abi::syscall::TtyIndex) -> i32;
        close_ref(tty_index: slopos_abi::syscall::TtyIndex) -> i32;
        hangup(tty_index: slopos_abi::syscall::TtyIndex);
        is_hung_up(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        alloc_pty() -> i32;
        get_pty_number(tty_index: slopos_abi::syscall::TtyIndex) -> i32;
        is_pty_slave(tty_index: slopos_abi::syscall::TtyIndex) -> bool;
        open_pty_slave(tty_index: slopos_abi::syscall::TtyIndex) -> i32;
        detach_session_by_id(session_id: u32);
        poll_events(tty_index: slopos_abi::syscall::TtyIndex, requested: u16) -> u16;
        poll_sleep();
    }
}
