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

    /// Allocate the backing buffer.  Returns `true` on success.
    fn alloc(&mut self) -> bool {
        if self.buf.is_some() {
            return true;
        }
        self.buf = Some(Box::new([0u8; UNIX_BUF_SIZE]));
        true
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

    fn reset(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
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
        }
    }

    fn reset(&mut self) {
        self.valid = false;
        self.state = UnixState::Unbound;
        self.path = [0u8; UNIX_PATH_MAX];
        self.path_len = 0;
        self.backlog = [0u32; MAX_BACKLOG];
        self.backlog_len = 0;
        // Release heap-allocated ring buffers.
        self.buf_a_to_b.release();
        self.buf_b_to_a.release();
        self.side = PairSide::A;
        self.peer_idx = u32::MAX;
        self.peer_closed = false;
        self.nonblocking = false;
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
pub fn unix_create() -> i32 {
    let mut state = UNIX_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid {
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
        if !slot.valid {
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

    // Set up caller (side A) — allocate ring buffers on the A-side slot.
    {
        let slot = &mut state.slots[i];
        if !slot.buf_a_to_b.alloc() || !slot.buf_b_to_a.alloc() {
            // Allocation failed — release partial allocs and the B slot.
            slot.buf_a_to_b.release();
            slot.buf_b_to_a.release();
            state.slots[b_idx].valid = false;
            return -12; // ENOMEM
        }
        slot.state = UnixState::Connected;
        slot.side = PairSide::A;
        slot.peer_idx = b_idx as u32;
        slot.peer_closed = false;
        slot.buf_a_to_b.reset();
        slot.buf_b_to_a.reset();
    }

    // Set up accepted side (side B) — shares ring buffers conceptually,
    // but since B is a separate slot we point it at the caller.
    let accepted = &mut state.slots[b_idx];
    accepted.state = UnixState::Connected;
    accepted.side = PairSide::B;
    accepted.peer_idx = i as u32;
    accepted.peer_closed = false;
    // B reads from A's buf_a_to_b and writes to A's buf_b_to_a.
    // Since we store buffers on side A's slot, side B must reference A.
    // We store the peer_idx so send/recv can find the right slot.

    // Enqueue B in the listener's backlog.
    let listener = &mut state.slots[listener_idx];
    let bl = listener.backlog_len as usize;
    listener.backlog[bl] = b_idx as u32;
    listener.backlog_len += 1;

    // Drop lock before waking.
    drop(state);

    // Wake the listener's accept() call.
    ACCEPT_WQS[listener_idx].wake_all();

    0
}

/// Send data on a connected AF_UNIX socket.
///
/// Returns the number of bytes written, or a negative errno.
pub fn unix_send(idx: u32, data: *const u8, len: usize) -> i32 {
    if data.is_null() && len != 0 {
        return -14; // EFAULT
    }
    if len == 0 {
        return 0;
    }

    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

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
    if buf.is_null() && len != 0 {
        return -14; // EFAULT
    }
    if len == 0 {
        return 0;
    }

    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

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
pub fn unix_close(idx: u32) -> i32 {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return -9;
    }

    let (peer_idx, was_listener) = {
        let mut state = UNIX_STATE.lock();
        if !state.slots[i].valid {
            return -9;
        }

        let peer = if state.slots[i].state == UnixState::Connected {
            let p = state.slots[i].peer_idx as usize;
            // Mark peer as peer_closed.
            if p < MAX_UNIX_SOCKETS && p != i && state.slots[p].valid {
                state.slots[p].peer_closed = true;
            }
            Some(p)
        } else {
            None
        };

        let was_listener = state.slots[i].state == UnixState::Listening;

        // If this is side A, free the ring buffers now.
        if state.slots[i].side == PairSide::A {
            state.slots[i].buf_a_to_b.release();
            state.slots[i].buf_b_to_a.release();
        }

        state.slots[i].valid = false;
        state.slots[i].state = UnixState::Closed;

        (peer, was_listener)
    };

    // Wake peer waiters so they see the close / EOF.
    if let Some(peer) = peer_idx {
        if peer < MAX_UNIX_SOCKETS {
            RECV_WQS[peer].wake_all();
            SEND_WQS[peer].wake_all();
        }
    }

    // If we were a listener, wake any blocked accept() calls.
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

            if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].valid {
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
    let registered = if is_listener {
        ACCEPT_WQS[i].enqueue_current()
    } else {
        RECV_WQS[i].enqueue_current()
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
                if a_idx < MAX_UNIX_SOCKETS && state.slots[a_idx].valid {
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
            _ => 0,
        }
    };

    (revents, registered)
}

/// Enqueue the current task on the appropriate wait queue for poll().
///
/// Listener sockets register on `ACCEPT_WQS` (woken by `unix_connect`),
/// connected sockets register on `RECV_WQS` (woken by peer `unix_send`).
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
        RECV_WQS[i].enqueue_current()
    }
}

/// Remove the current task from the appropriate poll wait queue.
pub fn unix_poll_unregister(idx: u32) {
    let i = idx as usize;
    if i >= MAX_UNIX_SOCKETS {
        return;
    }
    // Remove from both — the task is on at most one, and remove is a no-op
    // if the task isn't present.  This avoids needing to re-check socket
    // state (which may have changed between register and unregister).
    RECV_WQS[i].remove_current();
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
