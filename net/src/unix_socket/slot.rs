//! Per-slot AF_UNIX socket state, encoded as a typestate enum.
//!
//! Each variant carries exactly the data that is meaningful in that state —
//! the bind path appears only in `Bound`/`Listening`, the backlog only in
//! `Listening`, the pair handle only in `Connected`.  The compiler enforces
//! these invariants: there is no way to read a path off a freshly-created
//! slot or to reach a buffer from an unconnected socket.
//!
//! Slot occupancy and the generation counter are owned by the registry's
//! `HandleTable` (a free slot is simply absent from the table; `remove` bumps
//! its generation). `nonblocking` is the only cross-state field — a
//! socket-level option that survives state changes.

use super::handle::SocketHandle;
use super::pair::{PairHandle, PairSide};

/// Maximum abstract namespace path length (matches the POSIX
/// `sockaddr_un::sun_path` size).
pub(super) const UNIX_PATH_MAX: usize = 108;

/// Maximum pending connections in the accept backlog.
/// Matches Wayland's libwayland-server default of 128.
pub(super) const MAX_BACKLOG: usize = 32;

/// Per-slot state.
///
/// Each variant owns the data that is meaningful while the slot is in
/// that state.  Transitions happen via assignment — `slot.state =
/// SlotState::Bound { path }` etc. — so the previous variant's data
/// drops automatically.
pub(super) enum SlotState {
    /// `unix_create()` succeeded; socket has no address and no peer. Also the
    /// neutral placeholder while a state transition moves data out of the
    /// previous variant.
    Created,
    /// `unix_bind()` succeeded.
    Bound {
        /// Heap-allocated copy of the abstract namespace path bytes.
        path: slopos_ostd::KVec<u8>,
    },
    /// `unix_listen()` succeeded.  Backlog is a `KVecDeque` so
    /// `accept()`'s pop-front is O(1).
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
    /// Non-blocking mode (socket-level option, persists across
    /// state transitions).
    pub(super) nonblocking: bool,
}

impl UnixSlot {
    /// A freshly-created, unbound socket. Inserted into the registry table by
    /// `unix_create`, which mints the generation-checked handle.
    pub(super) fn created() -> Self {
        Self {
            state: SlotState::Created,
            nonblocking: false,
        }
    }
}
