//! Session and process-group identity objects.
//!
//! A [`ProcessGroup`] is kept alive by its member tasks: every member holds a
//! strong [`KArc`], so the group's lifetime is exactly "at least one member
//! still occupies a task slot". A terminal references its foreground group and
//! controlling session only weakly ([`KWeak`]), so once the last member is
//! reaped the group drops and the terminal's handle upgrades to `None` — a
//! reused pid can never be mistaken for the old group.
//!
//! The strong graph `Task -> ProcessGroup -> Session` is a DAG: a group pins
//! its session, so a session outlives every group that belongs to it. Both
//! `Drop`s are trivial (no locks, no callbacks), so a handle may be released
//! under any lock.

use core::num::NonZeroU32;

use crate::KArc;

/// A POSIX session, identified by its leader's pid at creation time.
pub struct Session {
    id: NonZeroU32,
}

impl Session {
    /// Create a session with id `id`. Returns `None` for the reserved id `0`.
    pub fn new(id: u32) -> Option<Self> {
        NonZeroU32::new(id).map(|id| Self { id })
    }

    pub fn id(&self) -> u32 {
        self.id.get()
    }
}

/// A POSIX process group. Holds its [`Session`] strongly.
pub struct ProcessGroup {
    id: NonZeroU32,
    session: KArc<Session>,
}

impl ProcessGroup {
    /// Create a process group with id `id` belonging to `session`. Returns
    /// `None` for the reserved id `0`.
    pub fn new(id: u32, session: KArc<Session>) -> Option<Self> {
        NonZeroU32::new(id).map(|id| Self { id, session })
    }

    pub fn id(&self) -> u32 {
        self.id.get()
    }

    pub fn session(&self) -> &KArc<Session> {
        &self.session
    }

    pub fn session_id(&self) -> u32 {
        self.session.id()
    }
}

/// Mint a fresh session and its initial process group for a session leader
/// whose pid is `id`; both share `id`. Returns `None` for id `0` or on
/// allocation failure.
pub fn new_session_group(id: u32) -> Option<KArc<ProcessGroup>> {
    let session = KArc::try_new(Session::new(id)?).ok()?;
    KArc::try_new(ProcessGroup::new(id, session)?).ok()
}

/// Mint a fresh process group `pgid` within an existing `session`. Returns
/// `None` for pgid `0` or on allocation failure.
pub fn new_group_in_session(pgid: u32, session: KArc<Session>) -> Option<KArc<ProcessGroup>> {
    KArc::try_new(ProcessGroup::new(pgid, session)?).ok()
}
