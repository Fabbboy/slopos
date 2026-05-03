//! Per-pair connection state for connected AF_UNIX socket pairs.
//!
//! Buffer ownership lives here, not on the slot.  Each connected pair
//! holds its FIFOs and ancillary queues exactly once; both slots
//! reference the same pair via a [`PairHandle`].  Refcount lives on
//! the pair: it is `2` while both endpoints are connected, dropped to
//! `1` when one side closes, and dropped to `0` (freeing the pair) when
//! the second side closes.

use slopos_ostd::{AllocError, KVec};

use super::MAX_UNIX_SOCKETS;
use super::buffer::UnixFifo;

/// Soft cap on in-flight file descriptors per direction (SCM_RIGHTS).
///
/// The queue itself is a `KVec` so the bound is enforced at the call
/// site, not by the storage shape.  Mirrors Linux's `SCM_MAX_FD`
/// philosophy: a hard upper limit prevents a sender from pinning
/// arbitrary kernel memory; the exact number is a policy choice.
pub(super) const MAX_INFLIGHT_FDS: usize = 8;

/// Pair-table slab capacity.  Every pair owns two slots, so the table
/// can never need more than half as many entries as the slot table.
pub(super) const MAX_UNIX_PAIRS: usize = MAX_UNIX_SOCKETS / 2;

/// Which side of a connected pair this slot represents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PairSide {
    A,
    B,
}

/// A file descriptor reference in transit through a Unix socket.
pub(super) struct InFlightFd {
    pub(super) handle: usize,
    pub(super) ops: &'static dyn slopos_abi::file_ops::FileOps,
}

/// Per-direction queue of in-flight fds (SCM_RIGHTS side-channel).
///
/// Backed by a `KVec` — entries live in their own heap allocation, so
/// the queue's footprint inside [`ConnectionPair`] is just a vector
/// header (~24 bytes) regardless of how many fds are queued.  On drop,
/// any unclaimed fds are released via `ops.release` to avoid leaking
/// kernel-side fd references.
pub(super) struct AncillaryQueue {
    entries: KVec<InFlightFd>,
}

impl AncillaryQueue {
    pub(super) const fn new() -> Self {
        Self {
            entries: KVec::new(),
        }
    }

    /// Push an fd, capped at [`MAX_INFLIGHT_FDS`].  Returns `false` if
    /// the cap is reached or the underlying allocation cannot grow.
    pub(super) fn push(&mut self, fd: InFlightFd) -> bool {
        if self.entries.len() >= MAX_INFLIGHT_FDS {
            return false;
        }
        self.entries.push(fd).is_ok()
    }

    /// Drain all entries.  The returned `KVec` owns the fds; the
    /// caller forwards each to userspace or releases on overflow.
    pub(super) fn drain(&mut self) -> KVec<InFlightFd> {
        core::mem::replace(&mut self.entries, KVec::new())
    }
}

impl Drop for AncillaryQueue {
    fn drop(&mut self) {
        // Release all unclaimed fds before the KVec backing storage drops.
        for fd in self.entries.drain(..) {
            fd.ops.release(fd.handle);
        }
    }
}

/// Identifies a connection pair in the [`PairTable`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct PairHandle(u8);

/// Shared state owned jointly by both halves of a connected AF_UNIX pair.
///
/// Body size after the `KVec` switch is ~120 bytes — well under the
/// 2 KiB stack-frame budget — so this struct is constructed by safe
/// `Self { … }` syntax with no in-place-init machinery required.
pub(super) struct ConnectionPair {
    a_to_b: UnixFifo,
    b_to_a: UnixFifo,
    anc_a_to_b: AncillaryQueue,
    anc_b_to_a: AncillaryQueue,
    /// Live reference count.  Initialised to `2` at connect; each close
    /// of either endpoint decrements it.  When it reaches `0` the pair
    /// is removed from the table and its FIFOs / queues drop normally.
    refcount: u8,
}

impl ConnectionPair {
    fn new() -> Result<Self, AllocError> {
        Ok(Self {
            a_to_b: UnixFifo::new()?,
            b_to_a: UnixFifo::new()?,
            anc_a_to_b: AncillaryQueue::new(),
            anc_b_to_a: AncillaryQueue::new(),
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

/// Slab of [`ConnectionPair`]s.  Inline storage — the pair body is
/// small enough (~120 bytes) that boxing would cost an extra
/// indirection without benefit.  The two FIFOs inside each pair
/// already carry their 16 KiB payloads on the heap via `KVecDeque`.
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

    /// Decrement the refcount of `handle`.  Returns `true` if the pair
    /// was freed (refcount reached zero) — the caller should then no
    /// longer use the handle.
    pub(super) fn release(&mut self, handle: PairHandle) -> bool {
        let entry = match self.pairs.get_mut(handle.0 as usize) {
            Some(e) => e,
            None => return false,
        };
        match entry {
            Some(pair) => {
                pair.refcount = pair.refcount.saturating_sub(1);
                if pair.refcount == 0 {
                    *entry = None; // drops ConnectionPair → drops queues → releases fds
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }
}
