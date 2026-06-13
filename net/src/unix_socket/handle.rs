//! Type-safe handle for AF_UNIX socket kernel objects.

use slopos_ostd::handle::Handle;

use super::MAX_UNIX_SOCKETS;
use super::slot::UnixSlot;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]). 8 bits cover MAX_UNIX_SOCKETS slots.
pub(super) const SLOT_BITS: u32 = 8;

/// Opaque handle identifying an AF_UNIX socket slot.
///
/// Wraps a generation-checked [`Handle`] over the registry's [`UnixSlot`]
/// table, so a handle left over from a closed socket whose slot was recycled
/// fails validation rather than aliasing the recycled socket. Packed into the
/// open-file `handle: usize` via [`Handle::pack`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SocketHandle(Handle<UnixSlot>);

impl SocketHandle {
    /// Wrap a freshly-minted table handle.
    pub(super) fn from_handle(h: Handle<UnixSlot>) -> Self {
        Self(h)
    }

    /// The underlying table handle. Resolving it against the registry table
    /// validates the generation, so a stale handle yields a typed miss.
    pub(super) fn handle(self) -> Handle<UnixSlot> {
        self.0
    }

    /// Raw slot index — used to key the socket's event-bus queue and to wake
    /// a peer. Bounds- but **not** generation-checked; all slot *data* access
    /// must go through the table (which validates the generation).
    pub(super) fn slot(self) -> usize {
        self.0.slot() as usize
    }

    /// Slot index for event-bus keying, or `None` if out of range. Keying a
    /// recycled slot's queue is harmless (spurious wakeups are tolerated).
    pub(crate) fn slot_for_wq(self) -> Option<usize> {
        let i = self.slot();
        if i < MAX_UNIX_SOCKETS { Some(i) } else { None }
    }

    /// Pack into the `usize` stored in the open-file entry.
    pub fn as_usize(self) -> usize {
        self.0.pack(SLOT_BITS)
    }

    /// Reconstruct from the `usize` stored in the open-file entry.
    pub fn from_usize(v: usize) -> Self {
        Self(Handle::unpack(v, SLOT_BITS))
    }
}
