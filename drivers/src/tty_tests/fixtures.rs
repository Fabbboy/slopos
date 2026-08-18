//! Shared fixtures and imports for TTY tests.

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
pub(super) use slopos_ostd::process::quota::FileBacking;
pub(super) use slopos_ostd::task::{ProcessGroup, Session};

pub(super) use slopos_ostd::{KArc, KWeak};

/// Echo is staged in the discipline's queue rather than returned from
/// `receive_buf`, so a test drains it the way the emitter does.
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

/// Returns a TTY to service when it goes out of scope. A hangup is terminal for
/// the slot, and TTY 0 carries the serial console every later test writes
/// through; a guard restores it on the failure paths too. Termios goes back
/// alongside the flag because a `B0` baud rate would hang the line up again on
/// the next `tcsetattr`.
pub(super) struct HangupScope {
    idx: TtyIndex,
    saved: Option<slopos_abi::syscall::UserTermios>,
}

impl HangupScope {
    /// For a hangup the caller performs itself. Construct it *before* the
    /// hangup, so the termios it snapshots is the healthy one.
    pub(super) fn guard(idx: TtyIndex) -> Self {
        Self {
            idx,
            saved: tty::get_termios(idx).ok(),
        }
    }

    pub(super) fn hang_up(idx: TtyIndex) -> Self {
        let scope = Self::guard(idx);
        tty::hangup(idx);
        scope
    }
}

impl Drop for HangupScope {
    fn drop(&mut self) {
        tty::clear_hangup(self.idx);
        if let Some(saved) = self.saved {
            let _ = tty::set_termios(self.idx, &saved);
        }
    }
}

/// A `TtySession` stores only `KWeak`s, so keep this in scope across the
/// assertions to hold its session and foreground group resolvable.
pub(super) struct SessionScope {
    pub(super) session: KArc<Session>,
    pub(super) pgrp: KArc<ProcessGroup>,
}

impl SessionScope {
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

    pub(super) fn attach_to(&self, s: &mut TtySession) {
        s.attach(self.session_weak(), self.pgrp_weak());
    }

    /// Another foreground-group candidate in this session; the caller must keep
    /// the returned handle alive.
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

/// The flush is not redundant with the reads: a canonical discipline hands back
/// only complete lines, so an unterminated tail survives every read and
/// reappears as the next test's phantom input once something clears `ICANON`.
pub(super) fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        match tty::read(idx, &mut scratch, true) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = tty::tcflush(idx, slopos_abi::syscall::TCIFLUSH);
}

/// Two things sit outside what `tcdrain` promises, so the pair is retried: a
/// `TCOFLUSH` racing an emission zeroes an in-flight count that emission still
/// owns, and the water-mark crossing producing the byte is a one-shot any CPU
/// may consume. The bound keeps a lost byte a failure, not a hang.
pub(super) fn drain_then_read_byte(stage: TtyIndex, peer: TtyIndex, byte: u8) -> bool {
    const ROUNDS: usize = 64;
    for _ in 0..ROUNDS {
        let _ = tty::bytes_available(stage);
        if tty::tcsbrk(stage, 1).is_err() {
            return false;
        }
        let mut back = [0u8; 8];
        if matches!(tty::read(peer, &mut back, true), Ok(1) if back[0] == byte) {
            return true;
        }
        crate::hpet::delay_ns(50_000);
    }
    false
}

/// The master backing is declared first so it drops first: its drop hangs up
/// the slave, and the last slave-open drop then frees the slave slot.
pub(super) struct PtyPair {
    pub(super) master: TtyIndex,
    pub(super) slave: TtyIndex,
    pub(super) master_backing: KArc<TtyBacking>,
    pub(super) slave_backing: KArc<dyn FileBacking>,
}

pub(super) fn open_pty_pair() -> PtyPair {
    tty::table::tty_table_init();
    let (master, master_backing) =
        tty::pty_alloc(slopos_ostd::process::quota::root()).expect("pty_alloc");
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

/// The peer link from a PTY end's driver, for direct `master_write` /
/// `slave_write` calls.
pub(super) fn peer_link_of(idx: TtyIndex) -> KWeak<TtyBacking> {
    let guard = TTY_SLOTS[idx.0 as usize].lock();
    match guard.as_ref().map(|tty| &tty.driver) {
        Some(TtyDriverKind::PtyMaster { peer }) | Some(TtyDriverKind::PtySlave { peer }) => {
            peer.clone()
        }
        _ => KWeak::new(),
    }
}

/// Holds both ends of a packet-mode PTY pair open; dropping it frees the pair.
pub(super) struct PtyGuard {
    _master_backing: KArc<TtyBacking>,
    _slave_backing: KArc<dyn FileBacking>,
}

/// The slave is switched to raw; the returned termios is the saved original.
pub(super) fn packet_mode_setup_pty() -> Option<(
    TtyIndex,
    TtyIndex,
    slopos_abi::syscall::UserTermios,
    PtyGuard,
)> {
    tty::table::tty_table_init();
    let (master, master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).ok()?;
    let slave_num = tty::get_pty_number(master).ok()?;
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).ok()?;
    let slave_backing = tty::pty_open_slave(slave).ok()?;
    let saved = tty::get_termios(slave).ok()?;
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    raw.c_iflag = InputFlags::empty();
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

/// Restores termios only; the [`PtyGuard`] closes the pair when it drops.
pub(super) fn packet_mode_teardown_pty(
    _master: TtyIndex,
    slave: TtyIndex,
    saved: &slopos_abi::syscall::UserTermios,
) {
    let _ = tty::set_termios(slave, saved);
}
