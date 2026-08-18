//! Per-slot AF_UNIX socket state, encoded as a typestate enum.
//!
//! Slot occupancy and the generation counter are owned by the registry's
//! `HandleTable`, not by the slot.

use super::handle::SocketHandle;
use super::pair::{PairHandle, PairSide};

/// Maximum abstract namespace path length; matches POSIX `sockaddr_un::sun_path`.
pub(super) const UNIX_PATH_MAX: usize = 108;

pub(super) const MAX_BACKLOG: usize = 32;

pub(super) enum SlotState {
    /// `unix_create()` succeeded. Also the neutral placeholder while a state
    /// transition moves data out of the previous variant.
    Created,
    /// `unix_bind()` succeeded.
    Bound { path: slopos_ostd::KVec<u8> },
    /// `unix_listen()` succeeded. `KVecDeque` keeps `accept()`'s pop-front O(1).
    Listening {
        path: slopos_ostd::KVec<u8>,
        backlog: slopos_ostd::KVecDeque<SocketHandle>,
    },
    /// `unix_connect()` (caller side) or accept (peer side) succeeded.
    Connected {
        pair: PairHandle,
        side: PairSide,
        peer: SocketHandle,
        peer_closed: bool,
    },
}

pub(super) struct UnixSlot {
    pub(super) state: SlotState,
    /// Socket-level option; persists across state transitions.
    pub(super) nonblocking: bool,
}

impl UnixSlot {
    pub(super) fn created() -> Self {
        Self {
            state: SlotState::Created,
            nonblocking: false,
        }
    }
}
