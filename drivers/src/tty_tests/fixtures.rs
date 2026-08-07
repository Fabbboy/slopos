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

pub(super) use slopos_ostd::KBox;

pub(super) use slopos_abi::KernelErrno;
pub(super) use slopos_abi::signal::{
    SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH,
};
pub(super) use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, OutputFlags, POSIX_VDISABLE,
};
pub(super) use slopos_ostd::klog_info;
pub(super) use slopos_testing::TestResult;

pub(super) use crate::tty;
pub(super) use crate::tty::TtyError;
pub(super) use crate::tty::TtyIndex;
pub(super) use crate::tty::backing::TtyBacking;
pub(super) use crate::tty::driver::{
    DriverId, InputEvent, InputStatus, SerialConsoleDriver, TtyDriverKind, VConsoleDriver,
};
pub(super) use crate::tty::ldisc::{InputAction, LdiscKind, LineDisc, OutputAction, RawDisc};
pub(super) use crate::tty::output::WriteNesting;
pub(super) use crate::tty::session::ForegroundCheck;
pub(super) use crate::tty::session::TtySession;
pub(super) use crate::tty::table::{TTY_OUTPUT_INFLIGHT, TTY_SLOTS};
pub(super) use crate::tty::vconsole::{
    Cell, CellAttributes, CellGrid, CursorAttributes, VCONSOLE_MAX_COLS, VCONSOLE_MAX_ROWS,
    VConsoleState,
};
pub(super) use crate::tty::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};
pub(super) use crate::tty::{PacketEvents, TtyFlags};
pub(super) use slopos_abi::file_ops::FileBacking;
pub(super) use slopos_ostd::task::{ProcessGroup, Session};

pub(super) use slopos_ostd::{KArc, KWeak};

/// Scratch for reading a discipline's staged echo back out.
///
/// Echo lives in the discipline's queue rather than in the `receive_buf`
/// return value, because the TTY core has to emit it after the slot guard
/// drops.  A test asserts on it by draining it the same way the emitter does.
pub(super) struct EchoScratch {
    buf: [u8; 512],
    len: usize,
}

impl EchoScratch {
    pub(super) fn drain(ld: &mut LineDisc) -> Self {
        let mut buf = [0u8; 512];
        let len = ld.echo_take(&mut buf);
        Self { buf, len }
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Holds strong session + foreground-group refs alive so that a `TtySession`'s
/// weak links resolve for the duration of a test. A `TtySession` only stores
/// `KWeak`s; keep the owning `SessionScope` in scope across the assertions.
pub(super) struct SessionScope {
    pub(super) session: KArc<Session>,
    pub(super) pgrp: KArc<ProcessGroup>,
}

impl SessionScope {
    /// A live session `sid` whose foreground group is `pgid`.
    pub(super) fn new(sid: u32, pgid: u32) -> Self {
        let session =
            KArc::try_new(Session::new(sid).expect("nonzero sid")).expect("alloc session");
        let pgrp = KArc::try_new(ProcessGroup::new(pgid, session.clone()).expect("nonzero pgid"))
            .expect("alloc pgrp");
        Self { session, pgrp }
    }

    pub(super) fn session_weak(&self) -> KWeak<Session> {
        KArc::downgrade(&self.session)
    }

    pub(super) fn pgrp_weak(&self) -> KWeak<ProcessGroup> {
        KArc::downgrade(&self.pgrp)
    }

    /// Attach this scope's session + foreground group to a bare `TtySession`.
    pub(super) fn attach_to(&self, s: &mut TtySession) {
        s.attach(self.session_weak(), self.pgrp_weak());
    }

    /// Mint an additional foreground-group candidate `pgid` inside this scope's
    /// session (for `set_fg_pgrp*`). The caller keeps the handle alive.
    pub(super) fn extra_group(&self, pgid: u32) -> KArc<ProcessGroup> {
        KArc::try_new(ProcessGroup::new(pgid, self.session.clone()).expect("nonzero pgid"))
            .expect("alloc pgrp")
    }
}

pub(super) fn boxed_vconsole_state() -> slopos_ostd::KBox<VConsoleState> {
    let mut state = slopos_ostd::KBox::try_init(VConsoleState::init_default()).expect("test alloc");
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

/// A live PTY master/slave pair with both ends open.
///
/// `master_backing` is the master's owning `TtyBacking`; `slave_backing` is
/// the shared slave-open (`TtySlaveOpen`), erased to `dyn FileBacking`.
/// Dropping a `PtyPair` closes both ends: the master drop hangs up the
/// slave and frees the master slot, and the last slave-open drop frees the
/// slave slot. The master backing is declared first so it drops first.
pub(super) struct PtyPair {
    pub(super) master: TtyIndex,
    pub(super) slave: TtyIndex,
    pub(super) master_backing: KArc<TtyBacking>,
    pub(super) slave_backing: KArc<dyn FileBacking>,
}

/// Allocate a PTY pair, unlock the slave, and open it. Both ends are held
/// open by the returned backings; dropping the [`PtyPair`] closes both.
pub(super) fn open_pty_pair() -> PtyPair {
    tty::table::tty_table_init();
    let (master, master_backing) = tty::pty_alloc().expect("pty_alloc");
    let slave = TtyIndex(tty::get_pty_number(master).expect("get_pty_number") as u8);
    tty::set_pty_lock(master, false).expect("unlock slave");
    let slave_backing = tty::pty_open_slave(slave).expect("open slave");
    PtyPair {
        master,
        slave,
        master_backing,
        slave_backing,
    }
}

/// The weak peer link carried in a PTY end's driver, cloned out for direct
/// `master_write` / `slave_write` calls. A `KWeak` upgrades to `None` once
/// its referent backing is gone.
pub(super) fn peer_link_of(idx: TtyIndex) -> KWeak<TtyBacking> {
    let guard = TTY_SLOTS[idx.0 as usize].lock();
    match guard.as_ref().map(|tty| &tty.driver) {
        Some(TtyDriverKind::PtyMaster { peer }) | Some(TtyDriverKind::PtySlave { peer }) => {
            peer.clone()
        }
        _ => KWeak::new(),
    }
}

/// Holds both ends of a packet-mode PTY pair open for the lifetime of a
/// test. Dropping it closes both ends and frees the pair.
pub(super) struct PtyGuard {
    _master_backing: KArc<TtyBacking>,
    _slave_backing: KArc<dyn FileBacking>,
}

/// Set up a raw-mode PTY pair for packet-mode tests: allocate, unlock and
/// open the slave, switch it to raw with no input processing, and return
/// the saved termios plus a guard that keeps both ends open.
pub(super) fn packet_mode_setup_pty() -> Option<(
    TtyIndex,
    TtyIndex,
    slopos_abi::syscall::UserTermios,
    PtyGuard,
)> {
    tty::table::tty_table_init();
    let (master, master_backing) = tty::pty_alloc().ok()?;
    let slave_num = tty::get_pty_number(master).ok()?;
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).ok()?;
    let slave_backing = tty::pty_open_slave(slave).ok()?;
    let saved = tty::get_termios(slave).ok()?;
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    raw.c_iflag = InputFlags::empty(); // clear all input flags including IXON
    tty::set_termios(slave, &raw).ok()?;
    Some((
        master,
        slave,
        saved,
        PtyGuard {
            _master_backing: master_backing,
            _slave_backing: slave_backing,
        },
    ))
}

/// Restore a packet-mode PTY slave's termios. The pair itself is closed
/// when the [`PtyGuard`] returned by [`packet_mode_setup_pty`] drops.
pub(super) fn packet_mode_teardown_pty(
    _master: TtyIndex,
    slave: TtyIndex,
    saved: &slopos_abi::syscall::UserTermios,
) {
    let _ = tty::set_termios(slave, saved);
}
