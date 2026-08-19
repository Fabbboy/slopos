//! [`FileKind::Seat`] file operations — the descriptor form of a held seat.
//!
//! The fd is a designator, not a byte stream: `read` and `write` are `EINVAL`.
//! Its `handle` packs the [`SeatKind`] and the grant epoch, so resolving it
//! yields both what resource is named and whether the grant is still the live
//! one. A seat revoked out from under a holder leaves the fd open and every
//! operation through it failing, rather than silently retargeting.
//!
//! Non-duplicable and non-transferable: the entry is stamped
//! `FdRights::LINEAR` at creation, and every aliasing and moving path reads
//! that stamp rather than re-deriving it. The backing's `Drop` does **not**
//! release the seat —
//! release is arbiter revocation from the task cleanup hook, because a
//! reference cycle among holders would otherwise wedge the display.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};
use slopos_ostd::seat::{self, SeatGrant, SeatId, SeatKind};

pub struct SeatFileOps;

pub static SEAT_FILE_OPS: SeatFileOps = SeatFileOps;

/// Pack a grant into the `usize` the fd layer carries.
///
/// Low 8 bits the kind, the rest the epoch. The epoch is what makes a stale
/// fd fail closed: it is compared against the arbiter's live value on every
/// resolve, so a revoked holder's descriptor authorizes nothing.
#[inline]
fn pack(kind: SeatKind, epoch: u64) -> usize {
    ((epoch as usize) << 8) | (kind.as_u8() as usize)
}

/// Owns nothing the arbiter does not already own: the seat is released by
/// [`seat::revoke_for_task`], not here. This exists to carry the quota charge
/// for the object row, so a process cannot mint unbounded seat fds.
#[derive(slopos_ostd::Charged)]
pub(crate) struct SeatBacking {
    pub(crate) object_charge: Charge<ObjectRow>,
}

impl FileBacking for SeatBacking {}

impl FileOps for SeatFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Seat
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_events(&self, _handle: usize, _events: u16) -> u16 {
        0
    }

    fn poll_wait(&self, _handle: usize) -> bool {
        false
    }
}

/// Take `kind`'s seat at rank `id` for `task_id` and install a descriptor
/// naming it.
///
/// Returns the fd (`>= 0`), or a negated errno: `EBUSY` when a seat of equal
/// or higher rank is held by a live task.
pub fn seat_acquire_fd(table: FdTable, kind: SeatKind, id: SeatId, task_id: u32) -> i32 {
    let Ok(reservation) = try_charge::<ObjectRow>(table.account(), 1) else {
        return Errno::ENFILE.raw();
    };
    let grant = match seat::acquire(kind, id, task_id) {
        Ok(grant) => grant,
        Err(seat::SeatError::Busy) => return Errno::EBUSY.raw(),
    };
    let backing: KArc<dyn FileBacking> = match KArc::try_new(SeatBacking {
        object_charge: Charge::commit(reservation),
    }) {
        Ok(backing) => backing,
        Err(_) => {
            seat::release(kind, task_id);
            return Errno::ENOMEM.raw();
        }
    };
    let fd = slopos_fs::fileio_open_fd_with_ops(
        table,
        &SEAT_FILE_OPS,
        pack(kind, grant.epoch()),
        Some(backing),
        // Neither fork nor exec carries a seat forward: the child is a
        // different principal and the arbiter revokes at exec anyway.
        slopos_fs::FdFlags::PROCESS_PRIVATE,
    );
    if fd < 0 {
        seat::release(kind, task_id);
    }
    fd
}

/// Whether `task_id` holds `kind`'s seat.
///
/// The display syscalls check this rather than resolving a descriptor
/// argument, because their ABI predates the seat and takes no fd. The seat is
/// still the authority: the arbiter decides who holds it, the descriptor is
/// the holder's evidence, and this asks the arbiter directly.
///
/// A descriptor-taking form was written and then deleted rather than left
/// unused: a security helper with no callers drifts out of step with the one
/// that is actually used, and the next person to need it copies whichever they
/// find first. When `fb_flip` grows an fd argument, it comes back with a
/// caller.
#[inline]
pub fn seat_held_by(kind: SeatKind, task_id: u32) -> bool {
    seat::is_held_by(kind, task_id)
}

#[allow(dead_code)]
type _UnusedGrant = SeatGrant;
