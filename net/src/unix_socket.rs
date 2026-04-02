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
use slopos_sync::{IrqMutex, WaitQueue};

/// Maximum number of concurrent AF_UNIX sockets.
pub const MAX_UNIX_SOCKETS: usize = 32;

/// Per-direction ring buffer size (16 KB).
pub const UNIX_BUF_SIZE: usize = 16384;

/// Maximum abstract namespace path length.
const UNIX_PATH_MAX: usize = 108;

/// Maximum pending connections in the accept backlog.
/// Matches Wayland's libwayland-server default of 128.
const MAX_BACKLOG: usize = 32;

// ---------------------------------------------------------------------------
// Wait queues — one set per socket slot, separate from UNIX_STATE.
// ---------------------------------------------------------------------------

static RECV_WQS: [WaitQueue; MAX_UNIX_SOCKETS] = [const { WaitQueue::new() }; MAX_UNIX_SOCKETS];
static SEND_WQS: [WaitQueue; MAX_UNIX_SOCKETS] = [const { WaitQueue::new() }; MAX_UNIX_SOCKETS];
static ACCEPT_WQS: [WaitQueue; MAX_UNIX_SOCKETS] = [const { WaitQueue::new() }; MAX_UNIX_SOCKETS];

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

static UNIX_STATE: IrqMutex<UnixSocketState> = IrqMutex::new(UnixSocketState::new());

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate a new AF_UNIX socket slot. Returns slot index or negative errno.
///
/// Slots with `buf_refcount > 0` are skipped even if `valid == false`:
/// the peer side still holds a live reference to the ring buffers on
/// that slot, so reusing it would corrupt the peer's data.
pub fn unix_create() -> i32 {
    let mut state = UNIX_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid && slot.buf_refcount == 0 {
            slot.reset();
            slot.valid = true;
            slot.state = UnixState::Unbound;
            return idx as i32;
        }
    }
    -1 // ENOMEM
}

/// Bind a socket to an abstract namespace path.
pub fn unix_bind(idx: u32, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    let mut state = UNIX_STATE.lock();
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9; // EBADF
    }
    let slot = &mut state.slots[i];
    if !slot.valid {
        return -9; // EBADF
    }
    if slot.state != UnixState::Unbound {
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
pub fn unix_listen(idx: u32, _backlog: u32) -> i32 {
    let mut state = UNIX_STATE.lock();
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }
    let slot = &mut state.slots[i];
    if !slot.valid {
        return -9;
    }
    if slot.state != UnixState::Bound {
        return -22; // EINVAL — must be bound first
    }
    slot.state = UnixState::Listening;
    slot.backlog_len = 0;
    0
}

/// Accept a pending connection from a listening socket.
///
/// Blocks the caller until a connection arrives (unless non-blocking).
/// Returns the index of a new connected socket slot.
pub fn unix_accept(idx: u32) -> i32 {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

    loop {
        // Try to dequeue a pending connection.
        let (nonblocking, got) = {
            let mut state = UNIX_STATE.lock();
            let slot = &mut state.slots[i];
            if !slot.valid || slot.state != UnixState::Listening {
                return -22; // EINVAL
            }
            let nb = slot.nonblocking;
            if slot.backlog_len > 0 {
                let connected_idx = slot.backlog[0];
                // Shift backlog entries down.
                let bl = slot.backlog_len as usize;
                for k in 1..bl {
                    slot.backlog[k - 1] = slot.backlog[k];
                }
                slot.backlog[bl - 1] = 0;
                slot.backlog_len -= 1;
                (nb, Some(connected_idx))
            } else {
                (nb, None)
            }
        };

        if let Some(connected_idx) = got {
            return connected_idx as i32;
        }

        if nonblocking {
            return -11; // EAGAIN
        }

        // Block until a connection arrives.
        ACCEPT_WQS[i].wait_event(|| {
            let state = UNIX_STATE.lock();
            let slot = &state.slots[i];
            !slot.valid || slot.backlog_len > 0
        });
    }
}

/// Connect to a listening socket identified by path.
///
/// Creates a connected pair: the caller's slot becomes side A, a new slot
/// is allocated for side B (which is enqueued in the listener's backlog
/// for `accept()` to return).
pub fn unix_connect(idx: u32, path: &[u8]) -> i32 {
    if path.is_empty() || path.len() > UNIX_PATH_MAX {
        return -22; // EINVAL
    }

    // Pre-allocate ring buffers BEFORE acquiring the global lock.
    // This avoids blocking all socket operations during the heap allocation
    // (each buffer is UNIX_BUF_SIZE = 16 KB).
    let pre_buf_a = Box::new([0u8; UNIX_BUF_SIZE]);
    let pre_buf_b = Box::new([0u8; UNIX_BUF_SIZE]);

    let mut state = UNIX_STATE.lock();
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }
    {
        let slot = &state.slots[i];
        if !slot.valid {
            return -9;
        }
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
    let a_gen = state.slots[i].generation;
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

    let _ = a_gen; // generation stored for future use in wait_event guards

    // Wake the listener's accept() call.
    ACCEPT_WQS[listener_idx].wake_all();

    0
}

/// Send data on a connected AF_UNIX socket.
///
/// Returns the number of bytes written, or a negative errno.
pub fn unix_send(idx: u32, data: *const u8, len: usize) -> i32 {
    if data.is_null() || len == 0 {
        return if len == 0 { 0 } else { -14 }; // EFAULT for null+nonzero
    }

    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

    // SAFETY: data is non-null (checked above) and caller guarantees len
    // bytes are readable (kernel scratch buffer from net_handlers).
    let input = unsafe { core::slice::from_raw_parts(data, len) };

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let slot = &state.slots[i];
            if !slot.valid || slot.state != UnixState::Connected {
                return -107; // ENOTCONN
            }
            if slot.peer_closed {
                return -32; // EPIPE
            }

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

            let a_slot = &mut state.slots[a_idx];
            let buf = match side {
                PairSide::A => &mut a_slot.buf_a_to_b,
                PairSide::B => &mut a_slot.buf_b_to_a,
            };

            if buf.has_space() {
                let n = buf.write_from(input);
                Ok(n)
            } else {
                // Check non-blocking before deciding to block.
                let nb = state.slots[i].nonblocking;
                Err(nb)
            }
        };

        match result {
            Ok(n) => {
                // Wake the peer's recv wait queue.
                let peer = {
                    let state = UNIX_STATE.lock();
                    state.slots[i].peer_idx as usize
                };
                if peer < MAX_UNIX_SOCKETS {
                    RECV_WQS[peer].wake_all();
                }
                return n as i32;
            }
            Err(true) => {
                // Non-blocking and buffer full.
                return -11; // EAGAIN
            }
            Err(false) => {
                // Block until space is available or peer closes.
                SEND_WQS[i].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[i];
                    if !slot.valid || slot.state != UnixState::Connected {
                        return true;
                    }
                    if slot.peer_closed {
                        return true;
                    }
                    let peer = slot.peer_idx as usize;
                    let side = slot.side;
                    let (a_idx, _) = match side {
                        PairSide::A => (i, peer),
                        PairSide::B => (peer, i),
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
pub fn unix_recv(idx: u32, buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 {
        return if len == 0 { 0 } else { -14 }; // EFAULT for null+nonzero
    }

    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

    // SAFETY: buf is non-null (checked above) and caller guarantees len
    // bytes are writable (kernel scratch buffer from net_handlers).
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };

    loop {
        let result = {
            let mut state = UNIX_STATE.lock();
            let slot = &state.slots[i];
            if !slot.valid || slot.state != UnixState::Connected {
                return -107; // ENOTCONN
            }

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
                Ok(n)
            } else if peer_closed {
                // EOF — peer has closed.
                Ok(0)
            } else {
                let nb = state.slots[i].nonblocking;
                Err(nb)
            }
        };

        match result {
            Ok(n) => {
                if n > 0 {
                    // Wake the peer's send wait queue (we freed buffer space).
                    let peer = {
                        let state = UNIX_STATE.lock();
                        state.slots[i].peer_idx as usize
                    };
                    if peer < MAX_UNIX_SOCKETS {
                        SEND_WQS[peer].wake_all();
                    }
                }
                return n as i32;
            }
            Err(true) => {
                return -11; // EAGAIN
            }
            Err(false) => {
                // Block until data arrives or peer closes.
                RECV_WQS[i].wait_event(|| {
                    let state = UNIX_STATE.lock();
                    let slot = &state.slots[i];
                    if !slot.valid || slot.state != UnixState::Connected {
                        return true;
                    }
                    if slot.peer_closed {
                        return true;
                    }
                    let peer = slot.peer_idx as usize;
                    let side = slot.side;
                    let (a_idx, _) = match side {
                        PairSide::A => (i, peer),
                        PairSide::B => (peer, i),
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

/// Close an AF_UNIX socket. Wakes all waiters on the peer if connected.
///
/// For listeners, all pending backlog entries (side-B slots that were
/// created by `unix_connect()` but never `accept()`-ed) are closed
/// and their side-A peers are notified.  This prevents permanent slot
/// leaks and ghost connections.
pub fn unix_close(idx: u32) -> i32 {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

    // Collect wakeup targets under the lock, wake outside.
    let mut wake_peer: Option<usize> = None;
    // Side-A peers of backlog entries that need waking after lock drop.
    let mut backlog_a_peers: [usize; MAX_BACKLOG] = [usize::MAX; MAX_BACKLOG];
    let mut backlog_wake_count = 0usize;
    let was_listener;

    {
        let mut state = UNIX_STATE.lock();
        if !state.slots[i].valid {
            return -9;
        }

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
            RECV_WQS[peer].wake_all();
            SEND_WQS[peer].wake_all();
        }
    }

    // Wake A-side peers of cleaned-up backlog entries so they see EPIPE.
    for k in 0..backlog_wake_count {
        let a_idx = backlog_a_peers[k];
        if a_idx < MAX_UNIX_SOCKETS {
            RECV_WQS[a_idx].wake_all();
            SEND_WQS[a_idx].wake_all();
        }
    }

    // Wake blocked accept() callers on the closing listener.
    if was_listener {
        ACCEPT_WQS[i].wake_all();
    }

    0
}

/// Return POLL* bitmask of currently ready events for a Unix socket.
pub fn unix_poll_events(idx: u32, requested: u16) -> u16 {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return 0;
    }

    let state = UNIX_STATE.lock();
    let slot = &state.slots[i];
    if !slot.valid {
        return 0;
    }

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
pub fn unix_poll_fused(idx: u32, requested: u16) -> (u16, bool) {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return (0, false);
    }

    // ── Phase 1: Peek socket type for WQ selection ─────────────────
    let is_listener = {
        let state = UNIX_STATE.lock();
        let slot = &state.slots[i];
        if !slot.valid {
            return (0, false);
        }
        slot.state == UnixState::Listening
    };

    // ── Phase 2: Register FIRST (Linux sock_poll_wait pattern) ─────
    // By the time the readiness check below runs, the task is already
    // visible to wake_all().  This eliminates the lost-wakeup race
    // where data arrived between a check and a late registration.
    //
    // Connected sockets register on RECV_WQS for POLLIN (woken by peer
    // send) and on SEND_WQS for POLLOUT (woken by peer recv freeing
    // buffer space).  Both are registered if both events are requested.
    let registered = if is_listener {
        ACCEPT_WQS[i].enqueue_current()
    } else {
        let mut reg = false;
        if (requested & POLLIN) != 0 || requested == 0 {
            reg |= RECV_WQS[i].enqueue_current();
        }
        if (requested & POLLOUT) != 0 {
            reg |= SEND_WQS[i].enqueue_current();
        }
        // Default: at least register on RECV if no specific event requested.
        if !reg {
            reg = RECV_WQS[i].enqueue_current();
        }
        reg
    };

    // ── Phase 3: Check readiness AFTER registration ────────────────
    let revents = {
        let state = UNIX_STATE.lock();
        let slot = &state.slots[i];
        if !slot.valid {
            return (0, false);
        }

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
/// Listener sockets register on `ACCEPT_WQS` (woken by `unix_connect`),
/// connected sockets register on both `RECV_WQS` and `SEND_WQS`.
pub fn unix_poll_register(idx: u32) -> bool {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return false;
    }
    let is_listener = {
        let state = UNIX_STATE.lock();
        state.slots[i].valid && state.slots[i].state == UnixState::Listening
    };
    if is_listener {
        ACCEPT_WQS[i].enqueue_current()
    } else {
        let r = RECV_WQS[i].enqueue_current();
        let s = SEND_WQS[i].enqueue_current();
        r || s
    }
}

/// Remove the current task from all poll wait queues for this socket.
pub fn unix_poll_unregister(idx: u32) {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return;
    }
    // Remove from all three — the task is on at most two (RECV + SEND for
    // connected sockets), and remove is a no-op if the task isn't present.
    RECV_WQS[i].remove_current();
    SEND_WQS[i].remove_current();
    ACCEPT_WQS[i].remove_current();
}

/// Set or clear non-blocking mode on a Unix socket.
pub fn unix_set_nonblocking(idx: u32, nonblocking: bool) -> i32 {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }
    let mut state = UNIX_STATE.lock();
    let slot = &mut state.slots[i];
    if !slot.valid {
        return -9;
    }
    slot.nonblocking = nonblocking;
    0
}
