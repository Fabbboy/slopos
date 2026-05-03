//! Kernel AF_UNIX (Unix domain) stream socket implementation.
//!
//! # Design
//!
//! Per-slot state is encoded as a [`slot::SlotState`] enum: `Free`,
//! `Created`, `Bound`, `Listening`, `Connected`.  Each variant carries
//! exactly the data meaningful in that state — paths only exist when
//! bound, backlog only when listening, pair handle only when
//! connected.  The compiler enforces these invariants statically.
//!
//! Connected pairs share a [`pair::ConnectionPair`] managed by
//! [`pair::PairTable`] — both halves reference the same pair via a
//! [`pair::PairHandle`], and the pair owns the bidirectional FIFOs and
//! SCM_RIGHTS queues exactly once.  The pair's refcount keeps it alive
//! while either endpoint still references it; the pair is freed when
//! the second close decrements the count to zero.
//!
//! # Locking
//!
//! Slot data and the pair table are both protected by [`UNIX_STATE`].
//! Wait queues live in separate statics indexed by slot, so wakers and
//! sleepers never hold `UNIX_STATE` and a wait-queue lock
//! simultaneously (same pattern as `fs/src/pipe.rs`).
//!
//! # Module layout
//!
//! - [`handle`] — `SocketHandle` + slot/generation encoding.
//! - [`buffer`] — `UnixFifo` (typed bounded byte FIFO).
//! - [`pair`] — `ConnectionPair`, `PairTable`, `AncillaryQueue`, `PairSide`.
//! - [`slot`] — `UnixSlot` + `SlotState` typestate enum.
//! - this module — global state, public API.

mod buffer;
mod handle;
mod pair;
mod slot;

use slopos_abi::syscall::{POLLHUP, POLLIN, POLLOUT};
use slopos_ostd::{KVec, KVecDeque};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY, WaitQueue};

use pair::{InFlightFd, PairSide, PairTable};
use slot::{MAX_BACKLOG, SlotState, UNIX_PATH_MAX, UnixSlot};

pub use buffer::UNIX_BUF_SIZE;
pub use handle::SocketHandle;

/// Maximum number of concurrent AF_UNIX sockets.
pub const MAX_UNIX_SOCKETS: usize = 32;

// ---------------------------------------------------------------------------
// Wait queues — one per socket slot, separate from UNIX_STATE.
// ---------------------------------------------------------------------------

static SOCKET_WQS: [WaitQueue; MAX_UNIX_SOCKETS] = [const { WaitQueue::new() }; MAX_UNIX_SOCKETS];

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

struct UnixSocketState {
    slots: [UnixSlot; MAX_UNIX_SOCKETS],
    pairs: PairTable,
}

// SAFETY: UnixSocketState is only accessed through the UNIX_STATE IrqMutex.
unsafe impl Send for UnixSocketState {}

impl UnixSocketState {
    const fn new() -> Self {
        Self {
            slots: [const { UnixSlot::new() }; MAX_UNIX_SOCKETS],
            pairs: PairTable::new(),
        }
    }
}

static UNIX_STATE: IrqMutex<UnixSocketState> =
    IrqMutex::new(UnixSocketState::new(), LOCK_LEVEL_REGISTRY);

/// Validate a socket handle against the current state, returning the slot
/// index if the handle is non-Free and the generation matches.
fn validate_socket_handle(state: &UnixSocketState, handle: SocketHandle) -> Option<usize> {
    let i = handle.raw_slot();
    if i >= MAX_UNIX_SOCKETS {
        return None;
    }
    let slot = &state.slots[i];
    if matches!(slot.state, SlotState::Free) {
        return None;
    }
    if slot.generation != handle.generation() {
        return None;
    }
    Some(i)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate a new AF_UNIX socket slot. Returns a [`SocketHandle`] or `None`.
pub fn unix_create() -> Option<SocketHandle> {
    let mut state = UNIX_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if matches!(slot.state, SlotState::Free) {
            slot.state = SlotState::Created;
            return Some(SocketHandle::new(idx, slot.generation));
        }
    }
    None
}

/// Bind a socket to an abstract namespace path.
pub fn unix_bind(handle: SocketHandle, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    // Pre-allocate the path buffer outside the lock.
    let mut owned_path = match KVec::<u8>::with_capacity(path.len()) {
        Ok(v) => v,
        Err(_) => return -12, // ENOMEM
    };
    if owned_path.extend_from_slice(path).is_err() {
        return -12;
    }

    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9; // EBADF
    };
    if !matches!(state.slots[i].state, SlotState::Created) {
        return -22; // EINVAL — must be Created
    }

    // Check for duplicate path.
    for (j, other) in state.slots.iter().enumerate() {
        if j == i {
            continue;
        }
        let other_path: &[u8] = match &other.state {
            SlotState::Bound { path } => path.as_slice(),
            SlotState::Listening { path, .. } => path.as_slice(),
            _ => continue,
        };
        if other_path == path {
            return -98; // EADDRINUSE
        }
    }

    state.slots[i].state = SlotState::Bound { path: owned_path };
    0
}

/// Mark a bound socket as listening.
pub fn unix_listen(handle: SocketHandle, _backlog: u32) -> i32 {
    // Pre-allocate the backlog deque outside the lock.
    let backlog: KVecDeque<SocketHandle> = match KVecDeque::with_capacity(MAX_BACKLOG) {
        Ok(d) => d,
        Err(_) => return -12, // ENOMEM
    };

    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9;
    };
    let slot = &mut state.slots[i];

    // Take ownership of the existing path by swapping the variant out.
    let path = match core::mem::replace(&mut slot.state, SlotState::Free) {
        SlotState::Bound { path } => path,
        other => {
            // Not Bound — restore and reject.
            slot.state = other;
            return -22; // EINVAL
        }
    };
    slot.state = SlotState::Listening { path, backlog };
    0
}

/// Accept a pending connection from a listening socket.
///
/// Blocks the caller until a connection arrives (unless non-blocking).
pub fn unix_accept(handle: SocketHandle) -> Result<SocketHandle, i32> {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Err(-9);
    };

    loop {
        let (nonblocking, got) = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return Err(-9);
            };
            let nb = state.slots[i].nonblocking;
            let slot = &mut state.slots[i];
            let accepted = match &mut slot.state {
                SlotState::Listening { backlog, .. } => backlog.pop_front(),
                _ => return Err(-22), // EINVAL
            };
            (nb, accepted)
        };

        if let Some(accepted_handle) = got {
            return Ok(accepted_handle);
        }

        if nonblocking {
            return Err(-11); // EAGAIN
        }

        SOCKET_WQS[wq_idx].wait_event(|| {
            let state = UNIX_STATE.lock();
            let slot = &state.slots[wq_idx];
            match &slot.state {
                SlotState::Free => true, // slot reused — bail out
                SlotState::Listening { backlog, .. } => !backlog.is_empty(),
                _ => true, // state changed unexpectedly — bail out
            }
        });
    }
}

/// Connect to a listening socket identified by path.
///
/// Allocates a new pair (with both FIFOs) and a fresh slot for the
/// accepted side.  Both sides reference the same pair handle; the
/// listener's backlog records the accepted-side handle for `accept()`
/// to dequeue.
pub fn unix_connect(handle: SocketHandle, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9;
    };

    // Caller must be Created or Bound (not Connected, Listening, etc.).
    match state.slots[i].state {
        SlotState::Created | SlotState::Bound { .. } => (),
        SlotState::Connected { .. } => return -106, // EISCONN
        SlotState::Listening { .. } => return -95,  // EOPNOTSUPP
        SlotState::Free => return -9,
    }

    // Find the listener and verify backlog has space.
    let mut listener_idx = None;
    for (j, slot) in state.slots.iter().enumerate() {
        if let SlotState::Listening {
            path: listener_path,
            backlog,
        } = &slot.state
        {
            if listener_path.as_slice() == path {
                if backlog.len() >= MAX_BACKLOG {
                    return -11; // EAGAIN — backlog full
                }
                listener_idx = Some(j);
                break;
            }
        }
    }
    let listener_idx = match listener_idx {
        Some(li) => li,
        None => return -111, // ECONNREFUSED
    };

    // Allocate a free slot for the accepted side (side B).
    let b_idx = match state
        .slots
        .iter()
        .position(|s| matches!(s.state, SlotState::Free))
    {
        Some(idx) => idx,
        None => return -23, // ENFILE — no free slots
    };

    // Allocate a pair entry; this is where the 16 KiB×2 FIFO heap allocations happen.
    let pair_handle = match state.pairs.allocate() {
        Ok(Some(ph)) => ph,
        Ok(None) => return -23, // ENFILE — pair table full
        Err(_) => return -12,   // ENOMEM
    };

    let a_handle = SocketHandle::new(i, state.slots[i].generation);
    let b_handle = SocketHandle::new(b_idx, state.slots[b_idx].generation);

    // Set up caller (side A).
    state.slots[i].state = SlotState::Connected {
        pair: pair_handle,
        side: PairSide::A,
        peer: b_handle,
        peer_closed: false,
    };

    // Set up accepted side (side B).
    state.slots[b_idx].state = SlotState::Connected {
        pair: pair_handle,
        side: PairSide::B,
        peer: a_handle,
        peer_closed: false,
    };

    // Enqueue B in the listener's backlog.
    if let SlotState::Listening { backlog, .. } = &mut state.slots[listener_idx].state {
        // Pre-reserved at unix_listen, so push_back never realloc'd.
        backlog
            .push_back(b_handle)
            .expect("backlog pre-reserved at listen");
    }

    drop(state);
    SOCKET_WQS[listener_idx].wake_all();

    0
}

/// Send data on a connected AF_UNIX socket.
pub fn unix_send(handle: SocketHandle, data: &[u8]) -> i32 {
    if data.is_empty() {
        return 0;
    }
    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };
    let slot_gen = handle.generation();

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return -107; // ENOTCONN
            };
            let nonblocking = state.slots[i].nonblocking;

            // Extract pair/side/peer from Connected variant; reject otherwise.
            let (pair_handle, side, peer_idx) = match state.slots[i].state {
                SlotState::Connected {
                    pair,
                    side,
                    peer,
                    peer_closed,
                } => {
                    if peer_closed {
                        return -32; // EPIPE
                    }
                    (pair, side, peer.raw_slot())
                }
                _ => return -107,
            };

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => return -32, // EPIPE — pair already freed
            };
            let buf = pair.send_fifo(side);
            if buf.has_space() {
                Ok((buf.write(data), peer_idx))
            } else {
                Err(nonblocking)
            }
        };

        match result {
            Ok((n, peer)) => {
                if peer < MAX_UNIX_SOCKETS {
                    SOCKET_WQS[peer].wake_all();
                }
                return n as i32;
            }
            Err(true) => return -11, // EAGAIN
            Err(false) => {
                SOCKET_WQS[wq_idx].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[wq_idx];
                    if slot.generation != slot_gen {
                        return true; // slot reused
                    }
                    match slot.state {
                        SlotState::Connected {
                            pair,
                            side,
                            peer_closed,
                            ..
                        } => {
                            if peer_closed {
                                return true;
                            }
                            match state.pairs.get(pair) {
                                Some(p) => p.send_fifo_ref(side).has_space(),
                                None => true,
                            }
                        }
                        _ => true, // state diverged — bail out
                    }
                });
            }
        }
    }
}

/// Receive data from a connected AF_UNIX socket.
pub fn unix_recv(handle: SocketHandle, buf: &mut [u8]) -> i32 {
    if buf.is_empty() {
        return 0;
    }
    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };
    let slot_gen = handle.generation();

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return -107;
            };
            let nonblocking = state.slots[i].nonblocking;

            let (pair_handle, side, peer_idx, peer_closed) = match state.slots[i].state {
                SlotState::Connected {
                    pair,
                    side,
                    peer,
                    peer_closed,
                } => (pair, side, peer.raw_slot(), peer_closed),
                _ => return -107,
            };

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => return if peer_closed { 0 } else { -107 },
            };
            let rbuf = pair.recv_fifo(side);
            if !rbuf.is_empty() {
                Ok((rbuf.read(buf), peer_idx))
            } else if peer_closed {
                Ok((0, peer_idx)) // EOF
            } else {
                Err(nonblocking)
            }
        };

        match result {
            Ok((n, peer)) => {
                if n > 0 && peer < MAX_UNIX_SOCKETS {
                    SOCKET_WQS[peer].wake_all();
                }
                return n as i32;
            }
            Err(true) => return -11, // EAGAIN
            Err(false) => {
                SOCKET_WQS[wq_idx].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[wq_idx];
                    if slot.generation != slot_gen {
                        return true;
                    }
                    match slot.state {
                        SlotState::Connected {
                            pair,
                            side,
                            peer_closed,
                            ..
                        } => {
                            if peer_closed {
                                return true;
                            }
                            match state.pairs.get(pair) {
                                Some(p) => !p.recv_fifo_ref(side).is_empty(),
                                None => true,
                            }
                        }
                        _ => true,
                    }
                });
            }
        }
    }
}

/// Send data on a connected AF_UNIX socket, with optional in-flight fds (SCM_RIGHTS).
pub fn unix_sendmsg(
    handle: SocketHandle,
    data: &[u8],
    inflight_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    fd_count: usize,
) -> i32 {
    let bytes_sent = if !data.is_empty() {
        let rc = unix_send(handle, data);
        if rc < 0 {
            return rc;
        }
        rc
    } else {
        0
    };

    if fd_count > 0 {
        let mut state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return -107;
        };
        let (pair_handle, side, peer_idx) = match state.slots[i].state {
            SlotState::Connected {
                pair, side, peer, ..
            } => (pair, side, peer.raw_slot()),
            _ => return -107,
        };
        let pair = match state.pairs.get_mut(pair_handle) {
            Some(p) => p,
            None => return -32, // EPIPE
        };
        let anc = pair.send_anc(side);

        for j in 0..fd_count {
            let (h, ops) = inflight_fds[j];
            if !anc.push(InFlightFd { handle: h, ops }) {
                // Queue full — release remaining fds and return error
                for k in j..fd_count {
                    inflight_fds[k].1.release(inflight_fds[k].0);
                }
                return -12; // ENOMEM (queue full)
            }
            inflight_fds[j] = (0, inflight_fds[j].1);
        }

        drop(state);
        if peer_idx < MAX_UNIX_SOCKETS {
            SOCKET_WQS[peer_idx].wake_all();
        }
    }

    bytes_sent
}

/// Receive data from a connected AF_UNIX socket, with optional in-flight fds.
pub fn unix_recvmsg(
    handle: SocketHandle,
    buf: &mut [u8],
    out_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    max_fds: usize,
) -> (i32, usize) {
    let bytes_read = unix_recv(handle, buf);

    let mut received_fds = 0usize;
    {
        let mut state = UNIX_STATE.lock();
        if let Some(i) = validate_socket_handle(&state, handle) {
            if let SlotState::Connected { pair, side, .. } = state.slots[i].state {
                if let Some(pair_ref) = state.pairs.get_mut(pair) {
                    let anc = pair_ref.recv_anc(side);
                    for fd in anc.drain() {
                        if received_fds < max_fds {
                            out_fds[received_fds] = (fd.handle, fd.ops);
                            received_fds += 1;
                        } else {
                            fd.ops.release(fd.handle);
                        }
                    }
                }
            }
        }
    }

    (bytes_read, received_fds)
}

/// Close an AF_UNIX socket. Wakes all waiters on the peer if connected.
///
/// For listeners, all pending backlog entries (side-B slots that were
/// created by `unix_connect()` but never `accept()`-ed) are closed
/// and their side-A peers are notified.
pub fn unix_close(handle: SocketHandle) -> i32 {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };

    // Wakeup targets collected under the lock; wakes happen after release.
    let mut wake_peer: Option<usize> = None;
    let mut backlog_a_peers: [usize; MAX_BACKLOG] = [usize::MAX; MAX_BACKLOG];
    let mut backlog_wake_count = 0usize;
    let mut was_listener = false;

    {
        let mut state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return -9;
        };

        // Take the closing slot's state out so we can move-match it; we
        // restore something explicit afterwards (Free).
        let old_state = core::mem::replace(&mut state.slots[i].state, SlotState::Free);

        match old_state {
            SlotState::Connected { pair, peer, .. } => {
                let peer_idx = peer.raw_slot();
                if peer_idx < MAX_UNIX_SOCKETS && peer_idx != i {
                    if let SlotState::Connected {
                        peer_closed: ref mut pc,
                        ..
                    } = state.slots[peer_idx].state
                    {
                        *pc = true;
                    }
                }
                wake_peer = Some(peer_idx);
                state.pairs.release(pair);
            }
            SlotState::Listening { ref backlog, .. } => {
                was_listener = true;
                // Each entry is a side-B handle whose slot is in
                // SlotState::Connected.  Tear those down too.
                for h in backlog.iter().copied() {
                    let b_idx = h.raw_slot();
                    if b_idx >= MAX_UNIX_SOCKETS {
                        continue;
                    }
                    // Validate generation against the slot before tearing it down.
                    if state.slots[b_idx].generation != h.generation() {
                        continue;
                    }
                    let b_old = core::mem::replace(&mut state.slots[b_idx].state, SlotState::Free);
                    if let SlotState::Connected {
                        pair: b_pair,
                        peer: b_peer,
                        ..
                    } = b_old
                    {
                        let a_idx = b_peer.raw_slot();
                        if a_idx < MAX_UNIX_SOCKETS
                            && state.slots[a_idx].generation == b_peer.generation()
                        {
                            if let SlotState::Connected {
                                peer_closed: ref mut pc,
                                ..
                            } = state.slots[a_idx].state
                            {
                                *pc = true;
                            }
                            backlog_a_peers[backlog_wake_count] = a_idx;
                            backlog_wake_count += 1;
                        }
                        state.pairs.release(b_pair);
                    }
                    // The B slot's state is already Free; bump generation.
                    state.slots[b_idx].generation = state.slots[b_idx].generation.wrapping_add(1);
                    state.slots[b_idx].nonblocking = false;
                }
            }
            // Free / Created / Bound — nothing extra to release.
            _ => {}
        }

        state.slots[i].transition_to_free();
    }

    if let Some(peer) = wake_peer {
        if peer < MAX_UNIX_SOCKETS {
            SOCKET_WQS[peer].wake_all();
        }
    }
    for k in 0..backlog_wake_count {
        let a_idx = backlog_a_peers[k];
        if a_idx < MAX_UNIX_SOCKETS {
            SOCKET_WQS[a_idx].wake_all();
        }
    }
    if was_listener {
        SOCKET_WQS[wq_idx].wake_all();
    }

    0
}

/// Compute the POLL* bitmask of currently ready events.  Shared
/// between `unix_poll_events` and `unix_poll_fused` so both views
/// stay in lockstep.
fn compute_revents(state: &UnixSocketState, slot_idx: usize, requested: u16) -> u16 {
    let slot = &state.slots[slot_idx];
    match slot.state {
        SlotState::Listening { ref backlog, .. } => {
            if !backlog.is_empty() && (requested & POLLIN) != 0 {
                POLLIN
            } else {
                0
            }
        }
        SlotState::Connected {
            pair,
            side,
            peer_closed,
            ..
        } => {
            let mut revents = 0u16;
            if let Some(pair_ref) = state.pairs.get(pair) {
                if !pair_ref.recv_fifo_ref(side).is_empty() && (requested & POLLIN) != 0 {
                    revents |= POLLIN;
                }
                if pair_ref.send_fifo_ref(side).has_space() && (requested & POLLOUT) != 0 {
                    revents |= POLLOUT;
                }
            }
            if peer_closed {
                revents |= POLLHUP;
                if (requested & POLLIN) != 0 {
                    revents |= POLLIN;
                }
            }
            revents
        }
        _ => 0,
    }
}

/// Return POLL* bitmask of currently ready events for a Unix socket.
pub fn unix_poll_events(handle: SocketHandle, requested: u16) -> u16 {
    let state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return 0;
    };
    compute_revents(&state, i, requested)
}

/// Fused poll: register on wait queue THEN check readiness.
pub fn unix_poll_fused(handle: SocketHandle, requested: u16) -> (u16, bool) {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return (0, false);
    };

    let registered = SOCKET_WQS[wq_idx].enqueue_current();

    let revents = {
        let state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return (0, false);
        };
        compute_revents(&state, i, requested)
    };

    (revents, registered)
}

/// Enqueue the current task on the socket's wait queue for poll().
pub fn unix_poll_register(handle: SocketHandle) -> bool {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return false;
    };
    SOCKET_WQS[wq_idx].enqueue_current()
}

/// Remove the current task from the socket's poll wait queue.
pub fn unix_poll_unregister(handle: SocketHandle) {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return;
    };
    SOCKET_WQS[wq_idx].remove_current();
}

/// Set or clear non-blocking mode on a Unix socket.
pub fn unix_set_nonblocking(handle: SocketHandle, nonblocking: bool) -> i32 {
    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9;
    };
    state.slots[i].nonblocking = nonblocking;
    0
}

/// Return the bound path for a Unix socket, if any.
pub fn unix_get_local_path(handle: SocketHandle) -> Option<[u8; UNIX_PATH_MAX]> {
    let state = UNIX_STATE.lock();
    let i = validate_socket_handle(&state, handle)?;
    let path: &[u8] = match &state.slots[i].state {
        SlotState::Bound { path } => path.as_slice(),
        SlotState::Listening { path, .. } => path.as_slice(),
        _ => return None,
    };
    if path.is_empty() {
        return None;
    }
    let mut out = [0u8; UNIX_PATH_MAX];
    out[..path.len()].copy_from_slice(path);
    Some(out)
}

/// Return the path length of the bound address for a Unix socket.
pub fn unix_get_local_path_len(handle: SocketHandle) -> usize {
    let state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return 0;
    };
    match &state.slots[i].state {
        SlotState::Bound { path } => path.len(),
        SlotState::Listening { path, .. } => path.len(),
        _ => 0,
    }
}

/// Return the bound path of the peer for a connected Unix socket.
pub fn unix_get_peer_path(handle: SocketHandle) -> Option<([u8; UNIX_PATH_MAX], usize)> {
    let state = UNIX_STATE.lock();
    let i = validate_socket_handle(&state, handle)?;
    let peer = match state.slots[i].state {
        SlotState::Connected { peer, .. } => peer,
        _ => return None,
    };
    let peer_idx = peer.raw_slot();
    if peer_idx >= MAX_UNIX_SOCKETS || state.slots[peer_idx].generation != peer.generation() {
        return None;
    }
    let peer_path: &[u8] = match &state.slots[peer_idx].state {
        SlotState::Bound { path } => path.as_slice(),
        SlotState::Listening { path, .. } => path.as_slice(),
        _ => return Some(([0u8; UNIX_PATH_MAX], 0)),
    };
    let len = peer_path.len();
    let mut out = [0u8; UNIX_PATH_MAX];
    if len > 0 {
        out[..len].copy_from_slice(peer_path);
    }
    Some((out, len))
}
