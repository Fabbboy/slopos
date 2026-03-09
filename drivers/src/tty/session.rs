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
//! The compositor still drives `focused_task_id` for window-level focus.
//! `set_compositor_focus()` (called by the compositor) sets only
//! `focused_task_id` — it does NOT alter `fg_pgrp`.  The two concepts are
//! independent (Phase 6 split).
//!
//! # Phase 14: Sentinel Newtypes
//!
//! Raw `u32` sentinel constants (`NO_SESSION = 0`, `NO_FOREGROUND_PGRP = 0`)
//! have been replaced with `Option<SessionId>` and `Option<ProcessGroupId>`.
//! `None` represents the "no session" / "no foreground pgrp" state.  This
//! makes invalid states unrepresentable at the type level.

use core::num::NonZeroU32;

use super::MAX_TTYS;
use super::table::TTY_SLOTS;

// ---------------------------------------------------------------------------
// Sentinel newtypes (Phase 14)
// ---------------------------------------------------------------------------

/// A non-zero session identifier.
///
/// Wraps `NonZeroU32` so that `Option<SessionId>` is the same size as `u32`
/// (niche optimisation) and `None` replaces the old `NO_SESSION = 0` sentinel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionId(NonZeroU32);

impl SessionId {
    /// Create a `SessionId` from a raw `u32`.
    ///
    /// Returns `None` when `v == 0` (the "no session" sentinel).
    pub const fn new(v: u32) -> Option<Self> {
        match NonZeroU32::new(v) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Return the underlying `u32` value.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A non-zero process-group identifier.
///
/// Wraps `NonZeroU32` so that `Option<ProcessGroupId>` is the same size as
/// `u32` (niche optimisation) and `None` replaces the old
/// `NO_FOREGROUND_PGRP = 0` sentinel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcessGroupId(NonZeroU32);

impl ProcessGroupId {
    /// Create a `ProcessGroupId` from a raw `u32`.
    ///
    /// Returns `None` when `v == 0` (the "no foreground pgrp" sentinel).
    pub const fn new(v: u32) -> Option<Self> {
        match NonZeroU32::new(v) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Return the underlying `u32` value.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

// ---------------------------------------------------------------------------
// Legacy sentinel constants (backward compatibility)
// ---------------------------------------------------------------------------

/// Legacy sentinel — prefer `Option<SessionId>` (`None` = no session).
pub const NO_SESSION: u32 = 0;

/// Legacy sentinel — prefer `Option<ProcessGroupId>` (`None` = no fg pgrp).
pub const NO_FOREGROUND_PGRP: u32 = 0;

// ---------------------------------------------------------------------------
// TtySession
// ---------------------------------------------------------------------------

/// Per-TTY session and foreground process-group state.
///
/// In the POSIX model, each terminal has at most one controlling session,
/// and within that session exactly one process group is "foreground" (allowed
/// to read from / write to the terminal without signals).
///
/// Phase 14: Fields use `Option<SessionId>` / `Option<ProcessGroupId>` instead
/// of raw `u32` with magic-zero sentinels.
#[derive(Clone, Copy)]
pub struct TtySession {
    /// Session leader's PID (`None` = no session attached).
    pub session_leader: Option<SessionId>,
    /// Session ID (typically == session leader's PID).
    pub session_id: Option<SessionId>,
    /// Foreground process group ID (`None` = none).
    pub fg_pgrp: Option<ProcessGroupId>,
    /// The task ID that currently has input focus on this TTY.
    /// Set by the compositor via `set_focus()`.  0 = no specific task focused.
    ///
    /// **Not** wrapped in a newtype — this is a compositor concept, not a POSIX
    /// session/pgrp ID.
    pub focused_task_id: u32,
}

/// Result of a foreground access check.
///
/// Phase 19: The overloaded `NoSession` variant has been replaced with
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
            session_leader: None,
            session_id: None,
            fg_pgrp: None,
            focused_task_id: 0,
        }
    }

    /// Returns `true` if a session is currently attached to this TTY.
    pub fn has_session(&self) -> bool {
        self.session_id.is_some()
    }

    /// Attach a session to this TTY.
    ///
    /// `leader_pid` is the PID of the session leader (from `setsid()`).
    /// The session ID is set equal to the leader's PID, matching POSIX semantics.
    /// The foreground process group is initially set to the leader's process group.
    pub fn attach(&mut self, leader_pid: u32, leader_pgid: u32) {
        self.session_leader = SessionId::new(leader_pid);
        self.session_id = SessionId::new(leader_pid);
        self.fg_pgrp = ProcessGroupId::new(leader_pgid);
    }

    /// Detach the current session from this TTY.
    ///
    /// Called when the session leader calls `setsid()` on a different terminal,
    /// or when the session leader exits.
    pub fn detach(&mut self) {
        self.session_leader = None;
        self.session_id = None;
        self.fg_pgrp = None;
        // Note: focused_task_id is NOT cleared — compositor focus is independent.
    }

    // -- Raw u32 accessors (public API compatibility) -------------------------

    /// Return the session ID as a raw `u32` (0 if no session).
    pub fn session_id_raw(&self) -> u32 {
        self.session_id.map_or(0, |sid| sid.get())
    }

    /// Return the session leader PID as a raw `u32` (0 if no session).
    pub fn session_leader_raw(&self) -> u32 {
        self.session_leader.map_or(0, |sid| sid.get())
    }

    /// Return the foreground pgrp as a raw `u32` (0 if none).
    pub fn fg_pgrp_raw(&self) -> u32 {
        self.fg_pgrp.map_or(0, |pgid| pgid.get())
    }

    /// Set the foreground pgrp from a raw `u32` (0 clears it).
    pub fn set_fg_pgrp_raw(&mut self, pgid: u32) {
        self.fg_pgrp = ProcessGroupId::new(pgid);
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
        // No session attached — permissive (pre-session-setup path).
        if !self.has_session() {
            return ForegroundCheck::BootstrapAllowed;
        }

        // No foreground pgrp set — permissive.
        if self.fg_pgrp.is_none() {
            return ForegroundCheck::BootstrapAllowed;
        }

        let sid_raw = self.session_id_raw();
        let fg_raw = self.fg_pgrp_raw();

        // Phase 19: Cross-session access — hard denial.  A process from a
        // different session must not read this TTY.  Kernel tasks
        // (caller_sid == 0) are exempted for early-boot permissiveness.
        if caller_sid != 0 && caller_sid != sid_raw {
            return ForegroundCheck::DeniedCrossSession;
        }

        // Foreground check.
        if caller_pgid == fg_raw {
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
        // No session attached or no fg_pgrp — allow writes freely.
        if !self.has_session() || self.fg_pgrp.is_none() {
            return ForegroundCheck::Allowed;
        }

        let sid_raw = self.session_id_raw();

        // Phase 19: Cross-session write — hard denial (same rule as reads).
        // Kernel tasks (caller_sid == 0) are exempted.
        if caller_sid != 0 && caller_sid != sid_raw {
            return ForegroundCheck::DeniedCrossSession;
        }

        // Without TOSTOP, same-session writes are always allowed.
        if !tostop {
            return ForegroundCheck::Allowed;
        }

        let fg_raw = self.fg_pgrp_raw();

        if caller_pgid == 0 || caller_pgid == fg_raw {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundWrite
    }

    // NOTE: `task_has_access()` has been removed in Phase 6.
    // Use `check_read()` and `check_write()` directly instead.

    /// Set the foreground process group, with session validation.
    ///
    /// In POSIX, only processes in the same session as the TTY's controlling
    /// session may set the foreground pgrp.
    ///
    /// Returns `true` if the operation was allowed.
    pub fn set_fg_pgrp_checked(&mut self, pgid: u32, caller_sid: u32) -> bool {
        // If no session is attached, allow freely (pre-session path).
        if !self.has_session() {
            self.fg_pgrp = ProcessGroupId::new(pgid);
            return true;
        }

        let sid_raw = self.session_id_raw();

        // Caller must be in the same session.
        if caller_sid != 0 && caller_sid != sid_raw {
            return false;
        }

        self.fg_pgrp = ProcessGroupId::new(pgid);
        true
    }
}

pub fn detach_session_by_id(session_id: u32) {
    if session_id == 0 {
        return;
    }
    for i in 0..MAX_TTYS {
        let mut guard = TTY_SLOTS[i].lock();
        if let Some(tty) = guard.as_mut() {
            if tty.session.session_id_raw() == session_id {
                tty.session.detach();
            }
        }
    }
}
