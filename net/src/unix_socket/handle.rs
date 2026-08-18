//! Type-safe handle for AF_UNIX socket kernel objects.

use slopos_ostd::handle::Handle;

use super::MAX_UNIX_SOCKETS;
use super::slot::UnixSlot;

/// Slot-index bit width in the packed fd handle; the rest hold the generation.
/// 8 bits cover [`MAX_UNIX_SOCKETS`] slots.
pub(super) const SLOT_BITS: u32 = 8;

/// Opaque handle identifying an AF_UNIX socket slot.
///
/// Generation-checked, so a handle outliving its socket fails validation rather
/// than aliasing whatever recycled the slot. Packed into the open-file
/// `handle: usize` via [`Handle::pack`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SocketHandle(Handle<UnixSlot>);

impl SocketHandle {
    pub(super) fn from_handle(h: Handle<UnixSlot>) -> Self {
        Self(h)
    }

    pub(super) fn handle(self) -> Handle<UnixSlot> {
        self.0
    }

    /// Raw slot index. Bounds- but **not** generation-checked; all slot *data*
    /// access must go through the table.
    pub(super) fn slot(self) -> usize {
        self.0.slot() as usize
    }

    /// Slot index for event-bus keying, or `None` if out of range; keying a
    /// recycled slot's queue costs at most a spurious wakeup.
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
