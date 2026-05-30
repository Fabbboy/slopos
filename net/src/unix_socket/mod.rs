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

use slopos_abi::event::{KernelEvent, UnixSocketSlot};
use slopos_abi::syscall::{POLLHUP, POLLIN, POLLOUT};
use slopos_ostd::sync::{BUS, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBTreeMap, KVec, KVecDeque};

use pair::{InFlightFd, PairSide, PairTable};
use slot::{MAX_BACKLOG, SlotState, UNIX_PATH_MAX, UnixSlot};

pub use buffer::UNIX_BUF_SIZE;
pub use handle::SocketHandle;

/// Maximum number of concurrent AF_UNIX sockets.
pub use slopos_abi::event::MAX_UNIX_SOCKETS;

/// The readiness event for a Unix socket slot. Recv- and send-blockers share
/// one queue per socket, so a single publish wakes both — preserving the
/// pre-migration `SOCKET_WQS[idx].wake_all()` semantics.
#[inline]
fn unix_ev(slot: usize) -> KernelEvent {
    KernelEvent::UnixSocket {
        sock: UnixSocketSlot(slot as u32),
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

struct UnixSocketState {
    slots: [UnixSlot; MAX_UNIX_SOCKETS],
    pairs: PairTable,
}

// SAFETY: UnixSocketState is only accessed through the UNIX_STATE SpinLock.

impl UnixSocketState {
    const fn new() -> Self {
        Self {
            slots: [const { UnixSlot::new() }; MAX_UNIX_SOCKETS],
            pairs: PairTable::new(),
        }
    }
}

static UNIX_STATE: SpinLock<UnixSocketState> =
    SpinLock::new(UnixSocketState::new(), LOCK_LEVEL_REGISTRY);

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

        BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
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
    BUS.publish(unix_ev(listener_idx));

    0
}

/// Send data on a connected AF_UNIX socket (no SCM_RIGHTS ancillary).
///
/// Thin wrapper around [`unix_sendmsg`] — the `write(2)` syscall path
/// and every caller that never carries fds reach the same atomic
/// data+fd publish primitive as `sendmsg(2)`. Keeping a single
/// implementation means there is exactly one place where the data
/// FIFO + ancillary queue + peer-wake ordering invariants live.
pub fn unix_send(handle: SocketHandle, data: &[u8]) -> i32 {
    unix_sendmsg(handle, data, &mut [], 0)
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
                    BUS.publish(unix_ev(peer));
                }
                return n as i32;
            }
            Err(true) => return -11, // EAGAIN
            Err(false) => {
                BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
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

/// Send data on a connected AF_UNIX socket with optional in-flight fds (SCM_RIGHTS).
///
/// # Atomicity contract
///
/// Data bytes and ancillary fds become visible to the peer **together**:
/// the peer's next `unix_recvmsg` either sees both or neither. This
/// matches Linux (`unix_scm_to_skb` attaches `scm->fp` to the skb
/// *before* `__skb_queue_tail` and *before* `sk_data_ready`), FreeBSD
/// (`unp_internalize` chains the `MT_CONTROL` mbuf into the data mbuf
/// chain before `sbappendaddr_locked`), and Asterinas (a single
/// `RangedAuxiliaryData` entry stamped with the data's byte range,
/// all committed under one mutex). Without this guarantee, the peer
/// can observe data bytes whose companion fds are still in transit
/// and a Wayland-style decoder ends up with `buffer_fd: None` for
/// messages that require an SCM_RIGHTS fd (e.g. `SurfaceAttach`).
///
/// The pre-refactor implementation took `UNIX_STATE` in two disjoint
/// critical sections (data write — unlock — wake — re-lock — fd push
/// — unlock — wake). The lazy-wake scheduler made that benign; the
/// preempt-on-enqueue path in `sched::scheduler::schedule_task`
/// (Phase 1.2) makes the peer drain the data FIFO between the two
/// critical sections.
///
/// # Single-critical-section shape
///
/// 1. Lock `UNIX_STATE`, validate the slot, resolve `(pair, side, peer)`.
/// 2. Capacity-check both publish targets:
///    - data FIFO: any space if `data` is non-empty — partial writes
///      are valid, mirroring Linux's per-skb behaviour where the
///      first skb takes as much data as fits + all fds and the
///      caller retries the remainder.
///    - ancillary queue: `current_len + fd_count <= MAX_INFLIGHT_FDS`
///      so a multi-fd send never publishes a partial set.
/// 3. Push all fds first, then write data — fds-before-data ordering
///    in the same critical section means a racing peer that reads
///    the data after unlock-then-wake always sees the companion fds
///    already queued.
/// 4. Unlock once, `wake_all` once.
///
/// # Failure modes (no partial publish)
///
/// - Ancillary queue would overflow → release the caller's fds,
///   return `ENOMEM`. The peer observes neither data nor fds.
/// - Data FIFO is full, non-blocking → release fds, return `EAGAIN`.
/// - Data FIFO is full, blocking → wait on the sender's wait queue;
///   fds stay in the caller's `inflight_fds` array (not yet
///   committed) and the next iteration runs the full capacity check
///   + atomic publish again.
/// - Slot reused / peer closed → release fds, return
///   `EBADF`/`ENOTCONN`/`EPIPE`.
pub fn unix_sendmsg(
    handle: SocketHandle,
    data: &[u8],
    inflight_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    fd_count: usize,
) -> i32 {
    let Some(wq_idx) = handle.slot_for_wq() else {
        release_inflight(inflight_fds, fd_count);
        return -9; // EBADF
    };
    let slot_gen = handle.generation();

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                drop(state);
                release_inflight(inflight_fds, fd_count);
                return -107; // ENOTCONN
            };
            let nonblocking = state.slots[i].nonblocking;

            let (pair_handle, side, peer_idx) = match state.slots[i].state {
                SlotState::Connected {
                    pair,
                    side,
                    peer,
                    peer_closed,
                } => {
                    if peer_closed {
                        drop(state);
                        release_inflight(inflight_fds, fd_count);
                        return -32; // EPIPE
                    }
                    (pair, side, peer.raw_slot())
                }
                _ => {
                    drop(state);
                    release_inflight(inflight_fds, fd_count);
                    return -107;
                }
            };

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => {
                    drop(state);
                    release_inflight(inflight_fds, fd_count);
                    return -32; // EPIPE — pair already freed
                }
            };

            // Ancillary capacity check first: all-or-nothing fds.
            if fd_count > 0 && pair.send_anc(side).len() + fd_count > pair::MAX_INFLIGHT_FDS {
                drop(state);
                release_inflight(inflight_fds, fd_count);
                return -12; // ENOMEM
            }

            // Data capacity check. Empty data trivially fits;
            // otherwise any free byte in the FIFO is enough for a
            // partial write (Linux per-skb semantics).
            let data_has_space = data.is_empty() || pair.send_fifo(side).has_space();
            if !data_has_space {
                Err(nonblocking)
            } else {
                // Commit. Push fds first so a peer that races us
                // post-unlock always sees the companion fds before
                // any data byte. We hold `UNIX_STATE` across both
                // operations, so the peer cannot observe an
                // intermediate state — but the in-CS ordering also
                // closes the lock-free `unix_poll_events` window
                // where readers snapshot `recv_fifo_ref().is_empty()`
                // outside the state lock.
                if fd_count > 0 {
                    let anc = pair.send_anc(side);
                    for j in 0..fd_count {
                        let (h, ops) = inflight_fds[j];
                        let pushed = anc.push(InFlightFd { handle: h, ops });
                        // Capacity was checked above; pushes must succeed.
                        debug_assert!(pushed, "anc.push must succeed after capacity check");
                        inflight_fds[j] = (0, inflight_fds[j].1);
                    }
                }
                let n = if data.is_empty() {
                    0
                } else {
                    pair.send_fifo(side).write(data) as i32
                };
                Ok((n, peer_idx, fd_count))
            }
        };

        match result {
            Ok((n, peer, committed_fds)) => {
                if (n > 0 || committed_fds > 0) && peer < MAX_UNIX_SOCKETS {
                    BUS.publish(unix_ev(peer));
                }
                return n;
            }
            Err(true) => {
                // Non-blocking with no data space; capacity check
                // failed in the same iteration so no fds were
                // committed.
                release_inflight(inflight_fds, fd_count);
                return -11; // EAGAIN
            }
            Err(false) => {
                // Block until peer drains, slot reuses, or peer
                // closes. fds remain in the caller's array — the
                // next iteration runs the full capacity check +
                // atomic publish again. Hand the fds to the per-task
                // custodian for the duration of the park so a kill
                // while blocked cannot leak them (see `inflight_park`).
                let task_id = slopos_kernel_services::driver_runtime::current_task_id();
                inflight_park(task_id, inflight_fds, fd_count);
                BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
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
                inflight_unpark(task_id);
            }
        }
    }
}

/// Release every still-owned fd in `inflight_fds[..fd_count]`.
/// Called on every error-return path of [`unix_sendmsg`] so a
/// failed send never leaks the fd refs the kernel duplicated from
/// the sender's process fd table in the sendmsg syscall handler.
#[inline]
fn release_inflight(
    inflight_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    fd_count: usize,
) {
    for j in 0..fd_count {
        if inflight_fds[j].0 != 0 {
            inflight_fds[j].1.release(inflight_fds[j].0);
            inflight_fds[j] = (0, inflight_fds[j].1);
        }
    }
}

// ── In-flight SCM_RIGHTS fd custody across a blocking send ──────────────────
//
// `unix_sendmsg` owns fd references (duplicated from the sender's fd table by
// the sendmsg syscall handler) until it either commits them to the peer or
// releases them via `release_inflight`. On the blocking path it holds them
// across a `wait_event` park. SlopOS tears a blocked task down asynchronously
// — its `schedule()` never returns, so neither the loop's `release_inflight`
// nor any stack cleanup runs — which would leak those fd refs if the sender is
// SIGKILL'd while parked.
//
// To stay leak-free we hand the fds to this per-task custodian for *exactly*
// the duration of the park (`inflight_park` before the wait, `inflight_unpark`
// after it wakes). Only a *blocked* task can be async-abandoned; a running
// task killed between operations still completes its current stack path
// (releasing or committing normally) before exiting at the syscall boundary.
// So covering the park window is sufficient and complete. If the sender dies
// while parked, `unix_inflight_cleanup_task` (a task-termination hook) releases
// the custodied refs — mirroring the poll/futex/waitpid teardown hooks.
//
// `inflight_park` only *copies* the (handle, ops) pairs (it does not zero the
// caller's array): on a normal wake `inflight_unpark` drops the custody copy
// without releasing, leaving the array to drive the usual commit/release path;
// on kill the array is abandoned and the hook releases via the custody copy.
// The two never both release — a killed task never runs `inflight_unpark`/the
// array path, and a surviving task empties the custody on `inflight_unpark`.
type InflightFd = (usize, &'static dyn slopos_abi::file_ops::FileOps);

static SENDMSG_INFLIGHT: SpinLock<KBTreeMap<u32, KVec<InflightFd>>> =
    SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE);

fn inflight_park(task_id: u32, inflight_fds: &[InflightFd], fd_count: usize) {
    if task_id == 0 || fd_count == 0 {
        return;
    }
    let Ok(mut held) = KVec::with_capacity(fd_count) else {
        return;
    };
    for &fd in &inflight_fds[..fd_count] {
        if fd.0 != 0 {
            let _ = held.push(fd);
        }
    }
    if held.is_empty() {
        return;
    }
    let mut map = SENDMSG_INFLIGHT.lock();
    if let Some(stale) = map.insert(task_id, held) {
        // A prior un-released custody for this task would be a bug (a task can
        // only be parked in one send at a time); release it rather than leak.
        for &fd in stale.iter() {
            if fd.0 != 0 {
                fd.1.release(fd.0);
            }
        }
    }
}

fn inflight_unpark(task_id: u32) {
    if task_id == 0 {
        return;
    }
    // Drop the custody copy WITHOUT releasing: the caller's array still owns
    // these refs on the normal wake path and will commit or release them.
    let _ = SENDMSG_INFLIGHT.lock().remove(&task_id);
}

/// Task-termination hook: release any in-flight SCM_RIGHTS fd refs a task was
/// holding across a blocking `unix_sendmsg` when it died. Registered via
/// `register_task_resource_cleanup_hook` at boot. No-op for tasks holding none.
pub fn unix_inflight_cleanup_task(task_id: u32) {
    let held = { SENDMSG_INFLIGHT.lock().remove(&task_id) };
    if let Some(held) = held {
        for &fd in held.iter() {
            if fd.0 != 0 {
                fd.1.release(fd.0);
            }
        }
    }
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
            BUS.publish(unix_ev(peer));
        }
    }
    for k in 0..backlog_wake_count {
        let a_idx = backlog_a_peers[k];
        if a_idx < MAX_UNIX_SOCKETS {
            BUS.publish(unix_ev(a_idx));
        }
    }
    if was_listener {
        BUS.publish(unix_ev(wq_idx));
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

    let registered = BUS.subscribe_current(unix_ev(wq_idx));

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
    BUS.subscribe_current(unix_ev(wq_idx))
}

/// Remove the current task from the socket's poll wait queue.
pub fn unix_poll_unregister(handle: SocketHandle) {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return;
    };
    BUS.unsubscribe_current(unix_ev(wq_idx));
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

/// Read a Unix socket's stored non-blocking flag. Returns `None` for a
/// stale handle. Used by the SlopRing `OP_ACCEPT` glue to restore the
/// listener's original mode after a forced-nonblocking probe.
pub fn unix_is_nonblocking(handle: SocketHandle) -> Option<bool> {
    let state = UNIX_STATE.lock();
    let i = validate_socket_handle(&state, handle)?;
    Some(state.slots[i].nonblocking)
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
