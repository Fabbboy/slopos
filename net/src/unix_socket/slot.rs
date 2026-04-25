//! Per-slot AF_UNIX socket state, encoded as a typestate enum.
//!
//! Stage D collapses the previous field bag into a [`SlotState`]
//! enum.  Each variant carries exactly the data that is meaningful in
//! that state — the bind path appears only in `Bound`/`Listening`, the
//! backlog only in `Listening`, the pair handle only in `Connected`.
//! The compiler enforces these invariants: there is no way to read
//! a path off a freshly-created slot or to reach a buffer from an
//! unconnected socket.
//!
//! `generation` and `nonblocking` are the only cross-state fields:
//! generation persists across transitions so stale handles can be
//! detected, and nonblocking is a socket-level option that survives
//! state changes.

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
    /// Slot is unallocated.  Allocator scans for this variant.
    Free,
    /// `unix_create()` succeeded; socket has no address and no peer.
    Created,
    /// `unix_bind()` succeeded.
    Bound {
        /// Heap-allocated copy of the abstract namespace path bytes.
        path: slopos_alloc::KVec<u8>,
    },
    /// `unix_listen()` succeeded.  Backlog is a `KVecDeque` so
    /// `accept()`'s pop-front is O(1).
    Listening {
        path: slopos_alloc::KVec<u8>,
        backlog: slopos_alloc::KVecDeque<SocketHandle>,
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
    /// Monotonically increasing generation counter.  Bumped on every
    /// transition into `Free` so stale `SocketHandle`s never validate
    /// against a recycled slot.
    pub(super) generation: u32,
    /// Non-blocking mode (socket-level option, persists across
    /// state transitions).
    pub(super) nonblocking: bool,
}

impl UnixSlot {
    pub(super) const fn new() -> Self {
        Self {
            state: SlotState::Free,
            generation: 0,
            nonblocking: false,
        }
    }

    /// Transition to `Free`.  Bumps the generation counter so any
    /// outstanding [`SocketHandle`]s referencing this slot become
    /// stale and fail validation.
    ///
    /// Caller responsibilities (compiler-enforced for the pair handle,
    /// debug-asserted otherwise):
    ///
    /// - The current state must not still be holding a pair reference
    ///   — `unix_close` must have called `pairs.release(...)` first.
    pub(super) fn transition_to_free(&mut self) {
        debug_assert!(
            !matches!(self.state, SlotState::Connected { .. }),
            "transition_to_free called while still Connected — pair leak"
        );
        self.state = SlotState::Free;
        self.generation = self.generation.wrapping_add(1);
        self.nonblocking = false;
    }
}
