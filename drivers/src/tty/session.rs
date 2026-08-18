//! TTY session and process-group management.
//!
//! Each TTY has at most one controlling session, and within it one foreground
//! process group. Both are held as [`KWeak`] handles, so a terminal can never
//! pin a dead session and a reused pid can never be mistaken for the old
//! group; a dead handle reads as id `0` — "no session / no foreground group".
//!
//! `focused_task_id` is compositor window focus and is independent of the
//! foreground group.

use slopos_ostd::task::{ProcessGroup, Session};
use slopos_ostd::{KArc, KWeak};

use super::MAX_TTYS;
use super::table::TTY_SLOTS;

/// Per-TTY session and foreground process-group state.
pub struct TtySession {
    session: KWeak<Session>,
    fg_pgrp: KWeak<ProcessGroup>,
    /// Compositor input focus; `0` = none. Not a POSIX session/pgrp id.
    pub(crate) focused_task_id: u32,
}

/// Result of a foreground access check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundCheck {
    Allowed,
    /// No session attached or no foreground pgrp set — permissive pre-session
    /// path.
    BootstrapAllowed,
    /// Caller is in a different session — hard denial (`-EIO`).
    DeniedCrossSession,
    /// Background read — caller should receive `SIGTTIN`.
    BackgroundRead,
    /// Background write under `TOSTOP` — caller should receive `SIGTTOU`.
    BackgroundWrite,
}

impl TtySession {
    pub const fn new() -> Self {
        Self {
            session: KWeak::new(),
            fg_pgrp: KWeak::new(),
            focused_task_id: 0,
        }
    }

    pub fn has_session(&self) -> bool {
        self.session.upgrade().is_some()
    }

    /// Handles are resolved by the caller before the per-TTY lock is taken.
    pub fn attach(&mut self, session: KWeak<Session>, fg: KWeak<ProcessGroup>) {
        self.session = session;
        self.fg_pgrp = fg;
    }

    pub fn detach(&mut self) {
        self.session = KWeak::new();
        self.fg_pgrp = KWeak::new();
        // focused_task_id is deliberately not cleared — compositor focus is independent.
    }

    /// The controlling session's id, or `0` when none is attached / alive.
    pub fn session_id(&self) -> u32 {
        self.session.upgrade().map_or(0, |s| s.id())
    }

    /// The foreground process group's id, or `0` when none is set / alive.
    pub fn fg_pgrp_id(&self) -> u32 {
        self.fg_pgrp.upgrade().map_or(0, |pg| pg.id())
    }

    /// Pin the foreground group across a signal delivery.
    pub fn fg_pgrp_handle(&self) -> Option<KArc<ProcessGroup>> {
        self.fg_pgrp.upgrade()
    }

    /// An empty weak clears it. No session validation — `set_fg_pgrp_checked`
    /// is the checked variant.
    pub fn set_fg_pgrp(&mut self, fg: KWeak<ProcessGroup>) {
        self.fg_pgrp = fg;
    }

    /// Check whether a process with the given pgid and sid may **read** from
    /// this TTY.
    pub fn check_read(&self, caller_pgid: u32, caller_sid: u32) -> ForegroundCheck {
        let sid = self.session_id();
        if sid == 0 {
            return ForegroundCheck::BootstrapAllowed;
        }

        let fg = self.fg_pgrp_id();
        if fg == 0 {
            return ForegroundCheck::BootstrapAllowed;
        }

        // A caller sid/pgid of 0 is a kernel or unknown task — exempt from
        // session scoping, here and in every other check below.
        if caller_sid != 0 && caller_sid != sid {
            return ForegroundCheck::DeniedCrossSession;
        }

        if caller_pgid == fg {
            return ForegroundCheck::Allowed;
        }

        if caller_pgid == 0 {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundRead
    }

    /// Check whether a process with the given pgid and sid may **write** to
    /// this TTY. Foreground enforcement applies only when `TOSTOP` is set in
    /// `c_lflag`; a cross-session write is denied either way.
    pub fn check_write(&self, caller_pgid: u32, caller_sid: u32, tostop: bool) -> ForegroundCheck {
        let sid = self.session_id();
        let fg = self.fg_pgrp_id();
        if sid == 0 || fg == 0 {
            return ForegroundCheck::Allowed;
        }

        if caller_sid != 0 && caller_sid != sid {
            return ForegroundCheck::DeniedCrossSession;
        }

        if !tostop {
            return ForegroundCheck::Allowed;
        }

        if caller_pgid == 0 || caller_pgid == fg {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundWrite
    }

    /// Set the foreground process group, with session validation: the caller
    /// and the target group must both belong to the TTY's controlling session.
    /// An empty `fg` clears it. Returns `true` if the operation was allowed.
    pub fn set_fg_pgrp_checked(&mut self, fg: KWeak<ProcessGroup>, caller_sid: u32) -> bool {
        let sid = self.session_id();
        if sid == 0 {
            self.fg_pgrp = fg;
            return true;
        }

        if caller_sid != 0 && caller_sid != sid {
            return false;
        }

        if let Some(pg) = fg.upgrade() {
            if pg.session_id() != sid {
                return false;
            }
        }

        self.fg_pgrp = fg;
        true
    }
}

/// Test-only: install session/foreground handles directly, so a test can drive
/// job control without a live task carrying the matching pgid/sid.
#[cfg(feature = "test-hooks")]
pub fn test_install_session(
    idx: super::TtyIndex,
    session: KWeak<Session>,
    fg: KWeak<ProcessGroup>,
) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.attach(session, fg);
    }
}

/// Detach every TTY whose controlling session matches `session_id` (called
/// when a session ends).
pub fn detach_session_by_id(session_id: u32) {
    if session_id == 0 {
        return;
    }
    for i in 0..MAX_TTYS {
        let mut guard = TTY_SLOTS[i].lock();
        if let Some(tty) = guard.as_mut() {
            if tty.session.session_id() == session_id {
                tty.session.detach();
            }
        }
    }
}
