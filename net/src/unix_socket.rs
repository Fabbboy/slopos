//! Kernel AF_UNIX (Unix domain) stream socket implementation.
//!
//! # Design
//!
//! Each socket slot can be in one of several states: Unbound, Bound,
//! Listening, Connected, or Closed.  Connected pairs use bidirectional
//! ring buffers (one per direction).  Abstract namespace binding uses a
//! kernel-internal lookup table of path bytes.
//!
//! Ring buffers are heap-allocated on demand when a connection is
//! established, and freed when the connection is closed.  This avoids
//! placing ~1 MB of static data in the kernel `.data` section which
//! would push the kernel image past Limine's mapped region.
//!
//! # Locking
//!
//! Socket slot data is protected by [`UNIX_STATE`].  Wait queues live in
//! separate statics indexed by slot, so wakers and sleepers never hold
//! `UNIX_STATE` and a wait-queue lock simultaneously (same pattern as
//! `fs/src/pipe.rs`).

extern crate alloc;

use alloc::boxed::Box;
use slopos_abi::syscall::{POLLHUP, POLLIN, POLLOUT};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY, WaitQueue};

/// Maximum number of concurrent AF_UNIX sockets.
pub const MAX_UNIX_SOCKETS: usize = 32;

/// Bits used for the slot index in the handle encoding.
const SLOT_BITS: u32 = 8;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1; // 0xFF — supports up to 256 slots

// ---------------------------------------------------------------------------
// SocketHandle — type-safe handle for AF_UNIX socket kernel objects
// ---------------------------------------------------------------------------

/// Opaque handle identifying an AF_UNIX socket slot.
///
/// Encodes a slot index and the slot's generation counter so that stale
/// handles (from a closed socket whose slot was recycled) are reliably
/// rejected. Replaces the old `UNIX_HANDLE_TAG` bit-hack.
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
    fn raw_slot(self) -> usize {
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

    pub(crate) fn generation(self) -> u32 {
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

/// Per-direction ring buffer size (16 KB).
pub const UNIX_BUF_SIZE: usize = 16384;

/// Maximum abstract namespace path length.
const UNIX_PATH_MAX: usize = 108;

/// Maximum pending connections in the accept backlog.
/// Matches Wayland's libwayland-server default of 128.
const MAX_BACKLOG: usize = 32;

/// Maximum number of in-flight file descriptors per direction (SCM_RIGHTS).
const MAX_INFLIGHT_FDS: usize = 8;

// ---------------------------------------------------------------------------
// Wait queues — one per socket slot, separate from UNIX_STATE.
//
// Unified design (Linux `sk->sk_wq` pattern): all blocking paths (recv,
// send, accept) and poll share a single queue per socket.  Spurious wakeups
// are harmless — every waiter re-checks its condition in a loop.  This
// eliminates the TOCTOU where poll registers on the wrong queue because
// the socket type changed between the state check and registration.
// ---------------------------------------------------------------------------

static SOCKET_WQS: [WaitQueue; MAX_UNIX_SOCKETS] = [const { WaitQueue::new() }; MAX_UNIX_SOCKETS];

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnixState {
    /// Freshly created, no address bound.
    Unbound,
    /// Address bound but not yet listening.
    Bound,
    /// Listening for incoming connections.
    Listening,
    /// Part of a connected pair.
    Connected,
    /// Closed / released.
    Closed,
}

// ---------------------------------------------------------------------------
// Ring buffer (one per direction within a connected pair)
// ---------------------------------------------------------------------------

struct RingBuf {
    /// Heap-allocated buffer, created on demand when a connection is
    /// established.  `None` means no buffer has been allocated yet.
    buf: Option<Box<[u8; UNIX_BUF_SIZE]>>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
}

impl RingBuf {
    const fn new() -> Self {
        Self {
            buf: None,
            read_pos: 0,
            write_pos: 0,
            len: 0,
        }
    }

    /// Install a pre-allocated buffer and reset cursors.
    fn install(&mut self, buf: Box<[u8; UNIX_BUF_SIZE]>) {
        self.buf = Some(buf);
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    /// Release the backing buffer and reset cursors.
    fn release(&mut self) {
        self.buf = None;
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    fn read_into(&mut self, out: &mut [u8]) -> usize {
        let buf = match self.buf.as_ref() {
            Some(b) => &**b,
            None => return 0,
        };
        let mut copied = 0usize;
        while copied < out.len() && self.len > 0 {
            out[copied] = buf[self.read_pos];
            self.read_pos = (self.read_pos + 1) % UNIX_BUF_SIZE;
            self.len -= 1;
            copied += 1;
        }
        copied
    }

    fn write_from(&mut self, input: &[u8]) -> usize {
        let buf = match self.buf.as_mut() {
            Some(b) => &mut **b,
            None => return 0,
        };
        let mut written = 0usize;
        while written < input.len() && self.len < UNIX_BUF_SIZE {
            buf[self.write_pos] = input[written];
            self.write_pos = (self.write_pos + 1) % UNIX_BUF_SIZE;
            self.len += 1;
            written += 1;
        }
        written
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn has_space(&self) -> bool {
        self.buf.is_some() && self.len < UNIX_BUF_SIZE
    }
}

// ---------------------------------------------------------------------------
// In-flight fd queue for SCM_RIGHTS (sendmsg/recvmsg)
// ---------------------------------------------------------------------------

/// A file descriptor reference in transit through a Unix socket.
struct InFlightFd {
    handle: usize,
    ops: &'static dyn slopos_abi::file_ops::FileOps,
}

/// Per-direction queue of in-flight fds (SCM_RIGHTS side-channel).
///
/// Fds are pushed by sendmsg and popped by recvmsg.  On socket close,
/// any unclaimed fds are released (ops.release) to avoid leaks.
struct AncillaryQueue {
    entries: [Option<InFlightFd>; MAX_INFLIGHT_FDS],
    /// Number of valid entries (at indices 0..count).
    count: u8,
}

impl AncillaryQueue {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_INFLIGHT_FDS],
            count: 0,
        }
    }

    fn push(&mut self, fd: InFlightFd) -> bool {
        if (self.count as usize) < MAX_INFLIGHT_FDS {
            self.entries[self.count as usize] = Some(fd);
            self.count += 1;
            true
        } else {
            false
        }
    }

    /// Drain all entries, returning them in a fixed-size array.
    fn drain(&mut self) -> ([Option<InFlightFd>; MAX_INFLIGHT_FDS], u8) {
        let mut out: [Option<InFlightFd>; MAX_INFLIGHT_FDS] = [const { None }; MAX_INFLIGHT_FDS];
        let n = self.count;
        for i in 0..n as usize {
            out[i] = self.entries[i].take();
        }
        self.count = 0;
        (out, n)
    }

    /// Release all unclaimed fds (called on socket close).
    fn release_all(&mut self) {
        for i in 0..self.count as usize {
            if let Some(fd) = self.entries[i].take() {
                fd.ops.release(fd.handle);
            }
        }
        self.count = 0;
    }
}

// ---------------------------------------------------------------------------
// Per-slot state
// ---------------------------------------------------------------------------

/// Which side of a connected pair this slot represents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PairSide {
    A,
    B,
}

struct UnixSlot {
    valid: bool,
    state: UnixState,

    // --- Addressing (Bound / Listening) ---
    path: [u8; UNIX_PATH_MAX],
    path_len: u8,

    // --- Listening ---
    backlog: [u32; MAX_BACKLOG],
    backlog_len: u8,

    // --- Connected pair ---
    /// Ring buffer for data flowing A→B (side A writes, side B reads).
    buf_a_to_b: RingBuf,
    /// Ring buffer for data flowing B→A (side B writes, side A reads).
    buf_b_to_a: RingBuf,
    /// In-flight fds flowing A→B (sendmsg on A, recvmsg on B).
    anc_a_to_b: AncillaryQueue,
    /// In-flight fds flowing B→A (sendmsg on B, recvmsg on A).
    anc_b_to_a: AncillaryQueue,
    /// Which side of the pair this slot is.
    side: PairSide,
    /// Index of the peer slot (the other half of the connected pair).
    peer_idx: u32,
    /// Whether the peer has closed its end.
    peer_closed: bool,
    /// Non-blocking mode.
    nonblocking: bool,
    /// Reference count for the ring buffers.  Both buffers live on the
    /// side-A slot.  Starts at 2 when a connection is established (one
    /// for each side).  Each close decrements by 1.  Buffers are freed
    /// only when the count reaches 0, preventing use-after-free when one
    /// side closes while the other still references the data.
    buf_refcount: u8,
    /// Monotonically increasing generation counter.  Incremented on each
    /// slot reuse so that stale `peer_idx` references from wait-queue
    /// condition closures can detect that the slot was repurposed.
    generation: u32,
}

impl UnixSlot {
    const fn new() -> Self {
        Self {
            valid: false,
            state: UnixState::Unbound,
            path: [0u8; UNIX_PATH_MAX],
            path_len: 0,
            backlog: [0u32; MAX_BACKLOG],
            backlog_len: 0,
            buf_a_to_b: RingBuf::new(),
            buf_b_to_a: RingBuf::new(),
            anc_a_to_b: AncillaryQueue::new(),
            anc_b_to_a: AncillaryQueue::new(),
            side: PairSide::A,
            peer_idx: u32::MAX,
            peer_closed: false,
            nonblocking: false,
            buf_refcount: 0,
            generation: 0,
        }
    }

    /// Reset slot metadata for reuse.
    ///
    /// Ring buffers are managed exclusively by the refcount path in
    /// `unix_close()`.  This method must only be called when
    /// `buf_refcount == 0` (enforced by `unix_create()`), at which
    /// point the buffers have already been freed.
    fn reset(&mut self) {
        debug_assert!(
            self.buf_refcount == 0,
            "reset() called with live buffer references (refcount={})",
            self.buf_refcount
        );
        self.valid = false;
        self.state = UnixState::Unbound;
        self.path = [0u8; UNIX_PATH_MAX];
        self.path_len = 0;
        self.backlog = [0u32; MAX_BACKLOG];
        self.backlog_len = 0;
        // Buffers are already released (refcount == 0), safe to reinit.
        self.buf_a_to_b = RingBuf::new();
        self.buf_b_to_a = RingBuf::new();
        self.anc_a_to_b = AncillaryQueue::new();
        self.anc_b_to_a = AncillaryQueue::new();
        self.side = PairSide::A;
        self.peer_idx = u32::MAX;
        self.peer_closed = false;
        self.nonblocking = false;
        self.generation = self.generation.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

struct UnixSocketState {
    slots: [UnixSlot; MAX_UNIX_SOCKETS],
}

// SAFETY: UnixSocketState is only accessed through the UNIX_STATE IrqMutex.
unsafe impl Send for UnixSocketState {}

impl UnixSocketState {
    const fn new() -> Self {
        Self {
            slots: [const { UnixSlot::new() }; MAX_UNIX_SOCKETS],
        }
    }
}

static UNIX_STATE: IrqMutex<UnixSocketState> =
    IrqMutex::new(UnixSocketState::new(), LOCK_LEVEL_REGISTRY);

/// Validate a socket handle against the current state, returning the slot
/// index if the handle is valid and the generation matches.
fn validate_socket_handle(state: &UnixSocketState, handle: SocketHandle) -> Option<usize> {
    let i = handle.raw_slot();
    if i >= MAX_UNIX_SOCKETS {
        return None;
    }
    let slot = &state.slots[i];
    if !slot.valid || slot.generation != handle.generation() {
        return None;
    }
    Some(i)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate a new AF_UNIX socket slot. Returns a [`SocketHandle`] or `None`.
///
/// Slots with `buf_refcount > 0` are skipped even if `valid == false`:
/// the peer side still holds a live reference to the ring buffers on
/// that slot, so reusing it would corrupt the peer's data.
pub fn unix_create() -> Option<SocketHandle> {
    let mut state = UNIX_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid && slot.buf_refcount == 0 {
            slot.reset();
            slot.valid = true;
            slot.state = UnixState::Unbound;
            let handle = SocketHandle::new(idx, slot.generation);
            return Some(handle);
        }
    }
    None
}

/// Bind a socket to an abstract namespace path.
pub fn unix_bind(handle: SocketHandle, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9; // EBADF
    };
    if state.slots[i].state != UnixState::Unbound {
        return -22; // EINVAL
    }

    // Check for duplicate path.
    for (j, other) in state.slots.iter().enumerate() {
        if j == i || !other.valid {
            continue;
        }
        if other.path_len as usize == path.len()
            && other.path[..path.len()] == *path
            && matches!(other.state, UnixState::Bound | UnixState::Listening)
        {
            return -98; // EADDRINUSE
        }
    }

    let slot = &mut state.slots[i];
    slot.path[..path.len()].copy_from_slice(path);
    slot.path_len = path.len() as u8;
    slot.state = UnixState::Bound;
    0
}

/// Mark a bound socket as listening.
pub fn unix_listen(handle: SocketHandle, _backlog: u32) -> i32 {
    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9;
    };
    if state.slots[i].state != UnixState::Bound {
        return -22; // EINVAL — must be bound first
    }
    state.slots[i].state = UnixState::Listening;
    state.slots[i].backlog_len = 0;
    0
}

/// Accept a pending connection from a listening socket.
///
/// Blocks the caller until a connection arrives (unless non-blocking).
/// Returns a [`SocketHandle`] for the new connected socket, or a
/// negative errno.
pub fn unix_accept(handle: SocketHandle) -> Result<SocketHandle, i32> {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return Err(-9);
    };

    loop {
        // Try to dequeue a pending connection.
        let (nonblocking, got) = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return Err(-9);
            };
            let slot = &mut state.slots[i];
            if slot.state != UnixState::Listening {
                return Err(-22); // EINVAL
            }
            let nb = slot.nonblocking;
            if slot.backlog_len > 0 {
                let connected_idx = slot.backlog[0] as usize;
                // Shift backlog entries down.
                let bl = slot.backlog_len as usize;
                for k in 1..bl {
                    slot.backlog[k - 1] = slot.backlog[k];
                }
                slot.backlog[bl - 1] = 0;
                slot.backlog_len -= 1;
                // Build a SocketHandle for the accepted slot using its generation.
                let accepted_gen = state.slots[connected_idx].generation;
                let accepted_handle = SocketHandle::new(connected_idx, accepted_gen);
                (nb, Some(accepted_handle))
            } else {
                (nb, None)
            }
        };

        if let Some(accepted_handle) = got {
            return Ok(accepted_handle);
        }

        if nonblocking {
            return Err(-11); // EAGAIN
        }

        // Block until a connection arrives.
        SOCKET_WQS[wq_idx].wait_event(|| {
            let state = UNIX_STATE.lock();
            let slot = &state.slots[wq_idx];
            !slot.valid || slot.backlog_len > 0
        });
    }
}

/// Connect to a listening socket identified by path.
///
/// Creates a connected pair: the caller's slot becomes side A, a new slot
/// is allocated for side B (which is enqueued in the listener's backlog
/// for `accept()` to return).
pub fn unix_connect(handle: SocketHandle, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    // Pre-allocate ring buffers BEFORE acquiring the global lock.
    // This avoids blocking all socket operations during the heap allocation
    // (each buffer is UNIX_BUF_SIZE = 16 KB).
    let pre_buf_a = Box::new([0u8; UNIX_BUF_SIZE]);
    let pre_buf_b = Box::new([0u8; UNIX_BUF_SIZE]);

    let mut state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return -9;
    };
    {
        let slot = &state.slots[i];
        if slot.state == UnixState::Connected {
            return -106; // EISCONN
        }
        if slot.state == UnixState::Listening {
            return -95; // EOPNOTSUPP
        }
    }

    // Find the listener.
    let mut listener_idx = None;
    for (j, slot) in state.slots.iter().enumerate() {
        if !slot.valid || slot.state != UnixState::Listening {
            continue;
        }
        if slot.path_len as usize == path.len() && slot.path[..path.len()] == *path {
            listener_idx = Some(j);
            break;
        }
    }
    let listener_idx = match listener_idx {
        Some(li) => li,
        None => return -111, // ECONNREFUSED
    };

    // Check backlog space.
    let listener = &state.slots[listener_idx];
    if listener.backlog_len as usize >= MAX_BACKLOG {
        return -11; // EAGAIN — backlog full
    }

    // Allocate a new slot for the accepted side (side B).
    let mut b_idx = None;
    for (j, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid && slot.buf_refcount == 0 {
            slot.reset();
            slot.valid = true;
            b_idx = Some(j);
            break;
        }
    }
    let b_idx = match b_idx {
        Some(bi) => bi,
        None => return -23, // ENFILE — no free slots
    };

    // Set up caller (side A) — install pre-allocated ring buffers.
    {
        let slot = &mut state.slots[i];
        slot.buf_a_to_b.install(pre_buf_a);
        slot.buf_b_to_a.install(pre_buf_b);
        slot.state = UnixState::Connected;
        slot.side = PairSide::A;
        slot.peer_idx = b_idx as u32;
        slot.peer_closed = false;
        // Two references: one for side A, one for side B.
        slot.buf_refcount = 2;
    }

    // Set up accepted side (side B) — shares ring buffers on side A.
    let accepted = &mut state.slots[b_idx];
    accepted.state = UnixState::Connected;
    accepted.side = PairSide::B;
    accepted.peer_idx = i as u32;
    accepted.peer_closed = false;

    // Enqueue B in the listener's backlog.
    let listener = &mut state.slots[listener_idx];
    let bl = listener.backlog_len as usize;
    listener.backlog[bl] = b_idx as u32;
    listener.backlog_len += 1;

    // Drop lock before waking.
    drop(state);

    // Wake the listener's accept() call.
    SOCKET_WQS[listener_idx].wake_all();

    0
}

/// Send data on a connected AF_UNIX socket.
///
/// Returns the number of bytes written, or a negative errno.
pub fn unix_send(handle: SocketHandle, data: &[u8]) -> i32 {
    if data.is_empty() {
        return 0;
    }

    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };

    let input = data;

    // Use the generation from the handle for stale-detection in condition closures.
    let slot_gen = handle.generation();

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return -107; // ENOTCONN (stale handle)
            };
            let slot = &state.slots[i];
            if slot.state != UnixState::Connected {
                return -107; // ENOTCONN
            }
            if slot.peer_closed {
                return -32; // EPIPE
            }

            // Capture nonblocking INSIDE the validated section (fixes TOCTOU).
            let nonblocking = slot.nonblocking;

            let peer = slot.peer_idx as usize;
            let side = slot.side;

            // Ring buffers live on the side-A slot. Determine which slot has them.
            let (a_idx, _b_idx) = match side {
                PairSide::A => (i, peer),
                PairSide::B => (peer, i),
            };

            // Validate the side-A slot still belongs to this connection.
            // After a peer close + slot reallocation, the A slot may have
            // been repurposed for a different connection.
            if a_idx >= MAX_UNIX_SOCKETS || state.slots[a_idx].buf_refcount == 0 {
                return -32; // EPIPE
            }

            // Also validate peer slot generation hasn't changed.
            let peer_gen = state.slots[a_idx].generation;

            let a_slot = &mut state.slots[a_idx];
            let buf = match side {
                PairSide::A => &mut a_slot.buf_a_to_b,
                PairSide::B => &mut a_slot.buf_b_to_a,
            };

            if buf.has_space() {
                let n = buf.write_from(input);
                Ok((n, peer, peer_gen))
            } else {
                Err(nonblocking)
            }
        };

        match result {
            Ok((n, peer, _peer_gen)) => {
                // Wake the peer's recv wait queue.
                // peer was captured under the lock above — no re-acquire needed.
                if peer < MAX_UNIX_SOCKETS {
                    SOCKET_WQS[peer].wake_all();
                }
                return n as i32;
            }
            Err(true) => {
                // Non-blocking and buffer full.
                return -11; // EAGAIN
            }
            Err(false) => {
                // Block until space is available or peer closes.
                SOCKET_WQS[wq_idx].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[wq_idx];
                    if !slot.valid
                        || slot.state != UnixState::Connected
                        || slot.generation != slot_gen
                    {
                        return true; // Slot reused — bail out.
                    }
                    if slot.peer_closed {
                        return true;
                    }
                    let peer = slot.peer_idx as usize;
                    let side = slot.side;
                    let (a_idx, _) = match side {
                        PairSide::A => (wq_idx, peer),
                        PairSide::B => (peer, wq_idx),
                    };
                    let a_slot = &state.slots[a_idx];
                    let buf = match side {
                        PairSide::A => &a_slot.buf_a_to_b,
                        PairSide::B => &a_slot.buf_b_to_a,
                    };
                    buf.has_space()
                });
                // Loop back to retry the write.
            }
        }
    }
}

/// Receive data from a connected AF_UNIX socket.
///
/// Returns the number of bytes read, 0 on EOF, or a negative errno.
pub fn unix_recv(handle: SocketHandle, buf: &mut [u8]) -> i32 {
    if buf.is_empty() {
        return 0;
    }

    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };

    let out = buf;

    // Use the generation from the handle for stale-detection in condition closures.
    let slot_gen = handle.generation();

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let Some(i) = validate_socket_handle(&state, handle) else {
                return -107; // ENOTCONN (stale handle)
            };
            let slot = &state.slots[i];
            if slot.state != UnixState::Connected {
                return -107; // ENOTCONN
            }

            // Capture nonblocking INSIDE the validated section (fixes TOCTOU).
            let nonblocking = slot.nonblocking;

            let peer = slot.peer_idx as usize;
            let side = slot.side;
            let peer_closed = slot.peer_closed;

            // Ring buffers live on the side-A slot.
            let (a_idx, _b_idx) = match side {
                PairSide::A => (i, peer),
                PairSide::B => (peer, i),
            };

            // Validate the side-A slot's buffers are still live.
            if a_idx >= MAX_UNIX_SOCKETS || state.slots[a_idx].buf_refcount == 0 {
                return if peer_closed { 0 } else { -107 }; // EOF or ENOTCONN
            }

            let a_slot = &mut state.slots[a_idx];
            let rbuf = match side {
                PairSide::A => &mut a_slot.buf_b_to_a,
                PairSide::B => &mut a_slot.buf_a_to_b,
            };

            if !rbuf.is_empty() {
                let n = rbuf.read_into(out);
                Ok((n, peer))
            } else if peer_closed {
                // EOF — peer has closed.
                Ok((0, peer))
            } else {
                Err(nonblocking)
            }
        };

        match result {
            Ok((n, peer)) => {
                if n > 0 {
                    // Wake the peer's send wait queue (we freed buffer space).
                    // peer was captured under the lock above — no re-acquire needed.
                    if peer < MAX_UNIX_SOCKETS {
                        SOCKET_WQS[peer].wake_all();
                    }
                }
                return n as i32;
            }
            Err(true) => {
                return -11; // EAGAIN
            }
            Err(false) => {
                // Block until data arrives or peer closes.
                SOCKET_WQS[wq_idx].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[wq_idx];
                    if !slot.valid
                        || slot.state != UnixState::Connected
                        || slot.generation != slot_gen
                    {
                        return true; // Slot reused — bail out.
                    }
                    if slot.peer_closed {
                        return true;
                    }
                    let peer = slot.peer_idx as usize;
                    let side = slot.side;
                    let (a_idx, _) = match side {
                        PairSide::A => (wq_idx, peer),
                        PairSide::B => (peer, wq_idx),
                    };
                    let a_slot = &state.slots[a_idx];
                    let rbuf = match side {
                        PairSide::A => &a_slot.buf_b_to_a,
                        PairSide::B => &a_slot.buf_a_to_b,
                    };
                    !rbuf.is_empty()
                });
            }
        }
    }
}

/// Send data on a connected AF_UNIX socket, with optional in-flight fds (SCM_RIGHTS).
///
/// `inflight_fds` contains already-dup'd (handle, ops) pairs.  On success,
/// ownership transfers to the ancillary queue.  On failure, the caller must
/// release them.
///
/// Returns bytes written or negative errno.
pub fn unix_sendmsg(
    handle: SocketHandle,
    data: &[u8],
    inflight_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    fd_count: usize,
) -> i32 {
    // Send the data bytes first (reuses existing unix_send logic inline).
    let bytes_sent = if !data.is_empty() {
        let rc = unix_send(handle, data);
        if rc < 0 {
            return rc;
        }
        rc
    } else {
        0
    };

    // Push in-flight fds into the ancillary queue.
    if fd_count > 0 {
        let mut state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return -107; // ENOTCONN
        };
        let slot = &state.slots[i];
        if slot.state != UnixState::Connected {
            return -107; // ENOTCONN
        }
        let side = slot.side;
        let peer = slot.peer_idx as usize;
        let a_idx = match side {
            PairSide::A => i,
            PairSide::B => peer,
        };
        if a_idx >= MAX_UNIX_SOCKETS || state.slots[a_idx].buf_refcount == 0 {
            return -32; // EPIPE
        }

        let anc = match side {
            PairSide::A => &mut state.slots[a_idx].anc_a_to_b,
            PairSide::B => &mut state.slots[a_idx].anc_b_to_a,
        };

        for j in 0..fd_count {
            let (handle, ops) = inflight_fds[j];
            if !anc.push(InFlightFd { handle, ops }) {
                // Queue full — release remaining fds and return error
                for k in j..fd_count {
                    inflight_fds[k].1.release(inflight_fds[k].0);
                }
                return -12; // ENOMEM (queue full)
            }
            // Mark as consumed so caller doesn't double-release
            inflight_fds[j] = (0, inflight_fds[j].1);
        }

        // Wake peer so recvmsg can pick up the fds
        drop(state);
        if peer < MAX_UNIX_SOCKETS {
            SOCKET_WQS[peer].wake_all();
        }
    }

    bytes_sent
}

/// Receive data from a connected AF_UNIX socket, with optional in-flight fds.
///
/// `out_fds` receives (handle, ops) pairs.  Returns (bytes_read, fd_count).
/// Negative bytes_read indicates an error.
pub fn unix_recvmsg(
    handle: SocketHandle,
    buf: &mut [u8],
    out_fds: &mut [(usize, &'static dyn slopos_abi::file_ops::FileOps)],
    max_fds: usize,
) -> (i32, usize) {
    // Receive data bytes first (reuses existing unix_recv logic).
    let bytes_read = unix_recv(handle, buf);

    // Drain in-flight fds from the ancillary queue.
    let mut received_fds = 0usize;
    {
        let mut state = UNIX_STATE.lock();
        if let Some(i) = validate_socket_handle(&state, handle) {
            let slot = &state.slots[i];
            if slot.state == UnixState::Connected {
                let side = slot.side;
                let peer = slot.peer_idx as usize;
                let a_idx = match side {
                    PairSide::A => i,
                    PairSide::B => peer,
                };
                if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].buf_refcount > 0 {
                    // Read from the queue that the PEER wrote to (opposite direction).
                    let anc = match side {
                        // If we are side A, peer is side B, peer writes to anc_b_to_a
                        PairSide::A => &mut state.slots[a_idx].anc_b_to_a,
                        // If we are side B, peer is side A, peer writes to anc_a_to_b
                        PairSide::B => &mut state.slots[a_idx].anc_a_to_b,
                    };

                    let (mut entries, count) = anc.drain();
                    for j in 0..count as usize {
                        if let Some(fd) = entries[j].take() {
                            if received_fds < max_fds {
                                out_fds[received_fds] = (fd.handle, fd.ops);
                                received_fds += 1;
                            } else {
                                // Doesn't fit — release
                                fd.ops.release(fd.handle);
                            }
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
/// and their side-A peers are notified.  This prevents permanent slot
/// leaks and ghost connections.
pub fn unix_close(handle: SocketHandle) -> i32 {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return -9;
    };

    // Collect wakeup targets under the lock, wake outside.
    let mut wake_peer: Option<usize> = None;
    // Side-A peers of backlog entries that need waking after lock drop.
    let mut backlog_a_peers: [usize; MAX_BACKLOG] = [usize::MAX; MAX_BACKLOG];
    let mut backlog_wake_count = 0usize;
    let was_listener;

    {
        let mut state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return -9;
        };

        if state.slots[i].state == UnixState::Connected {
            let p = state.slots[i].peer_idx as usize;
            if p < MAX_UNIX_SOCKETS && p != i && state.slots[p].valid {
                state.slots[p].peer_closed = true;
            }
            wake_peer = Some(p);

            // Decrement refcount on the side-A slot.  Buffers freed
            // only when both sides have closed (refcount reaches 0).
            let a_idx = match state.slots[i].side {
                PairSide::A => i,
                PairSide::B => state.slots[i].peer_idx as usize,
            };
            if a_idx < MAX_UNIX_SOCKETS {
                let rc = state.slots[a_idx].buf_refcount.saturating_sub(1);
                state.slots[a_idx].buf_refcount = rc;
                if rc == 0 {
                    state.slots[a_idx].buf_a_to_b.release();
                    state.slots[a_idx].buf_b_to_a.release();
                    // Release any unclaimed in-flight fds
                    state.slots[a_idx].anc_a_to_b.release_all();
                    state.slots[a_idx].anc_b_to_a.release_all();
                }
            }
        }

        was_listener = state.slots[i].state == UnixState::Listening;

        // Clean up pending backlog entries for listeners.  Each entry
        // is a side-B slot created by unix_connect() but never handed
        // to userspace via accept().  Close B, notify A, decrement
        // A's buffer refcount.
        if was_listener {
            let bl = state.slots[i].backlog_len as usize;
            for k in 0..bl {
                let b_idx = state.slots[i].backlog[k] as usize;
                if b_idx >= MAX_UNIX_SOCKETS || !state.slots[b_idx].valid {
                    continue;
                }
                let a_idx = state.slots[b_idx].peer_idx as usize;

                // Close the orphaned B-slot.
                state.slots[b_idx].valid = false;
                state.slots[b_idx].state = UnixState::Closed;

                // Notify the A-side peer and release B's buffer ref.
                if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].valid {
                    state.slots[a_idx].peer_closed = true;
                    let rc = state.slots[a_idx].buf_refcount.saturating_sub(1);
                    state.slots[a_idx].buf_refcount = rc;
                    if rc == 0 {
                        state.slots[a_idx].buf_a_to_b.release();
                        state.slots[a_idx].buf_b_to_a.release();
                        state.slots[a_idx].anc_a_to_b.release_all();
                        state.slots[a_idx].anc_b_to_a.release_all();
                    }
                    backlog_a_peers[backlog_wake_count] = a_idx;
                    backlog_wake_count += 1;
                }
            }
            state.slots[i].backlog_len = 0;
        }

        state.slots[i].valid = false;
        state.slots[i].state = UnixState::Closed;
    }

    // Wake peer waiters outside the lock.
    if let Some(peer) = wake_peer {
        if peer < MAX_UNIX_SOCKETS {
            SOCKET_WQS[peer].wake_all();
        }
    }

    // Wake A-side peers of cleaned-up backlog entries so they see EPIPE.
    for k in 0..backlog_wake_count {
        let a_idx = backlog_a_peers[k];
        if a_idx < MAX_UNIX_SOCKETS {
            SOCKET_WQS[a_idx].wake_all();
        }
    }

    // Wake blocked accept() callers on the closing listener.
    if was_listener {
        SOCKET_WQS[wq_idx].wake_all();
    }

    0
}

/// Return POLL* bitmask of currently ready events for a Unix socket.
pub fn unix_poll_events(handle: SocketHandle, requested: u16) -> u16 {
    let state = UNIX_STATE.lock();
    let Some(i) = validate_socket_handle(&state, handle) else {
        return 0;
    };
    let slot = &state.slots[i];

    match slot.state {
        UnixState::Listening => {
            if slot.backlog_len > 0 && (requested & POLLIN) != 0 {
                POLLIN
            } else {
                0
            }
        }
        UnixState::Connected => {
            let mut revents = 0u16;

            let peer = slot.peer_idx as usize;
            let side = slot.side;

            // Determine readable/writable from ring buffers on side-A slot.
            let (a_idx, _) = match side {
                PairSide::A => (i, peer),
                PairSide::B => (peer, i),
            };

            if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].buf_refcount > 0 {
                let a_slot = &state.slots[a_idx];
                let rbuf = match side {
                    PairSide::A => &a_slot.buf_b_to_a,
                    PairSide::B => &a_slot.buf_a_to_b,
                };
                let wbuf = match side {
                    PairSide::A => &a_slot.buf_a_to_b,
                    PairSide::B => &a_slot.buf_b_to_a,
                };

                if !rbuf.is_empty() && (requested & POLLIN) != 0 {
                    revents |= POLLIN;
                }
                if wbuf.has_space() && (requested & POLLOUT) != 0 {
                    revents |= POLLOUT;
                }
            }

            if slot.peer_closed {
                revents |= POLLHUP;
                // Also report readable on hangup so readers can get EOF.
                if (requested & POLLIN) != 0 {
                    revents |= POLLIN;
                }
            }

            revents
        }
        UnixState::Closed => POLLHUP,
        _ => 0,
    }
}

/// Fused poll: register on wait queue THEN check readiness.
///
/// Follows the Linux `sock_poll_wait` + readiness-check pattern: the
/// task is placed on the wait queue BEFORE the readiness snapshot so
/// that any wakeup firing after registration is guaranteed to find us.
/// The readiness check after registration acts as its own "triggered"
/// verification — if data arrived between registration and the check,
/// we see it and return immediately.
///
/// The caller (`syscall_poll`) sets `WillBlock` via `prepare_to_wait()`
/// before calling this function.  If `wake_all` → `unblock_task` fires
/// between registration and `block_current_task_with_timeout`, the CAS
/// `WillBlock → Blocked` fails and the task stays Running.
pub fn unix_poll_fused(handle: SocketHandle, requested: u16) -> (u16, bool) {
    let Some(wq_idx) = handle.slot_for_wq() else {
        return (0, false);
    };

    // ── Phase 1: Register on the unified socket wait queue ─────────
    //
    // Single queue per socket (Linux `sk->sk_wq` pattern).  All blocking
    // I/O and poll share it.  Spurious wakeups are harmless — every
    // waiter re-checks its condition.
    let registered = SOCKET_WQS[wq_idx].enqueue_current();

    // ── Phase 3: Check readiness AFTER registration ────────────────
    let revents = {
        let state = UNIX_STATE.lock();
        let Some(i) = validate_socket_handle(&state, handle) else {
            return (0, false);
        };
        let slot = &state.slots[i];

        match slot.state {
            UnixState::Listening => {
                if slot.backlog_len > 0 && (requested & POLLIN) != 0 {
                    POLLIN
                } else {
                    0
                }
            }
            UnixState::Connected => {
                let mut rev = 0u16;
                let peer = slot.peer_idx as usize;
                let side = slot.side;
                let (a_idx, _) = match side {
                    PairSide::A => (i, peer),
                    PairSide::B => (peer, i),
                };
                if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].buf_refcount > 0 {
                    let a_slot = &state.slots[a_idx];
                    let rbuf = match side {
                        PairSide::A => &a_slot.buf_b_to_a,
                        PairSide::B => &a_slot.buf_a_to_b,
                    };
                    let wbuf = match side {
                        PairSide::A => &a_slot.buf_a_to_b,
                        PairSide::B => &a_slot.buf_b_to_a,
                    };
                    if !rbuf.is_empty() && (requested & POLLIN) != 0 {
                        rev |= POLLIN;
                    }
                    if wbuf.has_space() && (requested & POLLOUT) != 0 {
                        rev |= POLLOUT;
                    }
                }
                if slot.peer_closed {
                    rev |= POLLHUP;
                    if (requested & POLLIN) != 0 {
                        rev |= POLLIN;
                    }
                }
                rev
            }
            UnixState::Closed => POLLHUP,
            _ => 0,
        }
    };

    (revents, registered)
}

/// Enqueue the current task on the appropriate wait queue(s) for poll().
///
/// Registers on all three queues unconditionally to avoid the TOCTOU
/// race where the socket transitions between Listening and Connected
/// after peeking state.  See `unix_poll_fused` for full rationale.
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
