//! Type-safe handle for AF_UNIX socket kernel objects.

use super::MAX_UNIX_SOCKETS;

/// Bits used for the slot index in the handle encoding.
pub(super) const SLOT_BITS: u32 = 8;
/// Mask for the slot-index portion of the encoded handle (supports up to 256 slots).
pub(super) const SLOT_MASK: usize = (1 << SLOT_BITS) - 1;

/// Opaque handle identifying an AF_UNIX socket slot.
///
/// Encodes a slot index and the slot's generation counter so that stale
/// handles (from a closed socket whose slot was recycled) are reliably
/// rejected.
///
/// The encoding is `(generation << SLOT_BITS) | slot_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct SocketHandle(u32);

impl SocketHandle {
    pub(crate) fn new(slot: usize, generation: u32) -> Self {
        Self(((generation as usize) << SLOT_BITS | (slot & SLOT_MASK)) as u32)
    }

    /// Extract the raw slot index.  Private — only `validate_socket_handle`
    /// should use this (it IS the generation check).
    pub(super) fn raw_slot(self) -> usize {
        (self.0 as usize) & SLOT_MASK
    }

    /// Slot index for wait-queue indexing (static `SOCKET_WQS` arrays).
    ///
    /// This performs a **bounds check** against `MAX_UNIX_SOCKETS` but does
    /// **not** validate the generation counter.  Safe for `SOCKET_WQS[i]`
    /// because the wait-queue array is a fixed-size static — indexing a
    /// recycled slot's queue is harmless (spurious wakeups are tolerated).
    /// All slot *data* access must go through `validate_socket_handle`.
    pub(crate) fn slot_for_wq(self) -> Option<usize> {
        let i = (self.0 as usize) & SLOT_MASK;
        if i < MAX_UNIX_SOCKETS { Some(i) } else { None }
    }

    pub(super) fn generation(self) -> u32 {
        (self.0 as usize >> SLOT_BITS) as u32
    }

    /// Convert to usize for storage in OpenFileEntry.handle.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from usize stored in OpenFileEntry.handle.
    pub fn from_usize(v: usize) -> Self {
        Self(v as u32)
    }
}
