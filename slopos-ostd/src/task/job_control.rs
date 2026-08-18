//! Session and process-group identity objects.
//!
//! Every member task holds a strong [`KArc`] on its [`ProcessGroup`]; a
//! terminal holds its foreground group and controlling session only weakly
//! ([`KWeak`]), so once the last member is reaped the handle upgrades to
//! `None` and a reused pid can never be mistaken for the old group.
//!
//! The strong graph `Task -> ProcessGroup -> Session` is a DAG. Both `Drop`s
//! are trivial, so a handle may be released under any lock.

use core::num::NonZeroU32;

use crate::KArc;

/// A POSIX session, identified by its leader's pid at creation time.
pub struct Session {
    id: NonZeroU32,
}

impl Session {
    /// Returns `None` for the reserved id `0`.
    pub fn new(id: u32) -> Option<Self> {
        NonZeroU32::new(id).map(|id| Self { id })
    }

    pub fn id(&self) -> u32 {
        self.id.get()
    }
}

pub struct ProcessGroup {
    id: NonZeroU32,
    session: KArc<Session>,
}

impl ProcessGroup {
    /// Returns `None` for the reserved id `0`.
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

/// Session and initial group both take the leader's pid `id`. Returns `None`
/// for id `0` or on allocation failure.
pub fn new_session_group(id: u32) -> Option<KArc<ProcessGroup>> {
    let session = KArc::try_new(Session::new(id)?).ok()?;
    KArc::try_new(ProcessGroup::new(id, session)?).ok()
}

/// Returns `None` for pgid `0` or on allocation failure.
pub fn new_group_in_session(pgid: u32, session: KArc<Session>) -> Option<KArc<ProcessGroup>> {
    KArc::try_new(ProcessGroup::new(pgid, session)?).ok()
}
