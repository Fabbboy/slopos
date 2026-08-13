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

use slopos_abi::Errno;
use slopos_abi::event::{KernelEvent, UnixSocketSlot};
use slopos_abi::syscall::{POLLHUP, POLLIN, POLLOUT};
use slopos_ostd::handle::HandleTable;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{BUS, LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::{KVec, KVecDeque};

use pair::{PairSide, PairTable};
use slopos_fs::FileRef;
use slopos_ostd::process::AccountId;
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
    slots: HandleTable<UnixSlot>,
    pairs: PairTable,
}

// SAFETY: UnixSocketState is only accessed through the UNIX_STATE SpinLock.

impl UnixSocketState {
    const fn new() -> Self {
        Self {
            slots: HandleTable::new(),
            pairs: PairTable::new(),
        }
    }
}

static UNIX_STATE: SpinLock<UnixSocketState> = SpinLock::new(
    UnixSocketState::new(),
    lock_class!("UNIX_STATE", LOCK_LEVEL_REGISTRY),
);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate a new AF_UNIX socket slot. Returns a [`SocketHandle`], or `None`
/// when the system-wide cap is reached.
pub fn unix_create() -> Option<SocketHandle> {
    let mut state = UNIX_STATE.lock();
    if state.slots.len() >= MAX_UNIX_SOCKETS {
        return None;
    }
    state
        .slots
        .insert(UnixSlot::created())
        .ok()
        .map(SocketHandle::from_handle)
}

/// Bind a socket to an abstract namespace path.
pub fn unix_bind(handle: SocketHandle, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return Errno::EINVAL.raw();
    }

    // Pre-allocate the path buffer outside the lock.
    let mut owned_path = match KVec::<u8>::with_capacity(path.len()) {
        Ok(v) => v,
        Err(_) => return Errno::ENOMEM.raw(),
    };
    if owned_path.extend_from_slice(path).is_err() {
        return Errno::ENOMEM.raw();
    }

    let mut state = UNIX_STATE.lock();
    let h = handle.handle();
    match state.slots.get(h) {
        Ok(slot) if matches!(slot.state, SlotState::Created) => {}
        Ok(_) => return Errno::EINVAL.raw(),
        Err(_) => return Errno::EBADF.raw(),
    }

    // Check for duplicate path among other sockets.
    for (other_h, other) in state.slots.iter() {
        if other_h == h {
            continue;
        }
        let other_path: &[u8] = match &other.state {
            SlotState::Bound { path } => path.as_slice(),
            SlotState::Listening { path, .. } => path.as_slice(),
            _ => continue,
        };
        if other_path == path {
            return Errno::EADDRINUSE.raw();
        }
    }

    if let Ok(slot) = state.slots.get_mut(h) {
        slot.state = SlotState::Bound { path: owned_path };
    }
    0
}

/// Mark a bound socket as listening.
pub fn unix_listen(handle: SocketHandle, _backlog: u32) -> i32 {
    // Pre-allocate the backlog deque outside the lock.
    let backlog: KVecDeque<SocketHandle> = match KVecDeque::with_capacity(MAX_BACKLOG) {
        Ok(d) => d,
        Err(_) => return Errno::ENOMEM.raw(),
    };

    let mut state = UNIX_STATE.lock();
    let Ok(slot) = state.slots.get_mut(handle.handle()) else {
        return Errno::EBADF.raw();
    };

    // Take ownership of the existing path by swapping in the neutral
    // placeholder.
    let path = match core::mem::replace(&mut slot.state, SlotState::Created) {
        SlotState::Bound { path } => path,
        other => {
            // Not Bound — restore and reject.
            slot.state = other;
            return Errno::EINVAL.raw();
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
        return Err(Errno::EBADF.raw());
    };

    loop {
        let (nonblocking, got) = {
            let mut state = UNIX_STATE.lock();
            let Ok(slot) = state.slots.get_mut(handle.handle()) else {
                return Err(Errno::EBADF.raw());
            };
            let nb = slot.nonblocking;
            let accepted = match &mut slot.state {
                SlotState::Listening { backlog, .. } => backlog.pop_front(),
                _ => return Err(Errno::EINVAL.raw()),
            };
            (nb, accepted)
        };

        if let Some(accepted_handle) = got {
            return Ok(accepted_handle);
        }

        if nonblocking {
            return Err(Errno::EAGAIN.raw());
        }

        let waited = BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
            let state = UNIX_STATE.lock();
            match state.slots.get(handle.handle()) {
                Err(_) => true, // slot reused/gone — bail out
                Ok(slot) => match &slot.state {
                    SlotState::Listening { backlog, .. } => !backlog.is_empty(),
                    _ => true, // state changed unexpectedly — bail out
                },
            }
        });
        if waited.is_err() {
            return Err(Errno::EINTR.raw());
        }
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
        return Errno::EINVAL.raw();
    }

    let mut state = UNIX_STATE.lock();
    let h_a = handle.handle();

    // Caller must be Created or Bound (not Connected, Listening, etc.).
    match state.slots.get(h_a) {
        Ok(slot) => match slot.state {
            SlotState::Created | SlotState::Bound { .. } => {}
            SlotState::Connected { .. } => return Errno::EISCONN.raw(),
            SlotState::Listening { .. } => return Errno::EOPNOTSUPP.raw(),
        },
        Err(_) => return Errno::EBADF.raw(),
    }

    // Find the listener and verify backlog has space.
    let mut listener = None;
    for (lh, slot) in state.slots.iter() {
        if let SlotState::Listening {
            path: listener_path,
            backlog,
        } = &slot.state
        {
            if listener_path.as_slice() == path {
                if backlog.len() >= MAX_BACKLOG {
                    return Errno::EAGAIN.raw();
                }
                listener = Some(lh);
                break;
            }
        }
    }
    let Some(h_listener) = listener else {
        return Errno::ECONNREFUSED.raw();
    };

    // Reserve room for the accepted side (side B) before committing.
    if state.slots.len() >= MAX_UNIX_SOCKETS {
        return Errno::ENFILE.raw();
    }

    // The *connecting client's* syscall allocates side B's slot, the pair
    // entry and both 16 KiB FIFOs — storage the server will use but the
    // client pays for. That is deliberate and load-bearing, not incidental:
    // moving these allocations to `accept` would flip 32 clients' worth of
    // kernel storage onto the compositor's budget and make a connect flood
    // exhaust the server rather than the caller. Keep the allocation here.
    //
    // Allocate a pair entry; this is where the 16 KiB×2 FIFO heap allocations happen.
    let pair_handle = match state.pairs.allocate() {
        Ok(Some(ph)) => ph,
        Ok(None) => return Errno::ENFILE.raw(),
        Err(_) => return Errno::ENOMEM.raw(),
    };

    let Ok(h_b) = state.slots.insert(UnixSlot::created()) else {
        // Capacity was checked above; if the insert nonetheless fails, drop
        // both endpoint refs of the just-allocated pair rather than leak it —
        // `ConnectionPair::new` starts at a refcount of two. Its queues are
        // still empty, so the in-lock drop is inert.
        drop(state.pairs.release(pair_handle));
        drop(state.pairs.release(pair_handle));
        return Errno::ENFILE.raw();
    };
    let a_handle = SocketHandle::from_handle(h_a);
    let b_handle = SocketHandle::from_handle(h_b);

    // Set up caller (side A).
    if let Ok(slot) = state.slots.get_mut(h_a) {
        slot.state = SlotState::Connected {
            pair: pair_handle,
            side: PairSide::A,
            peer: b_handle,
            peer_closed: false,
        };
    }

    // Set up accepted side (side B).
    if let Ok(slot) = state.slots.get_mut(h_b) {
        slot.state = SlotState::Connected {
            pair: pair_handle,
            side: PairSide::B,
            peer: a_handle,
            peer_closed: false,
        };
    }

    // Enqueue B in the listener's backlog.
    if let Ok(slot) = state.slots.get_mut(h_listener) {
        if let SlotState::Listening { backlog, .. } = &mut slot.state {
            // Pre-reserved at unix_listen, so push_back never realloc'd.
            backlog
                .push_back(b_handle)
                .expect("backlog pre-reserved at listen");
        }
    }

    let listener_idx = h_listener.slot() as usize;
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
    let mut no_files: KVec<FileRef> = KVec::new();
    // No fds, so no custody is ever charged and the account is never read.
    unix_sendmsg(handle, data, &mut no_files, AccountId::NONE)
}

/// Receive data from a connected AF_UNIX socket.
pub fn unix_recv(handle: SocketHandle, buf: &mut [u8]) -> i32 {
    if buf.is_empty() {
        return 0;
    }
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Errno::EBADF.raw();
    };

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let (nonblocking, pair_handle, side, peer_idx, peer_closed) =
                match state.slots.get(handle.handle()) {
                    Ok(slot) => match slot.state {
                        SlotState::Connected {
                            pair,
                            side,
                            peer,
                            peer_closed,
                        } => (slot.nonblocking, pair, side, peer.slot(), peer_closed),
                        _ => return Errno::ENOTCONN.raw(),
                    },
                    Err(_) => return Errno::ENOTCONN.raw(),
                };

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => {
                    return if peer_closed {
                        0
                    } else {
                        Errno::ENOTCONN.raw()
                    };
                }
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
            Err(true) => return Errno::EAGAIN.raw(),
            Err(false) => {
                let waited = BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
                    let state = UNIX_STATE.lock();
                    match state.slots.get(handle.handle()) {
                        Err(_) => true, // slot reused/gone — bail out
                        Ok(slot) => match slot.state {
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
                        },
                    }
                });
                if waited.is_err() {
                    return Errno::EINTR.raw();
                }
            }
        }
    }
}

/// Single-direct-copy [`unix_recv`]: drain the recv FIFO straight into the
/// pinned user pages (via `writer`) with one volatile copy per byte — no kernel
/// scratch. Mirrors `unix_recv`'s EOF / blocking / wake semantics; the byte
/// count is what the writer accepted.
pub fn unix_recv_into(handle: SocketHandle, writer: &mut slopos_ostd::mm::VmWriter<'_>) -> i32 {
    if !writer.has_remain() {
        return 0;
    }
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Errno::EBADF.raw();
    };

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let (nonblocking, pair_handle, side, peer_idx, peer_closed) =
                match state.slots.get(handle.handle()) {
                    Ok(slot) => match slot.state {
                        SlotState::Connected {
                            pair,
                            side,
                            peer,
                            peer_closed,
                        } => (slot.nonblocking, pair, side, peer.slot(), peer_closed),
                        _ => return Errno::ENOTCONN.raw(),
                    },
                    Err(_) => return Errno::ENOTCONN.raw(),
                };

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => {
                    return if peer_closed {
                        0
                    } else {
                        Errno::ENOTCONN.raw()
                    };
                }
            };
            let rbuf = pair.recv_fifo(side);
            if !rbuf.is_empty() {
                Ok((rbuf.read_into(writer), peer_idx))
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
            Err(true) => return Errno::EAGAIN.raw(),
            Err(false) => {
                let waited = BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
                    let state = UNIX_STATE.lock();
                    match state.slots.get(handle.handle()) {
                        Err(_) => true, // slot reused/gone — bail out
                        Ok(slot) => match slot.state {
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
                        },
                    }
                });
                if waited.is_err() {
                    return Errno::EINTR.raw();
                }
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
/// Send data plus optional in-flight files (SCM_RIGHTS).
///
/// `files` are owned aliases minted by the syscall handler
/// (`fileio_clone_file_ref`). On commit they move into the peer's
/// ancillary queue (all-or-nothing); on any error return they stay in
/// the caller's `KVec` and close when the caller drops it — no net lock
/// is held at that point, so the (possibly recursive) file teardown is
/// safe.
pub fn unix_sendmsg(
    handle: SocketHandle,
    data: &[u8],
    files: &mut KVec<FileRef>,
    sender_account: AccountId,
) -> i32 {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Errno::EBADF.raw();
    };

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let conn = match state.slots.get(handle.handle()) {
                Ok(slot) => match slot.state {
                    SlotState::Connected {
                        pair,
                        side,
                        peer,
                        peer_closed,
                    } => Ok((slot.nonblocking, pair, side, peer.slot(), peer_closed)),
                    _ => Err(Errno::ENOTCONN.raw()),
                },
                Err(_) => Err(Errno::ENOTCONN.raw()),
            };
            let (nonblocking, pair_handle, side, peer_idx, peer_closed) = match conn {
                Ok(t) => t,
                Err(e) => return e,
            };
            if peer_closed {
                return Errno::EPIPE.raw();
            }

            let pair = match state.pairs.get_mut(pair_handle) {
                Some(p) => p,
                None => return Errno::EPIPE.raw(),
            };

            // Ancillary capacity check first: all-or-nothing fds.
            let fd_count = files.len();
            if fd_count > 0 && pair.send_anc(side).len() + fd_count > pair::MAX_INFLIGHT_FDS {
                return Errno::ENOMEM.raw();
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
                    for file in files.drain(..) {
                        // Capacity was checked above and the queue's storage
                        // is pre-reserved, so the only refusal left is the
                        // sender's custody ceiling. A refused file drops here,
                        // closing that alias, rather than travelling uncharged.
                        if let Err(refused) = anc.push(file, sender_account) {
                            drop(refused);
                        }
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
                // committed — they stay with the caller.
                return Errno::EAGAIN.raw();
            }
            Err(false) => {
                // Block until peer drains, slot reuses, or peer closes. The
                // frame keeps the files across the park: an abort returns
                // through here, and `files` belongs to the caller either way.
                let waited = BUS.subscribe(unix_ev(wq_idx)).wait_event(|| {
                    let state = UNIX_STATE.lock();
                    match state.slots.get(handle.handle()) {
                        Err(_) => true, // slot reused/gone — bail out
                        Ok(slot) => match slot.state {
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
                        },
                    }
                });
                if waited.is_err() {
                    return Errno::EINTR.raw();
                }
            }
        }
    }
}

/// Single-direct-copy `unix_send` (no fds): append the data pulled straight
/// from the pinned user pages (via `reader`) into the peer FIFO with one
/// volatile copy per byte — no kernel scratch. This is the non-blocking
/// data-only subset of [`unix_sendmsg`] that the SlopRing fixed-buffer send
/// path uses (it forces the socket non-blocking, so a full FIFO returns
/// `-EAGAIN` rather than parking). Returns bytes written, `-EAGAIN`, or an
/// errno.
pub fn unix_send_from(handle: SocketHandle, reader: &mut slopos_ostd::mm::VmReader<'_>) -> i32 {
    let (n, peer_idx) = {
        let mut state = UNIX_STATE.lock();
        let (pair_handle, side, peer_idx) = match state.slots.get(handle.handle()) {
            Ok(slot) => match slot.state {
                SlotState::Connected {
                    pair,
                    side,
                    peer,
                    peer_closed,
                } => {
                    if peer_closed {
                        return Errno::EPIPE.raw();
                    }
                    (pair, side, peer.slot())
                }
                _ => return Errno::ENOTCONN.raw(),
            },
            Err(_) => return Errno::ENOTCONN.raw(),
        };

        let pair = match state.pairs.get_mut(pair_handle) {
            Some(p) => p,
            None => return Errno::EPIPE.raw(),
        };

        let empty = !reader.has_remain();
        let data_has_space = empty || pair.send_fifo(side).has_space();
        if !data_has_space {
            return Errno::EAGAIN.raw();
        }
        let n = if empty {
            0
        } else {
            pair.send_fifo(side).write_from(reader) as i32
        };
        (n, peer_idx)
    };

    if n > 0 && peer_idx < MAX_UNIX_SOCKETS {
        BUS.publish(unix_ev(peer_idx));
    }
    n
}

/// Drain this side's pending SCM_RIGHTS files: up to `max_fds` move into
/// `out_files`; any excess is carried out of the state lock and dropped
/// here (closing those aliases). Returns the number delivered.
fn drain_ancillary(handle: SocketHandle, out_files: &mut KVec<FileRef>, max_fds: usize) -> usize {
    // Pure move out of the locked region: nothing is dropped while the
    // state lock is held.
    let mut drained = {
        let mut state = UNIX_STATE.lock();
        let conn = match state.slots.get(handle.handle()) {
            Ok(slot) => match slot.state {
                SlotState::Connected { pair, side, .. } => Some((pair, side)),
                _ => None,
            },
            Err(_) => None,
        };
        match conn {
            Some((pair, side)) => match state.pairs.get_mut(pair) {
                Some(pair_ref) => pair_ref.recv_anc(side).drain(),
                None => KVec::new(),
            },
            None => KVec::new(),
        }
    };

    let mut received = 0usize;
    for entry in drained.drain(..) {
        // The custody charge drops with the entry as the file moves out, so
        // the sender's in-flight count falls exactly when the reference stops
        // being in flight — whether the receiver takes it or the cap drops it.
        if received < max_fds && out_files.push(entry.file).is_ok() {
            received += 1;
        }
        // Beyond the cap (or on push failure) the file drops here,
        // closing that alias — matching the overflow policy of the old
        // raw-reference queue.
    }
    received
}

/// Receive data from a connected AF_UNIX socket, with optional in-flight
/// files. Delivered files are appended to `out_files` (the receive side
/// installs them into the destination fd table).
pub fn unix_recvmsg(
    handle: SocketHandle,
    buf: &mut [u8],
    out_files: &mut KVec<FileRef>,
    max_fds: usize,
) -> (i32, usize) {
    let bytes_read = unix_recv(handle, buf);
    let received_fds = drain_ancillary(handle, out_files, max_fds);
    (bytes_read, received_fds)
}

/// Single-direct-copy [`unix_recvmsg`]: the data is drained straight into the
/// pinned user pages (via `writer`) by [`unix_recv_into`] with no kernel
/// scratch; any SCM_RIGHTS files are drained into `out_files` exactly as
/// `unix_recvmsg` does. Returns `(bytes_read, n_fds)`.
pub fn unix_recvmsg_into(
    handle: SocketHandle,
    writer: &mut slopos_ostd::mm::VmWriter<'_>,
    out_files: &mut KVec<FileRef>,
    max_fds: usize,
) -> (i32, usize) {
    let bytes_read = unix_recv_into(handle, writer);
    let received_fds = drain_ancillary(handle, out_files, max_fds);
    (bytes_read, received_fds)
}

/// Tear down a closing listener's pending backlog: each entry is a
/// side-B slot created by `unix_connect()` and never accepted. Marks
/// every side-A peer closed, records their slots for post-lock wakes,
/// and detaches freed pairs into `freed_pairs`. Returns the wake count.
/// Split out of [`unix_close`] to keep its stack frame under the kernel
/// gate.
#[inline(never)]
fn close_listener_backlog(
    slots: &mut HandleTable<UnixSlot>,
    pairs: &mut PairTable,
    backlog: &KVecDeque<SocketHandle>,
    backlog_a_peers: &mut [usize; MAX_BACKLOG],
    freed_pairs: &mut KVec<pair::ConnectionPair>,
) -> usize {
    let mut wake_count = 0usize;
    for h in backlog.iter().copied() {
        let Ok(b_old) = slots.remove(h.handle()) else {
            continue;
        };
        if let SlotState::Connected {
            pair: b_pair,
            peer: b_peer,
            ..
        } = b_old.state
        {
            if let Ok(a_slot) = slots.get_mut(b_peer.handle()) {
                if let SlotState::Connected {
                    peer_closed: ref mut pc,
                    ..
                } = a_slot.state
                {
                    *pc = true;
                }
                backlog_a_peers[wake_count] = b_peer.slot();
                wake_count += 1;
            }
            if let Some(freed) = pairs.release(b_pair) {
                let _ = freed_pairs.push(freed);
            }
        }
    }
    wake_count
}

/// Close an AF_UNIX socket. Wakes all waiters on the peer if connected.
///
/// For listeners, all pending backlog entries (side-B slots that were
/// created by `unix_connect()` but never `accept()`-ed) are closed
/// and their side-A peers are notified.
pub fn unix_close(handle: SocketHandle) -> i32 {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Errno::EBADF.raw();
    };

    // Wakeup targets collected under the lock; wakes happen after release.
    let mut wake_peer: Option<usize> = None;
    let mut backlog_a_peers: [usize; MAX_BACKLOG] = [usize::MAX; MAX_BACKLOG];
    let mut backlog_wake_count = 0usize;
    let mut was_listener = false;

    // Pairs freed by this close are detached under the lock and dropped
    // here, after it releases: their ancillary queues can hold in-flight
    // `FileRef`s whose teardown may recurse back into this module (a
    // socket passed over a socket). Reserved up-front so collecting the
    // freed pairs never allocates under the state lock.
    let mut freed_pairs = match KVec::with_capacity(MAX_BACKLOG + 1) {
        Ok(v) => v,
        Err(_) => return Errno::ENOMEM.raw(),
    };

    {
        let mut state = UNIX_STATE.lock();
        let UnixSocketState { slots, pairs } = &mut *state;

        // Remove the closing slot, taking ownership of its state (the table
        // bumps the slot's generation, so any leftover handle goes stale).
        let Ok(closed) = slots.remove(handle.handle()) else {
            return Errno::EBADF.raw();
        };

        match closed.state {
            SlotState::Connected { pair, peer, .. } => {
                // Mark the peer's half closed; a removed/recycled peer slot
                // resolves to a typed miss and is skipped.
                if let Ok(peer_slot) = slots.get_mut(peer.handle()) {
                    if let SlotState::Connected {
                        peer_closed: ref mut pc,
                        ..
                    } = peer_slot.state
                    {
                        *pc = true;
                    }
                }
                wake_peer = Some(peer.slot());
                if let Some(freed) = pairs.release(pair) {
                    let _ = freed_pairs.push(freed);
                }
            }
            SlotState::Listening { backlog, .. } => {
                was_listener = true;
                backlog_wake_count = close_listener_backlog(
                    slots,
                    pairs,
                    &backlog,
                    &mut backlog_a_peers,
                    &mut freed_pairs,
                );
            }
            // Created / Bound — nothing extra to release.
            _ => {}
        }
    }

    // State lock released: tear the freed pairs (and any in-flight files
    // they still carried) down now.
    drop(freed_pairs);

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
fn compute_revents(state: &UnixSocketState, handle: SocketHandle, requested: u16) -> u16 {
    let Ok(slot) = state.slots.get(handle.handle()) else {
        return 0;
    };
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
    compute_revents(&state, handle, requested)
}

/// Fused poll: register on wait queue THEN check readiness.
pub fn unix_poll_fused(handle: SocketHandle, requested: u16) -> (u16, bool) {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return (0, false);
    };

    let registered = BUS.subscribe_current(unix_ev(wq_idx));

    let revents = {
        let state = UNIX_STATE.lock();
        if !state.slots.contains(handle.handle()) {
            return (0, false);
        }
        compute_revents(&state, handle, requested)
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
    let Ok(slot) = state.slots.get_mut(handle.handle()) else {
        return Errno::EBADF.raw();
    };
    slot.nonblocking = nonblocking;
    0
}

/// Read a Unix socket's stored non-blocking flag. Returns `None` for a
/// stale handle. Used by the SlopRing `OP_ACCEPT` glue to restore the
/// listener's original mode after a forced-nonblocking probe.
pub fn unix_is_nonblocking(handle: SocketHandle) -> Option<bool> {
    let state = UNIX_STATE.lock();
    state.slots.get(handle.handle()).ok().map(|s| s.nonblocking)
}

/// Return the bound path for a Unix socket, if any.
pub fn unix_get_local_path(handle: SocketHandle) -> Option<[u8; UNIX_PATH_MAX]> {
    let state = UNIX_STATE.lock();
    let slot = state.slots.get(handle.handle()).ok()?;
    let path: &[u8] = match &slot.state {
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
    let Ok(slot) = state.slots.get(handle.handle()) else {
        return 0;
    };
    match &slot.state {
        SlotState::Bound { path } => path.len(),
        SlotState::Listening { path, .. } => path.len(),
        _ => 0,
    }
}

/// Return the bound path of the peer for a connected Unix socket.
pub fn unix_get_peer_path(handle: SocketHandle) -> Option<([u8; UNIX_PATH_MAX], usize)> {
    let state = UNIX_STATE.lock();
    let peer = match state.slots.get(handle.handle()).ok()?.state {
        SlotState::Connected { peer, .. } => peer,
        _ => return None,
    };
    let peer_path: &[u8] = match &state.slots.get(peer.handle()).ok()?.state {
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
