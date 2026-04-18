//! Shared fixtures and imports for TTY tests.

//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (compositor focus, foreground pgrp, active TTY).
//!
//! Coverage includes input flag processing, output processing, signal
//! generation, flow control, VLNEXT, VWERASE, ECHOCTL, compositor focus /
//! fg_pgrp split, check_read() as sole read gate, TtyIndex type safety,
//! and signal constant verification.

pub(super) use slopos_abi::KernelErrno;
pub(super) use slopos_abi::signal::{
    SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH,
};
pub(super) use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, OutputFlags, POSIX_VDISABLE,
};
pub(super) use slopos_testing::TestResult;
pub(super) use slopos_utils::klog_info;

pub(super) use crate::tty;
pub(super) use crate::tty::TtyError;
pub(super) use crate::tty::TtyIndex;
pub(super) use crate::tty::driver::{
    DriverId, InputEvent, InputStatus, SerialConsoleDriver, TtyDriverKind, VConsoleDriver,
};
pub(super) use crate::tty::ldisc::{InputAction, LdiscKind, LineDisc, OutputAction, RawDisc};
pub(super) use crate::tty::session::TtySession;
pub(super) use crate::tty::session::{
    ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION, ProcessGroupId, SessionId,
};
pub(super) use crate::tty::table::{TTY_GENERATIONS, TTY_OUTPUT_INFLIGHT, TTY_SLOTS};
pub(super) use crate::tty::vconsole::{
    Cell, CellAttributes, CellGrid, CursorAttributes, VCONSOLE_MAX_COLS, VCONSOLE_MAX_ROWS,
    VConsoleState,
};
pub(super) use crate::tty::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};
pub(super) use crate::tty::{PacketEvents, TtyFlags};

pub(super) use crate::tty::pty::PtyPeerHandle;

pub(super) fn boxed_vconsole_state() -> slopos_alloc::KBox<VConsoleState> {
    let mut state = slopos_alloc::KBox::try_new(VConsoleState::new()).expect("test alloc");
    state.rows = 25;
    state.cols = 80;
    state.cursor_attrs = CursorAttributes {
        fg: 0x00AAAAAA,
        bg: 0x00000000,
        bold: false,
        underline: false,
        inverse: false,
    };
    state.saved_cursor_attrs = state.cursor_attrs;
    state.cells.allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
        .alt_cells
        .allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
}

pub(super) fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        match tty::read(idx, &mut scratch, true) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

pub(super) fn packet_mode_setup_pty()
-> Option<(TtyIndex, TtyIndex, slopos_abi::syscall::UserTermios)> {
    tty::table::tty_table_init();
    let master = tty::pty_alloc().ok()?;
    let _ = tty::open_ref(master);
    let slave_num = tty::get_pty_number(master).ok()?;
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).ok()?;
    tty::pty_open_slave(slave).ok()?;
    let saved = tty::get_termios(slave).ok()?;
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    raw.c_iflag = InputFlags::empty(); // clear all input flags including IXON
    tty::set_termios(slave, &raw).ok()?;
    Some((master, slave, saved))
}

/// Helper: tear down a PTY pair.
pub(super) fn packet_mode_teardown_pty(
    master: TtyIndex,
    slave: TtyIndex,
    saved: &slopos_abi::syscall::UserTermios,
) {
    let _ = tty::set_termios(slave, saved);
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);
}
