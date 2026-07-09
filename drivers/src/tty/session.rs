//! TTY session and process-group management.
//!
//! Implements POSIX-like session semantics for per-TTY foreground control.
//!
//! # Model
//!
//! Each TTY may have at most one **controlling session**.  Within that session,
//! exactly one process group is the **foreground group** — only members of this
//! group are allowed to read from (and, if `TOSTOP` is set, write to) the
//! terminal without receiving `SIGTTIN` / `SIGTTOU`.
//!
//! The session and foreground group are held as **weak** references
//! ([`KWeak`]) to the kernel's [`Session`] / [`ProcessGroup`] objects. A group
//! is kept alive by its member tasks, so once the last member is reaped the
//! weak upgrades to `None` — a terminal can never pin a dead session, and a
//! reused pid can never be mistaken for the old group. A dead handle reads as
//! id `0`, which the foreground checks treat as "no session / no foreground
//! group".
//!
//! The compositor still drives `focused_task_id` for window-level focus.
//! `set_compositor_focus()` (called by the compositor) sets only
//! `focused_task_id` — it does NOT alter the foreground group.  The two
//! concepts are independent.

use slopos_ostd::task::{ProcessGroup, Session};
use slopos_ostd::{KArc, KWeak};

use super::MAX_TTYS;
use super::table::TTY_SLOTS;

// ---------------------------------------------------------------------------
// TtySession
// ---------------------------------------------------------------------------

/// Per-TTY session and foreground process-group state.
///
/// In the POSIX model, each terminal has at most one controlling session,
/// and within that session exactly one process group is "foreground" (allowed
/// to read from / write to the terminal without signals).
pub struct TtySession {
    /// Controlling session — empty when no session is attached, or when the
    /// attached session has ended.
    session: KWeak<Session>,
    /// Foreground process group — empty when none, or when the group has ended.
    fg_pgrp: KWeak<ProcessGroup>,
    /// The task ID that currently has input focus on this TTY.
    /// Set by the compositor via `set_focus()`.  0 = no specific task focused.
    ///
    /// This is a compositor concept, not a POSIX session/pgrp ID.
    pub(crate) focused_task_id: u32,
}

/// Result of a foreground access check.
///
/// The overloaded `NoSession` variant has been replaced with
/// explicit states that separate bootstrap permissiveness from real access
/// denial.  This makes the control plane easy to reason about — one enum,
/// one mapping layer at the syscall boundary, no scattered ad-hoc booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundCheck {
    /// Caller is in the foreground group — access allowed.
    Allowed,
    /// No session is attached yet, or no foreground pgrp is set.
    /// Access is allowed (permissive early-boot / pre-session path).
    BootstrapAllowed,
    /// Caller belongs to a different session than the TTY's controlling
    /// session — hard denial (`-EIO`).  This is the POSIX requirement that
    /// terminals are session-scoped resources.
    DeniedCrossSession,
    /// Caller is a background process trying to read — should receive `SIGTTIN`.
    BackgroundRead,
    /// Caller is a background process trying to write with `TOSTOP` — should
    /// receive `SIGTTOU`.
    BackgroundWrite,
}

impl TtySession {
    /// Create a new empty session (no controlling process).
    pub const fn new() -> Self {
        Self {
            session: KWeak::new(),
            fg_pgrp: KWeak::new(),
            focused_task_id: 0,
        }
    }

    /// Returns `true` if a live session is currently attached to this TTY.
    pub fn has_session(&self) -> bool {
        self.session.upgrade().is_some()
    }

    /// Attach a session (and its initial foreground group) to this TTY.
    ///
    /// The handles are resolved by the caller before the per-TTY lock is taken.
    pub fn attach(&mut self, session: KWeak<Session>, fg: KWeak<ProcessGroup>) {
        self.session = session;
        self.fg_pgrp = fg;
    }

    /// Detach the current session from this TTY.
    ///
    /// Called when the session leader calls `setsid()` on a different terminal,
    /// or when the session leader exits.
    pub fn detach(&mut self) {
        self.session = KWeak::new();
        self.fg_pgrp = KWeak::new();
        // Note: focused_task_id is NOT cleared — compositor focus is independent.
    }

    /// The controlling session's id, or `0` when none is attached / alive.
    pub fn session_id(&self) -> u32 {
        self.session.upgrade().map_or(0, |s| s.id())
    }

    /// The foreground process group's id, or `0` when none is set / alive.
    pub fn fg_pgrp_id(&self) -> u32 {
        self.fg_pgrp.upgrade().map_or(0, |pg| pg.id())
    }

    /// Pin the foreground group for the duration of a signal delivery, or
    /// `None` when there is no live foreground group.
    pub fn fg_pgrp_handle(&self) -> Option<KArc<ProcessGroup>> {
        self.fg_pgrp.upgrade()
    }

    /// Set the foreground process group from a pre-resolved handle (an empty
    /// weak clears it). No session validation — the checked variant enforces
    /// POSIX rules.
    pub fn set_fg_pgrp(&mut self, fg: KWeak<ProcessGroup>) {
        self.fg_pgrp = fg;
    }

    // -- Foreground checks ----------------------------------------------------

    /// Check whether a process with the given pgid and sid may **read** from
    /// this TTY.
    ///
    /// # Returns
    ///
    /// - `Allowed` if the caller is in the foreground group.
    /// - `BootstrapAllowed` if no session is attached yet (permissive).
    /// - `DeniedCrossSession` if the caller belongs to a different session.
    /// - `BackgroundRead` if the caller is in a background group.
    pub fn check_read(&self, caller_pgid: u32, caller_sid: u32) -> ForegroundCheck {
        let sid = self.session_id();
        // No live session attached — permissive (pre-session-setup path).
        if sid == 0 {
            return ForegroundCheck::BootstrapAllowed;
        }

        let fg = self.fg_pgrp_id();
        // No live foreground pgrp set — permissive.
        if fg == 0 {
            return ForegroundCheck::BootstrapAllowed;
        }

        // Cross-session access — hard denial.  A process from a
        // different session must not read this TTY.  Kernel tasks
        // (caller_sid == 0) are exempted for early-boot permissiveness.
        if caller_sid != 0 && caller_sid != sid {
            return ForegroundCheck::DeniedCrossSession;
        }

        // Foreground check.
        if caller_pgid == fg {
            return ForegroundCheck::Allowed;
        }

        // Caller's pgid doesn't match, but maybe caller_pgid is 0 (kernel
        // task or unknown) — be permissive.
        if caller_pgid == 0 {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundRead
    }

    /// Check whether a process with the given pgid and sid may **write** to
    /// this TTY.
    ///
    /// Write-side foreground enforcement only applies when `TOSTOP` is set in
    /// the TTY's termios.  Without `TOSTOP`, any process may write (unless the
    /// caller belongs to a different session, which is a hard denial).
    ///
    /// # Arguments
    ///
    /// * `caller_pgid` — The caller's process group ID.
    /// * `caller_sid` — The caller's session ID.
    /// * `tostop` — Whether the `TOSTOP` flag is set in `c_lflag`.
    pub fn check_write(&self, caller_pgid: u32, caller_sid: u32, tostop: bool) -> ForegroundCheck {
        let sid = self.session_id();
        let fg = self.fg_pgrp_id();
        // No live session or no fg_pgrp — allow writes freely.
        if sid == 0 || fg == 0 {
            return ForegroundCheck::Allowed;
        }

        // Cross-session write — hard denial (same rule as reads).
        // Kernel tasks (caller_sid == 0) are exempted.
        if caller_sid != 0 && caller_sid != sid {
            return ForegroundCheck::DeniedCrossSession;
        }

        // Without TOSTOP, same-session writes are always allowed.
        if !tostop {
            return ForegroundCheck::Allowed;
        }

        if caller_pgid == 0 || caller_pgid == fg {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundWrite
    }

    /// Set the foreground process group, with session validation.
    ///
    /// In POSIX, only processes in the same session as the TTY's controlling
    /// session may set the foreground pgrp, and the target group must itself
    /// belong to that session. `fg` is a pre-resolved handle (empty = clear).
    ///
    /// Returns `true` if the operation was allowed.
    pub fn set_fg_pgrp_checked(&mut self, fg: KWeak<ProcessGroup>, caller_sid: u32) -> bool {
        let sid = self.session_id();
        // If no live session is attached, allow freely (pre-session path).
        if sid == 0 {
            self.fg_pgrp = fg;
            return true;
        }

        // Caller must be in the terminal's session.
        if caller_sid != 0 && caller_sid != sid {
            return false;
        }

        // A named target group must belong to the terminal's session.
        if let Some(pg) = fg.upgrade() {
            if pg.session_id() != sid {
                return false;
            }
        }

        self.fg_pgrp = fg;
        true
    }
}

/// Install pre-resolved session/foreground handles onto a TTY slot, bypassing
/// task-table resolution. Test-only: lets a test drive job control without a
/// live task carrying the matching pgid/sid.
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
