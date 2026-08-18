//! Per-pair connection state for connected AF_UNIX socket pairs.
//!
//! Buffer ownership lives here, not on the slot: each connected pair holds its
//! FIFOs and ancillary queues exactly once, and both slots reference it via a
//! [`PairHandle`].

use slopos_abi::quota::CustodyAxis;
use slopos_fs::FileRef;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};
use slopos_ostd::{AllocError, KVec};

use super::MAX_UNIX_SOCKETS;
use super::buffer::UnixFifo;

/// Soft cap on in-flight file descriptors per direction (SCM_RIGHTS).
///
/// Enforced at the call site, not by the storage shape: the bound stops a
/// sender pinning arbitrary kernel memory; the exact number is policy.
pub(super) const MAX_INFLIGHT_FDS: usize = 8;

/// Every pair owns two slots, so the table can never need more than half as
/// many entries as the slot table.
pub(super) const MAX_UNIX_PAIRS: usize = MAX_UNIX_SOCKETS / 2;

/// Which side of a connected pair this slot represents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PairSide {
    A,
    B,
}

/// Per-direction queue of in-flight files (SCM_RIGHTS side-channel).
///
/// Entries are owned [`FileRef`] aliases: queueing takes custody, delivery
/// moves it to the receiver's fd table, and dropping the queue closes whatever
/// was never claimed.
///
/// Storage is pre-reserved to [`MAX_INFLIGHT_FDS`] at pair creation so
/// commit-path pushes never allocate and never drop a `FileRef` under the
/// socket state lock — a drop can recurse into `unix_close`.
pub(super) struct AncillaryQueue {
    entries: KVec<InFlightFile>,
}

/// One in-flight descriptor and the custody charge that accounts for it.
///
/// The charge is the **sender's** and is mandatory: 8 in-flight descriptors x
/// 2 directions x 16 pairs is 256 `FileRef`s held by no descriptor table at
/// all, against a far lower per-process descriptor limit.
///
/// The charge travels with the reference, so whichever way the queue empties
/// the refund happens exactly once, in the same move.
pub(super) struct InFlightFile {
    pub(super) file: FileRef,
    #[expect(dead_code, reason = "held for ownership; dropping it is the refund")]
    custody: Charge<CustodyAxis>,
}

impl AncillaryQueue {
    fn new() -> Result<Self, AllocError> {
        Ok(Self {
            entries: KVec::with_capacity(MAX_INFLIGHT_FDS)?,
        })
    }

    /// Push a file, capped at [`MAX_INFLIGHT_FDS`] and charged to `sender`.
    /// Returns the file back on refusal so the caller can drop it off-lock.
    ///
    /// The charge is minted **after** the only two things that can refuse —
    /// the cap and the reservation — because `KVec::push` consumes its
    /// argument on failure, so a token built before a failing push would be
    /// lost with it.
    pub(super) fn push(&mut self, file: FileRef, sender: AccountId) -> Result<(), FileRef> {
        if self.entries.len() >= MAX_INFLIGHT_FDS {
            return Err(file);
        }
        let Ok(reservation) = try_charge::<CustodyAxis>(sender, 1) else {
            return Err(file);
        };
        let alias = file.alias();
        if self
            .entries
            .push(InFlightFile {
                file,
                custody: Charge::commit(reservation),
            })
            .is_err()
        {
            return Err(alias);
        }
        drop(alias);
        Ok(())
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drain all entries.  The returned `KVec` owns the aliases, so the caller
    /// can carry them out of the state lock before forwarding or dropping them.
    pub(super) fn drain(&mut self) -> KVec<InFlightFile> {
        core::mem::replace(&mut self.entries, KVec::new())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct PairHandle(u8);

/// Shared state owned jointly by both halves of a connected AF_UNIX pair.
///
/// The body is ~120 bytes — well under the 2 KiB stack-frame budget — so it is
/// constructed by plain `Self { … }` with no in-place-init machinery.
pub(super) struct ConnectionPair {
    a_to_b: UnixFifo,
    b_to_a: UnixFifo,
    anc_a_to_b: AncillaryQueue,
    anc_b_to_a: AncillaryQueue,
    /// `2` at connect; each endpoint close decrements, and at `0` the pair
    /// leaves the table and its FIFOs / queues drop.
    refcount: u8,
}

impl ConnectionPair {
    fn new() -> Result<Self, AllocError> {
        Ok(Self {
            a_to_b: UnixFifo::new()?,
            b_to_a: UnixFifo::new()?,
            anc_a_to_b: AncillaryQueue::new()?,
            anc_b_to_a: AncillaryQueue::new()?,
            refcount: 2,
        })
    }

    /// FIFO this side writes into (peer reads).
    pub(super) fn send_fifo(&mut self, side: PairSide) -> &mut UnixFifo {
        match side {
            PairSide::A => &mut self.a_to_b,
            PairSide::B => &mut self.b_to_a,
        }
    }

    /// FIFO this side reads from (peer wrote).
    pub(super) fn recv_fifo(&mut self, side: PairSide) -> &mut UnixFifo {
        match side {
            PairSide::A => &mut self.b_to_a,
            PairSide::B => &mut self.a_to_b,
        }
    }

    /// Read-only view of [`Self::send_fifo`] for poll readiness checks.
    pub(super) fn send_fifo_ref(&self, side: PairSide) -> &UnixFifo {
        match side {
            PairSide::A => &self.a_to_b,
            PairSide::B => &self.b_to_a,
        }
    }

    /// Read-only view of [`Self::recv_fifo`] for poll readiness checks.
    pub(super) fn recv_fifo_ref(&self, side: PairSide) -> &UnixFifo {
        match side {
            PairSide::A => &self.b_to_a,
            PairSide::B => &self.a_to_b,
        }
    }

    /// Ancillary queue this side writes into (peer drains).
    pub(super) fn send_anc(&mut self, side: PairSide) -> &mut AncillaryQueue {
        match side {
            PairSide::A => &mut self.anc_a_to_b,
            PairSide::B => &mut self.anc_b_to_a,
        }
    }

    /// Ancillary queue this side drains from (peer wrote).
    pub(super) fn recv_anc(&mut self, side: PairSide) -> &mut AncillaryQueue {
        match side {
            PairSide::A => &mut self.anc_b_to_a,
            PairSide::B => &mut self.anc_a_to_b,
        }
    }
}

/// Slab of [`ConnectionPair`]s.  Inline storage: boxing so small a body would
/// only add indirection, and each pair's FIFOs already carry their 16 KiB
/// payloads on the heap via `KVecDeque`.
pub(super) struct PairTable {
    pairs: [Option<ConnectionPair>; MAX_UNIX_PAIRS],
}

impl PairTable {
    pub(super) const fn new() -> Self {
        Self {
            pairs: [const { None }; MAX_UNIX_PAIRS],
        }
    }

    /// Allocate a new pair entry.  Returns `Err` on FIFO allocation
    /// failure or `Ok(None)` if the table is full.
    pub(super) fn allocate(&mut self) -> Result<Option<PairHandle>, AllocError> {
        for (idx, slot) in self.pairs.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(ConnectionPair::new()?);
                return Ok(Some(PairHandle(idx as u8)));
            }
        }
        Ok(None)
    }

    pub(super) fn get(&self, handle: PairHandle) -> Option<&ConnectionPair> {
        self.pairs.get(handle.0 as usize).and_then(|s| s.as_ref())
    }

    pub(super) fn get_mut(&mut self, handle: PairHandle) -> Option<&mut ConnectionPair> {
        self.pairs
            .get_mut(handle.0 as usize)
            .and_then(|s| s.as_mut())
    }

    /// Decrement the refcount; at zero the pair is detached and returned so the
    /// caller can drop it *after* releasing the socket state lock — its
    /// ancillary `FileRef`s can recurse into `unix_close` on teardown.
    #[must_use]
    pub(super) fn release(&mut self, handle: PairHandle) -> Option<ConnectionPair> {
        let entry = self.pairs.get_mut(handle.0 as usize)?;
        match entry {
            Some(pair) => {
                pair.refcount = pair.refcount.saturating_sub(1);
                if pair.refcount == 0 {
                    entry.take()
                } else {
                    None
                }
            }
            None => None,
        }
    }
}
