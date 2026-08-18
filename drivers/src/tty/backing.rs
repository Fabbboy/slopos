//! TTY backing objects — single-owner lifetime for every TTY/PTY.
//!
//! A [`TtyBacking`] is the owning handle for one TTY slot: every open file
//! referencing the TTY holds a `KArc<TtyBacking>` clone, so the strong count
//! *is* the open count and the backing's `Drop` *is* the last-close teardown.
//!
//! # Pair topology
//!
//! The master backing holds its slave **strongly**; the slave holds its master
//! **weakly** (`upgrade() == None` ⇔ the master is gone). Both links are built
//! with [`KArc::try_new_cyclic`] so they are valid from birth. Slave **opens**
//! are a separate tier: all slave fds share one [`TtySlaveOpen`] whose `Drop`
//! latches `PEER_CLOSED` on the master, so last-slave-close (master sees EOF,
//! slave stays reopenable) stays distinguishable from pair teardown.
//!
//! # Lock ordering
//!
//! `TTY_BACKINGS[i]` / `TTY_SLAVE_OPENS[i]` → `TTY_SLOTS[j]`. Drop bodies
//! take only per-slot locks, never `PTY_ALLOC_LOCK`. Because teardown
//! retakes a slot's registry locks, a strong backing reference must never be
//! dropped while its own slot's registry lock is held — open paths release
//! the guard first or pin an alias past it.

use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::ControlFlags;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{
    AliasOf, Charge, FileBacking, SharedCharge, root as quota_root, try_charge,
};
use slopos_ostd::{KArc, KWeak};

use super::driver::TtyDriverKind;
use super::pty;
use super::table::{TTY_BACKINGS, TTY_SLAVE_OPENS, TTY_SLOTS, free_slot};
use super::{MAX_TTYS, TtyError, TtyFlags, TtyIndex};

/// Owning handle for one TTY slot.
///
/// The charge accounts for the **slot**, and a PTY pair genuinely occupies
/// two of them — `pty_alloc` draws `find_free_slot` twice — so each backing
/// carries its own, both billed to the `/dev/ptmx` opener. What must not
/// happen is a charge per slave *fd*: every slave fd aliases one
/// [`TtySlaveOpen`], which is why that type carries [`AliasOf`] instead.
#[derive(slopos_ostd::Charged)]
pub struct TtyBacking {
    idx: TtyIndex,
    peer: PeerLink,
    object_charge: SharedCharge,
}

enum PeerLink {
    /// Serial or virtual console — no peer.
    Console,
    MasterOf(KArc<TtyBacking>),
    SlaveOf(KWeak<TtyBacking>),
}

slopos_ostd::charge_audit!(TtyBacking);

impl FileBacking for TtyBacking {}

impl TtyBacking {
    pub fn index(&self) -> TtyIndex {
        self.idx
    }

    pub fn is_pty_master(&self) -> bool {
        matches!(self.peer, PeerLink::MasterOf(_))
    }

    pub fn is_pty_slave(&self) -> bool {
        matches!(self.peer, PeerLink::SlaveOf(_))
    }

    /// The slave end of a master backing.
    pub(crate) fn slave_link(&self) -> Option<KArc<TtyBacking>> {
        match &self.peer {
            PeerLink::MasterOf(slave) => Some(slave.clone()),
            _ => None,
        }
    }

    /// The master end of a slave backing, if it is still alive.
    fn master_link(&self) -> Option<KArc<TtyBacking>> {
        match &self.peer {
            PeerLink::SlaveOf(master) => master.upgrade(),
            _ => None,
        }
    }

    /// Build a linked master/slave backing pair; the slave is constructed
    /// inside the master's cyclic initialiser, so both links are valid from
    /// birth.
    pub(crate) fn new_pair(
        master_idx: TtyIndex,
        slave_idx: TtyIndex,
        account: AccountId,
    ) -> Option<(KArc<TtyBacking>, KArc<TtyBacking>)> {
        // Both slots are charged before either object exists, so a refusal
        // never leaves half a pair built. One two-unit token on the master
        // rather than one each: `try_new_cyclic`'s initialiser is `FnOnce` but
        // not statically known to run once, so a distinct slave token would
        // need an `Option<Charge<_>>` — the separation linearity forbids.
        let master_charge = Charge::commit(try_charge::<ObjectRow>(account, 2).ok()?);
        let mut slave_alloc_failed = false;
        let master = KArc::try_new_cyclic(|master_weak| {
            match KArc::try_new(TtyBacking {
                idx: slave_idx,
                peer: PeerLink::SlaveOf(master_weak.clone()),
                object_charge: SharedCharge::Alias(AliasOf {
                    owner: "the master TtyBacking's two-slot charge",
                }),
            }) {
                Ok(slave) => TtyBacking {
                    idx: master_idx,
                    peer: PeerLink::MasterOf(slave),
                    object_charge: SharedCharge::Owner(master_charge),
                },
                Err(_) => {
                    // Degenerate placeholder: dropped by the caller against
                    // still-empty slots, where every teardown arm no-ops.
                    slave_alloc_failed = true;
                    TtyBacking {
                        idx: master_idx,
                        peer: PeerLink::Console,
                        object_charge: SharedCharge::Owner(master_charge),
                    }
                }
            }
        })
        .ok()?;
        if slave_alloc_failed {
            return None;
        }
        let slave = master.slave_link()?;
        Some((master, slave))
    }
}

impl Drop for TtyBacking {
    fn drop(&mut self) {
        match &self.peer {
            PeerLink::Console => console_last_close(self.idx),
            PeerLink::MasterOf(slave) => {
                // Hang up the slave while our strong link still pins its slot,
                // then free our own.
                super::lifecycle::hangup(slave.index());
                free_slot(self.idx);
            }
            PeerLink::SlaveOf(_) => {
                // Reachable only once the master (strong holder) is gone
                // and no slave open holds us: the pair is fully closed.
                free_slot(self.idx);
            }
        }
    }
}

/// Shared open-tracking object for a PTY slave: every slave fd owns an alias
/// of one of these, so its `Drop` is the last slave fd closing.
#[derive(slopos_ostd::Charged)]
pub struct TtySlaveOpen {
    backing: KArc<TtyBacking>,
    /// A charge here would be per-fd rather than per-object; the slot it names
    /// is charged on the [`TtyBacking`] it points at.
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(TtySlaveOpen);

impl FileBacking for TtySlaveOpen {}

impl TtySlaveOpen {
    pub fn index(&self) -> TtyIndex {
        self.backing.idx
    }
}

impl Drop for TtySlaveOpen {
    fn drop(&mut self) {
        if let Some(master) = self.backing.master_link() {
            pty::peer_closed(master.index());
        }
    }
}

/// Console last-close: HUPCL with an attached session hangs the terminal
/// up; otherwise flush and detach in place. The slot is never freed —
/// consoles are permanent.
///
/// The slot mutation runs under the slot's backing-registry lock, which also
/// serialises `open_tty`'s mint of a replacement backing: a close observing a
/// live registry entry is superseded and bows out. Liveness is
/// `strong_count() > 0` rather than `upgrade()` — a transient strong reference
/// dropped here could itself become the last one and re-enter this function
/// under the registry lock.
fn console_last_close(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let notify_sid = {
        let reg = TTY_BACKINGS[slot].lock();
        if reg.strong_count() > 0 {
            return;
        }
        let hangup_now = {
            let mut guard = TTY_SLOTS[slot].lock();
            let Some(tty) = guard.as_mut() else { return };
            let hupcl = tty
                .ldisc
                .termios()
                .control_flags()
                .contains(ControlFlags::HUPCL);
            let sid = tty.session.session_id();
            // Without a session there is no process group to receive SIGHUP and
            // no DTR line to drop; POSIX leaves HUPCL implementation-defined
            // for terminals without modem control.
            if hupcl && sid != 0 {
                tty.flags.remove(TtyFlags::EXCLUSIVE);
                true
            } else {
                tty.ldisc.flush_all();
                tty.session.detach();
                tty.flags
                    .remove(TtyFlags::HUNG_UP | TtyFlags::PEER_CLOSED | TtyFlags::EXCLUSIVE);
                false
            }
        };
        if hangup_now {
            super::lifecycle::hangup_mark(idx)
        } else {
            None
        }
    };
    // Signals and wakeups fire outside every lock.
    if let Some(sid) = notify_sid {
        super::lifecycle::hangup_notify(idx, sid);
    }
}

/// Open the TTY at `idx`: share the live owner, or mint a fresh one for a
/// console slot whose previous backing has fully closed. PTY pairs cannot
/// be resurrected — a dead backing means the pair is (mid-)gone. Slave
/// indices resolve to the shared [`TtySlaveOpen`] (no `TIOCSPTLCK` check,
/// matching historical by-index open semantics).
pub fn open_tty(idx: TtyIndex) -> Result<KArc<dyn FileBacking>, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    loop {
        {
            let reg = TTY_BACKINGS[slot].lock();
            if let Some(existing) = reg.upgrade() {
                if existing.is_pty_slave() {
                    drop(reg);
                    return Ok(open_slave_shared(existing)?);
                }
                if let Err(e) = exclusive_gate(&existing) {
                    // Release the registry lock before `existing` drops: were
                    // this upgrade the last strong reference, its teardown
                    // retakes the same lock.
                    drop(reg);
                    return Err(e);
                }
                clear_own_latches(slot);
                return Ok(existing);
            }
        }

        {
            let guard = TTY_SLOTS[slot].lock();
            let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
            match tty.driver {
                TtyDriverKind::SerialConsole(_) | TtyDriverKind::VConsole(_) => {}
                _ => return Err(TtyError::NotAllocated),
            }
        }

        // Allocate with no lock held: a failed allocation drops a console
        // backing whose teardown takes the registry lock. Charged to the root
        // because a console slot is a fixed boot resource no process created,
        // so billing the first opener would make a shell's budget depend on
        // boot ordering.
        let backing = KArc::try_new(TtyBacking {
            idx,
            peer: PeerLink::Console,
            object_charge: SharedCharge::Owner(Charge::commit(
                try_charge::<ObjectRow>(quota_root(), 1).map_err(|_| TtyError::OutOfMemory)?,
            )),
        })
        .map_err(|_| TtyError::OutOfMemory)?;

        {
            let mut reg = TTY_BACKINGS[slot].lock();
            if reg.strong_count() == 0 {
                *reg = KArc::downgrade(&backing);
                // Re-open semantics: latches clear under the registry lock,
                // serialised against the previous backing's stale close.
                clear_own_latches(slot);
                return Ok(backing);
            }
        }
        // A concurrent open registered its backing first; ours tears down
        // harmlessly and the retry adopts the winner.
        drop(backing);
    }
}

/// Open the PTY slave at `idx` (`/dev/pts/N`). Fails on a locked slave
/// (`TIOCSPTLCK`) and on a dead pair.
pub fn pty_open_slave(idx: TtyIndex) -> Result<KArc<dyn FileBacking>, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let existing = {
        let reg = TTY_BACKINGS[slot].lock();
        reg.upgrade().ok_or(TtyError::NotAllocated)?
    };
    if !existing.is_pty_slave() {
        return Err(TtyError::NotAllocated);
    }
    if slave_locked(slot) {
        return Err(TtyError::PermissionDenied);
    }
    Ok(open_slave_shared(existing)?)
}

/// Open the slave paired with the master at `master_idx` (`TIOCGPTPEER`).
pub fn pty_open_peer(master_idx: TtyIndex) -> Result<(TtyIndex, KArc<dyn FileBacking>), TtyError> {
    let master_slot = master_idx.0 as usize;
    if master_slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let master = {
        let reg = TTY_BACKINGS[master_slot].lock();
        reg.upgrade().ok_or(TtyError::NotAllocated)?
    };
    let slave = master.slave_link().ok_or(TtyError::NotAllocated)?;
    let slave_idx = slave.index();
    if slave_locked(slave_idx.0 as usize) {
        return Err(TtyError::PermissionDenied);
    }
    Ok((slave_idx, open_slave_shared(slave)?))
}

/// Share (or mint) the slave's open-tracking object: the first open after
/// every fd closed mints a fresh one, giving the master a new EOF edge for
/// the next last-close.
pub(crate) fn open_slave_shared(backing: KArc<TtyBacking>) -> Result<KArc<TtySlaveOpen>, TtyError> {
    let slot = backing.idx.0 as usize;
    let exclusive = {
        let guard = TTY_SLOTS[slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.flags.contains(TtyFlags::EXCLUSIVE))
    };
    // A failure path below can drop the last alias of the slave backing under
    // the registry lock, and its teardown retakes that lock. This pin outlives
    // the guard, so the teardown can only run after release.
    let _pin = backing.clone();
    let mut reg = TTY_SLAVE_OPENS[slot].lock();
    if let Some(existing) = reg.upgrade() {
        // TIOCEXCL: an already-open exclusive slave rejects further opens.
        if exclusive {
            return Err(TtyError::DeviceBusy);
        }
        clear_slave_latches(&existing.backing);
        return Ok(existing);
    }
    let open = KArc::try_new(TtySlaveOpen {
        backing,
        object_charge: AliasOf {
            owner: "the slave TtyBacking's slot charge",
        },
    })
    .map_err(|_| TtyError::OutOfMemory)?;
    *reg = KArc::downgrade(&open);
    clear_slave_latches(&open.backing);
    Ok(open)
}

fn slave_locked(slot: usize) -> bool {
    let guard = TTY_SLOTS[slot].lock();
    matches!(guard.as_ref(), Some(tty) if tty.flags.contains(TtyFlags::SLAVE_LOCKED))
}

/// TIOCEXCL for consoles and masters: reject opens of an exclusive TTY that
/// is already open — any strong reference beyond the caller's fresh clone.
/// Transient data-path pins inflate the count during a cross-end write, so a
/// concurrent writer may surface a spurious `DeviceBusy`.
fn exclusive_gate(backing: &KArc<TtyBacking>) -> Result<(), TtyError> {
    let slot = backing.index().0 as usize;
    let exclusive = {
        let guard = TTY_SLOTS[slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.flags.contains(TtyFlags::EXCLUSIVE))
    };
    if exclusive && KArc::strong_count(backing) > 1 {
        return Err(TtyError::DeviceBusy);
    }
    Ok(())
}

fn clear_own_latches(slot: usize) {
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.flags.remove(TtyFlags::HUNG_UP | TtyFlags::PEER_CLOSED);
    }
}

/// Re-open semantics for a slave: clear its latches and the master's
/// `PEER_CLOSED` — but only while the master is still alive; a dead pair
/// stays hung up.
fn clear_slave_latches(backing: &KArc<TtyBacking>) {
    let Some(master) = backing.master_link() else {
        return;
    };
    clear_own_latches(backing.idx.0 as usize);
    pty::clear_peer_closed(master.index());
}
