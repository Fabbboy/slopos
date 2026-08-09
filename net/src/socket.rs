use core::fmt;

use slopos_ostd::AllocError;
use slopos_ostd::KVec;
use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::uframe::{coalesce_io_runs, copy_out_frames, redup_frames};
use slopos_ostd::mm::{VmReader, VmWriter};
use slopos_ostd::write_field;
use slopos_ostd::{Bitmap, words_for};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use crate::packetbuf::PacketBuf;
use crate::tcp;
use crate::tcp::listener as tcp_listener;
use crate::types::{Ipv4Addr, NetError, Port, SockAddr};

/// Internal storage for protocol-specific socket state.
pub enum SocketInner {
    /// UDP socket state (stateless at protocol level).
    Udp(UdpSocketInner),
    Icmp(IcmpSocketInner),
    Tcp(TcpSocketInner),
    Raw(RawSocketInner),
    /// AF_UNIX stream socket — actual state lives in `unix_socket::UNIX_STATE`.
    Unix(UnixSocketInner),
}

pub struct UdpSocketInner;

pub struct IcmpSocketInner {
    pub identifier: u16,
}

pub struct TcpSocketInner {
    /// Optional transport connection identifier.
    pub conn_id: Option<tcp::ConnId>,
    /// Two-queue listen state for TCP listening sockets.
    pub listen: Option<tcp_listener::TcpListenState>,
}

pub struct RawSocketInner;

/// AF_UNIX socket — the real state (ring buffers, wait queues) lives in
/// `unix_socket::UNIX_STATE`.  This struct only records which unix slot
/// and which side of the pair this FD represents.
pub struct UnixSocketInner {
    pub unix_idx: u32,
}

/// Socket status and mode flags.
///
/// This is a small bitflags-like wrapper with no external dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocketFlags(u32);

impl SocketFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);
    /// Non-blocking I/O mode.
    pub const O_NONBLOCK: Self = Self(1 << 0);
    /// Read side has been shut down.
    pub const SHUT_RD: Self = Self(1 << 1);
    /// Write side has been shut down.
    pub const SHUT_WR: Self = Self(1 << 2);

    /// Return `true` if all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Set the given flag bits.
    pub fn set(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    /// Clear the given flag bits.
    pub fn clear(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }

    /// Return raw bit representation.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Construct from raw bits.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }
}

/// Per-socket configurable options.
pub struct SocketOptions {
    /// Allow local address reuse.
    pub reuse_addr: bool,
    /// Receive buffer size in bytes.
    ///
    /// Default: 16384, valid range: 256..=262144.
    pub recv_buf_size: usize,
    /// Send buffer size in bytes.
    ///
    /// Default: 16384, valid range: 256..=262144.
    pub send_buf_size: usize,
    /// Receive timeout in milliseconds (`None` means infinite).
    pub recv_timeout: Option<u64>,
    /// Send timeout in milliseconds (`None` means infinite).
    pub send_timeout: Option<u64>,
    /// Enable keepalive (TCP only).
    pub keepalive: bool,
    /// Disable Nagle algorithm (TCP only).
    pub tcp_nodelay: bool,
}

impl SocketOptions {
    /// Default receive buffer size in bytes.
    pub const RECV_BUF_DEFAULT: usize = 16_384;
    /// Default send buffer size in bytes.
    pub const SEND_BUF_DEFAULT: usize = 16_384;
    /// Minimum allowed receive buffer size in bytes.
    pub const RECV_BUF_MIN: usize = 256;
    /// Maximum allowed receive buffer size in bytes.
    pub const RECV_BUF_MAX: usize = 262_144;
    /// Minimum allowed send buffer size in bytes.
    pub const SEND_BUF_MIN: usize = 256;
    /// Maximum allowed send buffer size in bytes.
    pub const SEND_BUF_MAX: usize = 262_144;

    /// Construct options with defaults.
    pub const fn new() -> Self {
        Self {
            reuse_addr: false,
            recv_buf_size: Self::RECV_BUF_DEFAULT,
            send_buf_size: Self::SEND_BUF_DEFAULT,
            recv_timeout: None,
            send_timeout: None,
            keepalive: false,
            tcp_nodelay: false,
        }
    }

    /// Validate and normalize a receive buffer size request.
    ///
    /// Returns `NetError::InvalidArgument` if the value is out of range.
    pub fn validate_recv_buf_size(size: usize) -> Result<usize, NetError> {
        if !(Self::RECV_BUF_MIN..=Self::RECV_BUF_MAX).contains(&size) {
            return Err(NetError::InvalidArgument);
        }
        Ok(size)
    }

    /// Validate and normalize a send buffer size request.
    ///
    /// Returns `NetError::InvalidArgument` if the value is out of range.
    pub fn validate_send_buf_size(size: usize) -> Result<usize, NetError> {
        if !(Self::SEND_BUF_MIN..=Self::SEND_BUF_MAX).contains(&size) {
            return Err(NetError::InvalidArgument);
        }
        Ok(size)
    }
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed-capacity queue with ring-buffer semantics.
///
/// Fixed-capacity queue with ring-buffer semantics.
/// Push never overwrites; it returns `false` when full.
pub struct BoundedQueue<T> {
    slots: KVec<Option<T>>,
    head: usize,
    len: usize,
}

impl<T> BoundedQueue<T> {
    /// Create a queue with `capacity` slots.
    pub fn new(capacity: usize) -> Self {
        let slots: KVec<Option<T>> = core::iter::repeat_with(|| None).take(capacity).collect();
        Self {
            slots,
            head: 0,
            len: 0,
        }
    }

    /// Push an item to the tail.
    ///
    /// Returns `false` if the queue is full; no item is overwritten.
    pub fn push(&mut self, item: T) -> bool {
        if self.is_full() {
            return false;
        }
        let cap = self.capacity();
        if cap == 0 {
            return false;
        }
        let tail = (self.head + self.len) % cap;
        self.slots[tail] = Some(item);
        self.len += 1;
        true
    }

    /// Pop an item from the head.
    pub fn pop(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let cap = self.capacity();
        if cap == 0 {
            return None;
        }
        let idx = self.head;
        self.head = (self.head + 1) % cap;
        self.len -= 1;
        self.slots[idx].take()
    }

    /// Return `true` if the queue has no elements.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return `true` if the queue cannot accept more elements.
    pub fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    /// Number of queued items.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Maximum number of storable items.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Clear all queued items.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            let _ = slot.take();
        }
        self.head = 0;
        self.len = 0;
    }

    /// Resize queue capacity, preserving item order. The queue is left
    /// untouched if either allocation fails.
    ///
    /// If `new_capacity` is smaller than current length, oldest items are kept
    /// until capacity is reached and the rest are dropped.
    pub fn resize(&mut self, new_capacity: usize) -> Result<(), AllocError> {
        let mut drained: KVec<T> = KVec::with_capacity(self.len)?;
        let mut slots: KVec<Option<T>> = KVec::with_capacity(new_capacity)?;
        for _ in 0..new_capacity {
            slots.push(None)?;
        }

        while let Some(item) = self.pop() {
            let _ = drained.push(item);
        }
        self.slots = slots;
        self.head = 0;
        self.len = 0;

        for item in drained {
            if !self.push(item) {
                break;
            }
        }
        Ok(())
    }
}

impl<T> fmt::Debug for BoundedQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedQueue")
            .field("len", &self.len)
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// Who opened a socket.
///
/// Two identifiers, because "may this caller be told who owns it" and "what
/// number names that owner" have different right answers.
///
/// `process_id` is the address space, and it decides disclosure: tasks sharing
/// one address space share the descriptor table that names this socket, so
/// withholding the owner from a sibling task would protect a fact the sibling
/// can read directly.
///
/// `task_id` is what is reported, because it is the number the rest of the
/// userland ABI speaks — `getpid` returns it, `kill` and `waitpid` accept it.
/// An address-space id names nothing any other syscall would take, so a tool
/// that printed one would produce a pid nobody could act on.
#[derive(Clone, Copy)]
pub struct SocketOwner {
    pub process_id: u32,
    pub task_id: u32,
}

impl SocketOwner {
    /// A socket no process opened, which in practice means one a test made.
    pub const UNOWNED: Self = Self {
        process_id: INVALID_PROCESS_ID,
        task_id: INVALID_PROCESS_ID,
    };
}

pub struct Socket {
    /// Protocol-specific socket state.
    pub inner: SocketInner,
    /// Generic lifecycle state.
    pub state: SocketState,
    /// Mode/shutdown flags.
    pub flags: SocketFlags,
    /// Socket options.
    pub options: SocketOptions,
    /// Optional bound local address.
    pub local_addr: Option<SockAddr>,
    /// Optional connected peer address.
    pub remote_addr: Option<SockAddr>,
    /// Receive queue of `(packet, source address)` tuples.
    pub recv_queue: BoundedQueue<(PacketBuf, SockAddr)>,
    /// Deferred error reported on next operation.
    pub pending_error: Option<NetError>,
    /// Who opened it. Set once, at the single allocation site.
    pub owner: SocketOwner,
}

impl Socket {
    /// Default receive queue capacity in packets.
    pub const RECV_QUEUE_DEFAULT_CAPACITY: usize = 16;

    /// Create a new socket object with defaults.
    pub fn new(inner: SocketInner) -> Self {
        Self {
            inner,
            state: SocketState::Unbound,
            flags: SocketFlags::NONE,
            options: SocketOptions::new(),
            local_addr: None,
            remote_addr: None,
            recv_queue: BoundedQueue::new(Self::RECV_QUEUE_DEFAULT_CAPACITY),
            pending_error: None,
            // Unowned until the allocation site says otherwise. Not `0`, which
            // is an id a real task can hold: a default that names a live task
            // would attribute a socket to one that never opened it, and
            // attribution is the field's whole purpose.
            owner: SocketOwner::UNOWNED,
        }
    }

    /// Return `true` if non-blocking mode is enabled.
    pub fn is_nonblocking(&self) -> bool {
        self.flags.contains(SocketFlags::O_NONBLOCK)
    }

    /// Return `true` if read shutdown is active.
    pub fn is_read_shutdown(&self) -> bool {
        self.flags.contains(SocketFlags::SHUT_RD)
    }

    /// Return `true` if write shutdown is active.
    pub fn is_write_shutdown(&self) -> bool {
        self.flags.contains(SocketFlags::SHUT_WR)
    }

    /// Enable or disable non-blocking mode.
    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        if nonblocking {
            self.flags.set(SocketFlags::O_NONBLOCK);
        } else {
            self.flags.clear(SocketFlags::O_NONBLOCK);
        }
    }

    /// Take and clear any pending error.
    pub fn take_pending_error(&mut self) -> Option<NetError> {
        self.pending_error.take()
    }
}

/// Slab-like socket table with freelist allocation.
pub struct SlabSocketTable {
    slots: KVec<Option<Socket>>,
    freelist: KVec<usize>,
    max_capacity: usize,
}

impl SlabSocketTable {
    /// Default initial slot count.
    pub const INITIAL_CAPACITY: usize = 64;
    /// Hard maximum slot count.
    pub const MAX_CAPACITY: usize = 1024;

    /// Create an empty, const-initializable table.
    ///
    /// This is used for global static initialization; first use should call
    /// [`init_if_needed`](Self::init_if_needed).
    pub const fn empty() -> Self {
        Self {
            slots: KVec::new(),
            freelist: KVec::new(),
            max_capacity: 0,
        }
    }

    /// Lazily initialize with default capacities if currently empty.
    ///
    /// Also syncs the allocation bitmap with the initial capacity.
    pub fn init_if_needed(&mut self) {
        if self.max_capacity == 0 {
            *self = Self::new(Self::INITIAL_CAPACITY, Self::MAX_CAPACITY);
            SOCKET_ALLOC.lock().set_capacity(Self::INITIAL_CAPACITY);
        }
    }

    /// Create a slab table with explicit initial and maximum capacities.
    ///
    /// Freelist is populated in reverse so index 0 is allocated first.
    pub fn new(initial_capacity: usize, max_capacity: usize) -> Self {
        let init_cap = core::cmp::min(initial_capacity, max_capacity);
        let mut slots: KVec<Option<Socket>> =
            core::iter::repeat_with(|| None).take(init_cap).collect();
        if slots.len() != init_cap {
            slots.clear();
        }
        let freelist = (0..init_cap).rev().collect();
        Self {
            slots,
            freelist,
            max_capacity,
        }
    }

    /// Allocate a new socket slot owned by `owner`.
    ///
    /// Returns the socket index on success. If no free slots are available,
    /// attempts to grow capacity (doubling, capped at `max_capacity`).
    /// Also marks the index in the allocation bitmap.
    ///
    /// The owner is taken here rather than assigned afterwards because this is
    /// the only place a socket comes into existence — both `socket_create` and
    /// `socket_accept` pass through it. An owner set after the fact is one an
    /// allocation path can forget, and the one that would forget is `accept`:
    /// its socket would answer to nobody while the connection it names is live.
    ///
    /// [`SocketOwner`] is what `net_query` redacts against, so a wrong value
    /// here is a wrong disclosure rather than a cosmetic slip.
    pub fn alloc(&mut self, inner: SocketInner, owner: SocketOwner) -> Option<usize> {
        self.init_if_needed();
        if self.freelist.is_empty() {
            self.grow();
        }
        let idx = self.freelist.pop()?;
        let mut socket = Socket::new(inner);
        socket.owner = owner;
        self.slots[idx] = Some(socket);
        {
            let mut alloc = SOCKET_ALLOC.lock();
            alloc.bitmap.set(idx);
            alloc.allocated_count += 1;
        }
        Some(idx)
    }

    /// Get an immutable socket reference by index.
    pub fn get(&self, idx: usize) -> Option<&Socket> {
        self.slots.get(idx)?.as_ref()
    }

    /// Get a mutable socket reference by index.
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Socket> {
        self.slots.get_mut(idx)?.as_mut()
    }

    /// Free an active slot and return it to the freelist.
    /// Also clears the index in the allocation bitmap.
    pub fn free(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            if slot.take().is_some() {
                let _ = self.freelist.push(idx);
                SOCKET_ALLOC.lock().free(idx);
            }
        }
    }

    /// Number of active sockets.
    pub fn count_active(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Current slot capacity.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Number of active sockets (alias of [`count_active`](Self::count_active)).
    pub fn len(&self) -> usize {
        self.count_active()
    }

    fn grow(&mut self) {
        let current = self.slots.len();
        if current >= self.max_capacity {
            return;
        }

        let mut new_cap = if current == 0 {
            Self::INITIAL_CAPACITY
        } else {
            current.saturating_mul(2)
        };
        if new_cap > self.max_capacity {
            new_cap = self.max_capacity;
        }
        if new_cap <= current {
            return;
        }

        let add = new_cap - current;
        self.slots
            .extend(core::iter::repeat_with(|| None).take(add));
        for idx in (current..new_cap).rev() {
            let _ = self.freelist.push(idx);
        }
        SOCKET_ALLOC.lock().set_capacity(new_cap);
    }
}

/// Ephemeral port allocator for dynamic local port selection.
///
/// this allocator and both old/new socket paths may use it.
/// Access must be serialized by the outer lock (no internal atomics).
#[derive(slopos_ostd::SlotFields)]
pub struct EphemeralPortAllocator {
    bitmap: Bitmap<{ words_for(Self::EPHEMERAL_PORT_COUNT) }>,
    next_port: u16,
    allocated_count: usize,
}

impl EphemeralPortAllocator {
    /// Start of IANA ephemeral range.
    pub const EPHEMERAL_PORT_START: u16 = 49_152;
    /// End of IANA ephemeral range.
    pub const EPHEMERAL_PORT_END: u16 = 65_535;
    /// Total number of ephemeral ports.
    pub const EPHEMERAL_PORT_COUNT: usize = 16_384;

    /// Create a fresh allocator with no allocated ports.
    pub const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
            next_port: Self::EPHEMERAL_PORT_START,
            allocated_count: 0,
        }
    }

    /// Reset every bit in-place, without materialising a fresh `Self`
    /// on the caller's stack. Equivalent to `*self = Self::new()` but
    /// keeps the 2 KiB bitmap on the heap slot it already occupies.
    pub fn reset(&mut self) {
        self.bitmap.clear_all();
        self.next_port = Self::EPHEMERAL_PORT_START;
        self.allocated_count = 0;
    }

    /// In-place [`Init`] recipe equivalent to [`Self::new`]. Used by
    /// `KBox::try_init(EphemeralPortAllocator::init_default())` so
    /// runtime callers (e.g. test fixtures) avoid the 2 KiB stack
    /// materialisation that `Self::new()` would otherwise incur. The
    /// `AllocError` carrier is the absorption shim required by
    /// `KBox::try_init`'s `E: From<AllocError>` bound — the closure
    /// itself never errors.
    pub fn init_default() -> impl Init<Self, slopos_ostd::mm::AllocError> {
        use slopos_ostd::mm::AllocError;
        // Closure zero-fills the whole slot (a valid empty `Bitmap`)
        // and then writes the two scalar fields whose `new()` isn't
        // all-zero.
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                slot.zero_all();
                write_field!(slot, next_port, Self::EPHEMERAL_PORT_START);
                write_field!(slot, allocated_count, 0);
                Ok(slot.finish())
            },
        )
    }

    /// Allocate one ephemeral port using round-robin selection.
    ///
    /// Returns `None` if all ephemeral ports are currently allocated.
    pub fn alloc(&mut self) -> Option<Port> {
        if self.allocated_count >= Self::EPHEMERAL_PORT_COUNT {
            return None;
        }

        let cursor = (self.next_port - Self::EPHEMERAL_PORT_START) as usize;
        let bit_idx = self
            .bitmap
            .find_next_zero(cursor, Self::EPHEMERAL_PORT_COUNT)
            .or_else(|| self.bitmap.find_next_zero(0, cursor))?;

        let candidate = Self::EPHEMERAL_PORT_START + bit_idx as u16;
        self.bitmap.set(bit_idx);
        self.allocated_count += 1;
        self.next_port = if candidate == Self::EPHEMERAL_PORT_END {
            Self::EPHEMERAL_PORT_START
        } else {
            candidate + 1
        };
        Some(Port(candidate))
    }

    /// Release a previously allocated ephemeral port.
    pub fn release(&mut self, port: Port) {
        let p = port.0;
        if !(Self::EPHEMERAL_PORT_START..=Self::EPHEMERAL_PORT_END).contains(&p) {
            return;
        }
        let bit_idx = (p - Self::EPHEMERAL_PORT_START) as usize;
        if self.bitmap.test(bit_idx) {
            self.bitmap.clear(bit_idx);
            self.allocated_count -= 1;
        }
    }

    /// Return `true` if `port` is currently allocated.
    pub fn is_in_use(&self, port: Port) -> bool {
        let p = port.0;
        if !(Self::EPHEMERAL_PORT_START..=Self::EPHEMERAL_PORT_END).contains(&p) {
            return false;
        }
        self.bitmap.test((p - Self::EPHEMERAL_PORT_START) as usize)
    }

    /// Number of currently available ephemeral ports.
    pub fn available(&self) -> usize {
        Self::EPHEMERAL_PORT_COUNT - self.allocated_count
    }
}

impl Default for EphemeralPortAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Socket allocation bitmap — separate lock from per-socket state
// =============================================================================

/// Socket allocation bitmap, keyed separately from per-socket data.
///
/// The bitmap tracks which socket indices are occupied. This allows
/// allocation decisions to be made without locking the full socket table,
/// reducing contention on the hot data-access path.
pub struct SocketAllocBitmap {
    bitmap: Bitmap<{ words_for(SlabSocketTable::MAX_CAPACITY) }>,
    allocated_count: usize,
    initialized_capacity: usize,
}

impl SocketAllocBitmap {
    pub const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
            allocated_count: 0,
            initialized_capacity: 0,
        }
    }

    /// Mark capacity as initialized (called when the socket table grows).
    pub fn set_capacity(&mut self, cap: usize) {
        self.initialized_capacity = cap;
    }

    /// Allocate a free index. Returns `None` if all slots are occupied.
    pub fn alloc(&mut self) -> Option<usize> {
        if self.allocated_count >= self.initialized_capacity {
            return None;
        }
        let idx = self.bitmap.find_next_zero(0, self.initialized_capacity)?;
        self.bitmap.set(idx);
        self.allocated_count += 1;
        Some(idx)
    }

    /// Release a previously allocated index.
    pub fn free(&mut self, idx: usize) {
        if idx < self.initialized_capacity && self.bitmap.test(idx) {
            self.bitmap.clear(idx);
            self.allocated_count = self.allocated_count.saturating_sub(1);
        }
    }

    /// Check if an index is currently allocated.
    pub fn is_allocated(&self, idx: usize) -> bool {
        idx < self.initialized_capacity && self.bitmap.test(idx)
    }

    /// Number of active sockets.
    pub fn count_active(&self) -> usize {
        self.allocated_count
    }

    /// Clear all allocations.
    pub fn clear(&mut self) {
        for i in 0..self.initialized_capacity {
            self.bitmap.clear(i);
        }
        self.allocated_count = 0;
    }
}

/// Socket allocation bitmap — separate lock from per-socket data.
pub static SOCKET_ALLOC: slopos_ostd::sync::SpinLock<SocketAllocBitmap> =
    slopos_ostd::sync::SpinLock::new(
        SocketAllocBitmap::new(),
        slopos_ostd::lock_class!("SOCKET_ALLOC", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

/// Global slab-based socket table.
pub static NEW_SOCKET_TABLE: slopos_ostd::sync::SpinLock<SlabSocketTable> =
    slopos_ostd::sync::SpinLock::new(
        SlabSocketTable::empty(),
        slopos_ostd::lock_class!("NEW_SOCKET_TABLE", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

/// Ephemeral port allocator.
pub static EPHEMERAL_PORTS: slopos_ostd::sync::SpinLock<EphemeralPortAllocator> =
    slopos_ostd::sync::SpinLock::new(
        EphemeralPortAllocator::new(),
        slopos_ostd::lock_class!("EPHEMERAL_PORTS", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

// =============================================================================
// Socket operations
// =============================================================================

use core::cmp;

use slopos_abi::KernelErrno;
use slopos_abi::event::{KernelEvent, SocketSlot};
use slopos_abi::net::{
    AF_INET, IPPROTO_ICMP, MAX_SOCKETS, NET_SOCK_CLOSE_WAIT, NET_SOCK_CLOSED, NET_SOCK_CLOSING,
    NET_SOCK_ESTABLISHED, NET_SOCK_FIN_WAIT1, NET_SOCK_FIN_WAIT2, NET_SOCK_LAST_ACK,
    NET_SOCK_LISTEN, NET_SOCK_SYN_RECV, NET_SOCK_SYN_SENT, NET_SOCK_TIME_WAIT, NET_SOCK_UNCONN,
    SOCK_DGRAM, SOCK_RAW, SOCK_STREAM,
};
use slopos_abi::task::INVALID_PROCESS_ID;

use slopos_abi::syscall::{
    ERRNO_EAFNOSUPPORT, ERRNO_EAGAIN, ERRNO_ECONNREFUSED, ERRNO_EDESTADDRREQ, ERRNO_EINPROGRESS,
    ERRNO_EINTR, ERRNO_EINVAL, ERRNO_EIO, ERRNO_EISCONN, ERRNO_ENOMEM, ERRNO_ENOTCONN,
    ERRNO_ENOTSOCK, ERRNO_EPIPE, ERRNO_EPROTONOSUPPORT, ERRNO_ETIMEDOUT, POLLERR, POLLHUP, POLLIN,
    POLLOUT,
};
use slopos_ostd::sync::{BUS, WaitAbort};

use crate as net;
use crate::tcp::{TCP_HEADER_LEN, TcpError, TcpOutSegment, TcpState};

const TCP_TX_MAX: usize = 1460;
pub const UDP_DGRAM_MAX_PAYLOAD: usize = 1472;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SocketState {
    Unbound,
    Bound,
    Listening,
    Connecting,
    Connected,
    Closed,
}

/// How many datagram slots a `SO_RCVBUF` of `bytes` buys.
///
/// The queue holds `PacketBuf`s, and every one of those is a buffer from the
/// global [`crate::pool::POOL_SIZE`] pool — so a queue longer than the pool
/// describes a depth no socket can ever reach while costing real memory per
/// slot, on every socket at once.
fn recv_queue_slots(bytes: usize) -> usize {
    let by_size = bytes / crate::pool::BUF_SIZE;
    by_size.clamp(1, crate::pool::POOL_SIZE)
}

/// The recv-readiness event for a socket table slot.
#[inline]
fn sock_recv_ev(idx: u32) -> KernelEvent {
    KernelEvent::SocketRecv {
        sock: SocketSlot(idx),
    }
}

/// The send-readiness event for a socket table slot.
#[inline]
fn sock_send_ev(idx: u32) -> KernelEvent {
    KernelEvent::SocketSend {
        sock: SocketSlot(idx),
    }
}

/// The accept-readiness event for a listening socket table slot.
#[inline]
fn sock_accept_ev(idx: u32) -> KernelEvent {
    KernelEvent::SocketAccept {
        sock: SocketSlot(idx),
    }
}
fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
}

fn map_tcp_err(err: TcpError) -> i32 {
    err.to_errno()
}

fn map_tcp_err_i64(err: TcpError) -> i64 {
    map_tcp_err(err) as i64
}

fn map_net_err(err: NetError) -> i32 {
    err.to_errno()
}

fn alloc_ephemeral_port() -> Option<Port> {
    EPHEMERAL_PORTS.lock().alloc()
}

fn be_port(port: u16) -> [u8; 2] {
    port.to_be_bytes()
}

pub fn socket_send_tcp_segment(seg: &TcpOutSegment, payload: &[u8]) -> i32 {
    let mut opt_len = 0usize;
    if seg.mss.is_some() {
        opt_len += 4;
    }
    if seg.wscale.is_some() {
        opt_len += 4;
    }
    if seg.sack_permitted {
        opt_len += 2;
    }
    if seg.timestamp.is_some() {
        opt_len += 12;
    }
    if seg.sack_block_count > 0 {
        opt_len += 2 + 2 + 8 * seg.sack_block_count as usize; // NOP NOP + kind len + blocks
    }
    let padded_opt_len = (opt_len + 3) & !3;
    let tcp_len = TCP_HEADER_LEN + padded_opt_len + payload.len();

    let mut tcp_segment = KVec::<u8>::zeroed(tcp_len).unwrap_or_else(|_| KVec::new());
    if tcp_segment.len() != tcp_len {
        return map_net_err(NetError::NoBufferSpace);
    }
    let tcp_len = match tcp::write_tcp_segment(seg, payload, tcp_segment.as_mut_slice()) {
        Some(n) => n,
        None => return errno_i32(ERRNO_EINVAL),
    };

    let mut pkt = match PacketBuf::alloc() {
        Some(pkt) => pkt,
        None => return map_net_err(NetError::NoBufferSpace),
    };

    if pkt.append(&tcp_segment[..tcp_len]).is_err() {
        return map_net_err(NetError::NoBufferSpace);
    }

    if let Err(err) = pkt.prepend_ipv4(
        seg.tuple.local_ip,
        seg.tuple.remote_ip,
        net::IpProtocol::Tcp.as_u8(),
        tcp_len,
    ) {
        return map_net_err(err);
    }

    let dst = Ipv4Addr(seg.tuple.remote_ip);
    let src_mac = net::route::ROUTE_TABLE
        .lookup(dst)
        .and_then(|(dev, _)| net::DEVICE_REGISTRY.mac_by_index(dev))
        .unwrap_or(net::types::MacAddr::ZERO);
    if let Err(err) = pkt.prepend_eth(src_mac.0, [0; 6]) {
        return map_net_err(err);
    }
    pkt.set_ipv4_offsets();

    match net::ipv4::send(Ipv4Addr(seg.tuple.remote_ip), pkt) {
        Ok(()) => 0,
        Err(err) => map_net_err(err),
    }
}

/// True NIC-DMA transmit of one TCP segment whose payload lives in pinned user
/// pages (TCP `MSG_ZEROCOPY`). Builds the Eth/IPv4/TCP headers, offloads the TCP
/// checksum to the device (pseudo-header seed + `csum_start`/`csum_offset`), and
/// DMAs the payload straight from `z`'s pages — re-DMA-safe across retransmits
/// (the driver holds an independent refcount on the pages until reclaim; the
/// send-queue chunk holds the data until ACK). On any ineligibility (cold
/// neighbor, no checksum offload, too many SG runs, oversize/loopback) or device
/// rejection it copies the segment's bytes from the pin into `scratch` and sends
/// them the ordinary way. Returns `0` on success or a negated errno (mirrors
/// [`socket_send_tcp_segment`]); the caller treats a nonzero result as a drain
/// stop (the chunk is queued, so the RTO retransmits).
fn socket_send_tcp_segment_zerocopy(
    seg: &TcpOutSegment,
    z: tcp::ZcSource,
    scratch: &mut [u8],
) -> i32 {
    use net::netdev::{CsumOffload, NetDeviceFeatures};

    let len = z.len;
    let local_ip = seg.tuple.local_ip;
    let dst_ip = seg.tuple.remote_ip;
    let dst = Ipv4Addr(dst_ip);

    // Eligibility — none of these consume `z` (so the copy fallback stays valid).
    let runs = coalesce_io_runs(z.keepalive.as_slice(), z.byte_start, len);
    let route = if len > 0
        && len <= TCP_TX_MAX
        && !dst.is_loopback()
        && !dst.is_broadcast()
        && !dst.is_multicast()
        && !runs.is_empty()
        && runs.len() <= 3
    {
        net::route::ROUTE_TABLE.lookup(dst)
    } else {
        None
    };

    if let Some((dev, next_hop)) = route
        && !next_hop.is_loopback()
        && let Some(dst_mac) = net::neighbor::NEIGHBOR_CACHE.lookup(dev, next_hop)
        && matches!(
            net::DEVICE_REGISTRY.features_by_index(dev),
            Some(f) if f.contains(NetDeviceFeatures::CHECKSUM_TX)
        )
        && let Some(src_mac) = net::DEVICE_REGISTRY.mac_by_index(dev)
    {
        // TCP header + options into a temp; patch the checksum field with the
        // pseudo-header seed (the device sums [csum_start..end] = TCP header +
        // DMA'd payload and completes it — NEEDS_CSUM).
        let mut tcp_hdr = [0u8; 60];
        if let Some(tcp_hdr_len) = tcp::write_tcp_segment(seg, &[], &mut tcp_hdr) {
            let tcp_total = tcp_hdr_len + len;
            let seed = net::checksum::pseudo_header_seed(
                local_ip,
                dst_ip,
                net::IpProtocol::Tcp.as_u8(),
                tcp_total,
            );
            tcp_hdr[16..18].copy_from_slice(&seed.to_be_bytes());

            let ip_total = net::IPV4_HEADER_LEN + tcp_total;
            let mut hdr = [0u8; net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + 60];
            let hlen = net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + tcp_hdr_len;
            hdr[0..6].copy_from_slice(&dst_mac.0);
            hdr[6..12].copy_from_slice(&src_mac.0);
            hdr[12..14].copy_from_slice(&net::EtherType::Ipv4.to_be_bytes());
            {
                let ip = &mut hdr[net::ETH_HEADER_LEN..net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN];
                ip[0] = 0x45;
                ip[1] = 0;
                ip[2..4].copy_from_slice(&(ip_total as u16).to_be_bytes());
                ip[4..8].copy_from_slice(&[0; 4]);
                ip[8] = 64;
                ip[9] = net::IpProtocol::Tcp.as_u8();
                ip[10..12].copy_from_slice(&[0; 2]);
                ip[12..16].copy_from_slice(&local_ip);
                ip[16..20].copy_from_slice(&dst_ip);
                let ip_csum = net::checksum::internet_checksum(ip);
                ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());
            }
            hdr[net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN..hlen]
                .copy_from_slice(&tcp_hdr[..tcp_hdr_len]);

            let csum = CsumOffload {
                csum_start: (net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN) as u16,
                csum_offset: 16,
            };
            // Independent keepalive clone for the driver TX slot (survives a
            // teardown mid-DMA); `z.keepalive` stays owned for the copy fallback.
            if let Some(driver_ka) = redup_frames(z.keepalive.as_slice()) {
                match net::DEVICE_REGISTRY.tx_zerocopy_notif_by_index(
                    dev,
                    &hdr[..hlen],
                    &runs,
                    Some(csum),
                    driver_ka,
                    z.token.clone(),
                ) {
                    Ok(()) => return 0,
                    // Device rejected (ring full / oversize): fall through to the
                    // copy fallback, which sends (or surfaces the device error).
                    Err(_) => {}
                }
            }
        }
    }

    // Copy fallback: read the segment straight from the pinned pages and send it
    // the ordinary way (cold neighbor / ineligible / device rejected).
    if len > scratch.len()
        || copy_out_frames(z.keepalive.as_slice(), z.byte_start, &mut scratch[..len]).is_err()
    {
        return errno_i32(ERRNO_EIO);
    }
    socket_send_tcp_segment(seg, &scratch[..len])
}

/// Outcome of a signal-interruptible socket wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SockWait {
    /// Predicate fired before timeout or signal.
    Ready,
    /// `timeout_ms > 0` elapsed without the predicate firing.
    Timeout,
    /// A signal is pending against the current task; abort the syscall
    /// and let the dispatcher deliver it.
    Signal,
}

/// Block on `wq` until `pred()` returns true, returning early on
/// pending signal so the syscall surfaces `EINTR` instead of stalling
/// up to the full timeout.
///
/// **IRQ-driven RX contract.** The threaded NAPI kthread
/// (`drivers/src/virtio_net.rs::napi_thread_entry`) runs at
/// `TaskPriority::KernelIo` and parks on `NAPI_WAKER`, woken from
/// the NIC IRQ handler. Every RX packet committed to the virtio
/// used ring reaches `tcp::input` / `socket_deliver_*` on an IRQ
/// boundary regardless of the parked user task. The local-CPU
/// preempt-pending path (`sched/src/scheduler.rs::schedule_task`)
/// hands the kthread the CPU on IRQ exit when it outranks the
/// running task; the lost-wakeup edge is closed by the post-burst
/// `has_pending_rx` recheck and the `NapiWaker`'s armed-bit. Phase 2
/// retired the synchronous-kick safety net the predicate held during
/// Phase 1 — the kthread alone is the RX cadence.
///
/// The predicate is augmented with a `has_pending_signal()` probe
/// so `wait_event{,_timeout}` short-circuits as soon as a `kill()`
/// queues a signal (the kill path also calls `unblock_task` which
/// wakes us synchronously). Both arms re-check
/// `has_pending_signal()` after wake to disambiguate "signal woke
/// us" from "data woke us"/"timeout expired".
fn wait_socket_event<F: FnMut() -> bool>(
    ev: KernelEvent,
    mut pred: F,
    timeout_ms: u64,
) -> SockWait {
    if slopos_kernel_services::driver_runtime::has_pending_signal() {
        return SockWait::Signal;
    }
    let mut predicate = || {
        // Sync-drain inside the wake-up predicate. The IRQ-driven
        // netpoll kthread is the primary RX cadence, but the
        // current virtio-net MSI-X configuration shows post-probe
        // IRQ-delivery gaps that have not yet been root-caused. Until
        // the driver-level fix lands, every wait predicate runs one
        // synchronous drain burst on the caller's CPU so a wake
        // observes the most recent committed used-ring state. The
        // kick is a no-op when no NIC driver is registered.
        // Allowlisted in `scripts/check_wait_predicate_purity.sh`.
        crate::napi::kick();
        pred()
    };
    let sub = BUS.subscribe(ev);
    let observed = if timeout_ms > 0 {
        sub.wait_event_interruptible_timeout(&mut predicate, timeout_ms)
    } else {
        sub.wait_event_interruptible(&mut predicate)
    };
    match observed {
        Ok(()) => SockWait::Ready,
        Err(WaitAbort::Killed | WaitAbort::Interrupted) => SockWait::Signal,
        Err(WaitAbort::Timeout | WaitAbort::NoRuntime) => SockWait::Timeout,
    }
}

fn socket_tcp_conn_id(sock: &Socket) -> Option<tcp::ConnId> {
    match &sock.inner {
        SocketInner::Tcp(tcp) => tcp.conn_id,
        _ => None,
    }
}

fn socket_is_udp(sock: &Socket) -> bool {
    matches!(sock.inner, SocketInner::Udp(_))
}

fn socket_is_icmp(sock: &Socket) -> bool {
    matches!(sock.inner, SocketInner::Icmp(_))
}

/// `true` iff `sock_idx` is an AF_INET TCP socket. Lets the ring's `OP_SEND_ZC`
/// dispatch route to the TCP `MSG_ZEROCOPY` send-queue path (which holds the
/// pinned pages across retransmits) instead of the UDP/ICMP one-shot NIC-DMA leaf.
pub fn socket_is_tcp(sock_idx: u32) -> bool {
    let table = NEW_SOCKET_TABLE.lock();
    table
        .get(sock_idx as usize)
        .map(|s| matches!(s.inner, SocketInner::Tcp(_)))
        .unwrap_or(false)
}

fn socket_notify_tcp_idx_waiters(tcp_idx: tcp::ConnId) {
    let table = NEW_SOCKET_TABLE.lock();
    for (idx, slot) in table.slots.iter().enumerate() {
        let Some(slot) = slot.as_ref() else {
            continue;
        };
        if socket_tcp_conn_id(slot) != Some(tcp_idx) {
            continue;
        }
        let idx = idx as u32;
        if tcp::recv_available(tcp_idx) > 0 || tcp::is_peer_closed(tcp_idx) {
            BUS.publish(sock_recv_ev(idx));
        }
        if tcp::send_buffer_space(tcp_idx) > 0 {
            BUS.publish(sock_send_ev(idx));
        }
        if !matches!(
            tcp::get_state(tcp_idx),
            Some(TcpState::Established | TcpState::CloseWait)
        ) {
            BUS.publish(sock_recv_ev(idx));
            BUS.publish(sock_send_ev(idx));
        }
    }
}

fn socket_notify_accept_waiters() {
    let mut table = NEW_SOCKET_TABLE.lock();
    for (idx, slot) in table.slots.iter_mut().enumerate() {
        let Some(sock) = slot.as_mut() else {
            continue;
        };
        if sock.state != SocketState::Listening {
            continue;
        }
        // Check accept queue in TcpListenState.
        let has_pending = if let SocketInner::Tcp(ref tcp_inner) = sock.inner {
            tcp_inner
                .listen
                .as_ref()
                .map(|ls| ls.accept_queue_len() > 0)
                .unwrap_or(false)
        } else {
            false
        };
        if has_pending {
            BUS.publish(sock_accept_ev(idx as u32));
        }
    }
}

pub fn socket_notify_tcp_activity(actions: &tcp::Actions) {
    if let Some(conn_id) = actions.conn_id {
        socket_notify_tcp_idx_waiters(conn_id);

        // Wire completed 3WHS into the listener's accept queue.
        // The child PCB inherits the parent listener's socket_id at
        // install time, so we read it directly instead of looking up
        // the old TCP_DEMUX table.
        if actions.notify.contains(tcp::SocketNotify::NEW_ESTABLISHED) {
            if let Some(tuple) = tcp::with_pcb(conn_id, |pcb| pcb.tuple) {
                // Passive connectivity evidence: a completed handshake with an
                // off-link peer is proof the path beyond the gateway works,
                // and it cost no packet of its own. Atomics only, so it is
                // safe here despite the locks this path takes around it.
                crate::connectivity::note_tcp_established(crate::types::Ipv4Addr(tuple.remote_ip));
                let listener_sock_idx =
                    tcp::with_pcb(conn_id, |pcb| pcb.socket_id.map(|s| s.0)).flatten();
                if let Some(listener_idx) = listener_sock_idx {
                    let accepted_meta = tcp::with_pcb(conn_id, |pcb| {
                        let tcp::PcbState::Data(d) = &pcb.state else {
                            return None;
                        };
                        Some(tcp_listener::AcceptedConn {
                            tuple: pcb.tuple,
                            iss: d.iss.raw(),
                            irs: d.irs.raw(),
                            peer_mss: d.peer_mss,
                            sack_permitted: d.sack_permitted,
                            peer_tsval: None,
                        })
                    })
                    .flatten();
                    if let Some(accepted) = accepted_meta {
                        let mut table = NEW_SOCKET_TABLE.lock();
                        if let Some(listener_sock) = table.get_mut(listener_idx as usize)
                            && listener_sock.state == SocketState::Listening
                            && let SocketInner::Tcp(ref mut tcp_inner) = listener_sock.inner
                            && let Some(ref mut listen_state) = tcp_inner.listen
                        {
                            listen_state.push_accepted(accepted);
                        }
                    }
                }
            }
        }
    }
    if actions.accepted.is_some() || actions.notify.contains(tcp::SocketNotify::NEW_ESTABLISHED) {
        socket_notify_accept_waiters();
    }
}

fn sync_socket_state(sock: &mut Socket) {
    if let Some(id) = socket_tcp_conn_id(sock) {
        use tcp::ObservedSocketState;
        match tcp::with_pcb(id, |pcb| pcb.state.observed_socket_state()) {
            Some(ObservedSocketState::Listening) => {}
            Some(ObservedSocketState::Connecting) => sock.state = SocketState::Connecting,
            Some(ObservedSocketState::Connected) => sock.state = SocketState::Connected,
            Some(ObservedSocketState::Closed) | None => sock.state = SocketState::Closed,
        }
    }
}

pub fn socket_deliver_udp(sock_idx: u32, src_ip: [u8; 4], src_port: u16, payload: &[u8]) {
    let packet = match PacketBuf::from_raw_copy(payload) {
        Some(pkt) => pkt,
        None => return,
    };

    let mut should_wake = false;
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return;
        };
        if !socket_is_udp(sock) {
            return;
        }
        let src = SockAddr::new(Ipv4Addr(src_ip), Port(src_port));
        if sock.recv_queue.push((packet, src)) {
            should_wake = true;
        }
    }

    if should_wake {
        BUS.publish(sock_recv_ev(sock_idx));
    }
}

pub fn socket_deliver_icmp(sock_idx: u32, src_ip: [u8; 4], icmp_message: &[u8]) {
    let packet = match PacketBuf::from_raw_copy(icmp_message) {
        Some(pkt) => pkt,
        None => return,
    };

    let mut should_wake = false;
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return;
        };
        if !socket_is_icmp(sock) {
            return;
        }
        let src = SockAddr::new(Ipv4Addr(src_ip), Port(0));
        if sock.recv_queue.push((packet, src)) {
            should_wake = true;
        }
    }

    if should_wake {
        BUS.publish(sock_recv_ev(sock_idx));
    }
}

pub fn socket_deliver_udp_from_dispatch(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) {
    let mut exact = None;
    let mut wildcard = None;
    {
        let table = NEW_SOCKET_TABLE.lock();
        for (idx, sock) in table.slots.iter().enumerate() {
            let Some(sock) = sock else {
                continue;
            };
            if !socket_is_udp(sock) {
                continue;
            }
            if !matches!(sock.state, SocketState::Bound | SocketState::Connected) {
                continue;
            }
            let Some(local) = sock.local_addr else {
                continue;
            };
            if local.port.0 != dst_port {
                continue;
            }

            if local.ip.0 == dst_ip {
                exact = Some(idx as u32);
                break;
            }
            if local.ip == Ipv4Addr::UNSPECIFIED {
                wildcard = Some(idx as u32);
            }
        }
    }

    if let Some(sock_idx) = exact.or(wildcard) {
        socket_deliver_udp(sock_idx, src_ip, src_port, payload);
    }
}

pub fn socket_create(domain: u16, sock_type: u16, protocol: u16, owner: SocketOwner) -> i32 {
    if domain != AF_INET {
        return errno_i32(ERRNO_EAFNOSUPPORT);
    }

    let inner = match sock_type {
        SOCK_DGRAM => {
            if protocol == IPPROTO_ICMP {
                SocketInner::Icmp(IcmpSocketInner { identifier: 0 })
            } else {
                SocketInner::Udp(UdpSocketInner)
            }
        }
        SOCK_STREAM => SocketInner::Tcp(TcpSocketInner {
            conn_id: None,
            listen: None,
        }),
        _ => return errno_i32(ERRNO_EPROTONOSUPPORT),
    };

    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(idx) = table.alloc(inner, owner) else {
        return errno_i32(ERRNO_ENOMEM);
    };
    if let Some(sock) = table.get_mut(idx) {
        sock.recv_queue.clear();
        sock.set_nonblocking(false);
    }
    idx as i32
}

/// Send `payload` to `dst_ip:dst_port`.
///
/// `payload` is a kernel staging buffer — every caller stages user bytes
/// through one before calling, so no user address reaches this function and
/// the bytes cannot change under it.
pub fn socket_sendto(sock_idx: u32, payload: &[u8], dst_ip: [u8; 4], dst_port: u16) -> i64 {
    let len = payload.len();
    if len > UDP_DGRAM_MAX_PAYLOAD {
        return errno_i32(ERRNO_EINVAL) as i64;
    }

    let mut auto_bind_udp: Option<(SockAddr, bool)> = None;
    let mut auto_bind_icmp: Option<(u16, bool)> = None;
    let (local, is_udp, identifier) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        let is_udp = socket_is_udp(sock);
        let is_icmp = socket_is_icmp(sock);
        if !is_udp && !is_icmp {
            return errno_i32(ERRNO_EPROTONOSUPPORT) as i64;
        }
        if is_udp && dst_port == 0 {
            return errno_i32(ERRNO_EDESTADDRREQ) as i64;
        }
        if sock.is_write_shutdown() {
            return errno_i32(ERRNO_EPIPE) as i64;
        }

        let local = if sock.local_addr.is_none()
            || sock.local_addr.map(|a| a.port.0 == 0).unwrap_or(true)
        {
            let Some(port) = alloc_ephemeral_port() else {
                return errno_i32(ERRNO_ENOMEM) as i64;
            };
            let local_ip = crate::iface::source_ip_for(Ipv4Addr(dst_ip))
                .map(|ip| ip.0)
                .unwrap_or([0; 4]);
            let bind_addr = SockAddr::new(Ipv4Addr(local_ip), port);
            sock.local_addr = Some(bind_addr);
            if sock.state == SocketState::Unbound {
                sock.state = SocketState::Bound;
            }
            if is_udp {
                auto_bind_udp = Some((bind_addr, sock.options.reuse_addr));
            } else {
                auto_bind_icmp = Some((bind_addr.port.0, sock.options.reuse_addr));
                if let SocketInner::Icmp(icmp) = &mut sock.inner {
                    icmp.identifier = bind_addr.port.0;
                }
            }
            bind_addr
        } else {
            sock.local_addr.unwrap()
        };

        let identifier = if let SocketInner::Icmp(icmp) = &mut sock.inner {
            if icmp.identifier == 0 {
                icmp.identifier = local.port.0;
            }
            icmp.identifier
        } else {
            0
        };

        (local, is_udp, identifier)
    };

    if let Some((bind_addr, reuse_addr)) = auto_bind_udp
        && let Err(err) = crate::udp::udp_bind(sock_idx, bind_addr.ip, bind_addr.port, reuse_addr)
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        if let Some(sock) = table.get_mut(sock_idx as usize)
            && socket_is_udp(sock)
            && sock.local_addr == Some(bind_addr)
            && sock.state == SocketState::Bound
        {
            sock.local_addr = None;
            sock.state = SocketState::Unbound;
        }
        EPHEMERAL_PORTS.lock().release(bind_addr.port);
        return map_net_err(err) as i64;
    }

    if let Some((identifier, reuse_addr)) = auto_bind_icmp
        && let Err(err) = crate::icmp::icmp_bind(sock_idx, identifier, reuse_addr)
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        if let Some(sock) = table.get_mut(sock_idx as usize)
            && socket_is_icmp(sock)
            && sock
                .local_addr
                .map(|a| a.port.0 == identifier)
                .unwrap_or(false)
            && sock.state == SocketState::Bound
        {
            sock.local_addr = None;
            sock.state = SocketState::Unbound;
            if let SocketInner::Icmp(icmp) = &mut sock.inner {
                icmp.identifier = 0;
            }
        }
        EPHEMERAL_PORTS.lock().release(Port(identifier));
        return map_net_err(err) as i64;
    }

    if is_udp {
        match crate::udp::udp_sendto(local.ip.0, dst_ip, local.port.0, dst_port, payload) {
            Ok(n) => n as i64,
            Err(err) => map_net_err(err) as i64,
        }
    } else {
        // ICMP SOCK_DGRAM contract (matches Linux):
        // User buffer = [type(1)|code(1)|cksum(2)|id(2)|seq(2)|payload...]
        // Kernel reads sequence from bytes 6-7, uses socket's bound
        // identifier, and sends the payload portion after the header.
        if payload.len() < crate::icmp::ICMP_HEADER_LEN {
            return errno_i32(ERRNO_EINVAL) as i64;
        }
        let sequence = u16::from_be_bytes([payload[6], payload[7]]);
        let icmp_payload = &payload[crate::icmp::ICMP_HEADER_LEN..];
        match crate::icmp::send_echo_request(dst_ip, identifier, sequence, icmp_payload) {
            Ok(n) => (n + crate::icmp::ICMP_HEADER_LEN) as i64,
            Err(err) => map_net_err(err) as i64,
        }
    }
}

/// Receive one datagram into `out`, reporting the sender through `src_out`.
///
/// `out` is a kernel staging buffer — every caller copies to or from user
/// memory itself, so no user address reaches this function.
pub fn socket_recvfrom(sock_idx: u32, out: &mut [u8], src_out: Option<&mut SockAddr>) -> i64 {
    let (nonblocking, timeout_ms) = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        if !socket_is_udp(sock) && !socket_is_icmp(sock) {
            return errno_i32(ERRNO_EPROTONOSUPPORT) as i64;
        }
        if sock.is_read_shutdown() {
            return 0;
        }
        (
            sock.is_nonblocking(),
            sock.options.recv_timeout.unwrap_or(0),
        )
    };

    loop {
        let packet = {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get_mut(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            };
            sock.recv_queue.pop()
        };

        if let Some((pkt, src)) = packet {
            let payload = pkt.payload();
            let copy_len = cmp::min(out.len(), payload.len());
            out[..copy_len].copy_from_slice(&payload[..copy_len]);

            if let Some(slot) = src_out {
                *slot = src;
            }
            return copy_len as i64;
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN) as i64;
        }

        match wait_socket_event(
            sock_recv_ev(sock_idx),
            || {
                let table = NEW_SOCKET_TABLE.lock();
                table
                    .get(sock_idx as usize)
                    .map(|sock| !sock.recv_queue.is_empty())
                    .unwrap_or(true)
            },
            timeout_ms,
        ) {
            SockWait::Ready => {}
            SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
            SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
        }
    }
}

pub fn socket_bind(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    let mut udp_bind_args: Option<(SockAddr, bool)> = None;
    let mut icmp_bind_args: Option<(u16, bool)> = None;
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        if sock.state != SocketState::Unbound {
            return errno_i32(ERRNO_EINVAL);
        }

        let local = SockAddr::new(Ipv4Addr(addr), Port(port));
        sock.local_addr = Some(local);
        sock.state = SocketState::Bound;

        if socket_is_udp(sock) {
            udp_bind_args = Some((local, sock.options.reuse_addr));
        } else if socket_is_icmp(sock) {
            if let SocketInner::Icmp(icmp) = &mut sock.inner {
                icmp.identifier = local.port.0;
            }
            icmp_bind_args = Some((local.port.0, sock.options.reuse_addr));
        }
    }

    if let Some((local, reuse_addr)) = udp_bind_args
        && let Err(err) = crate::udp::udp_bind(sock_idx, local.ip, local.port, reuse_addr)
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        if let Some(sock) = table.get_mut(sock_idx as usize)
            && socket_is_udp(sock)
            && sock.local_addr == Some(local)
            && sock.state == SocketState::Bound
        {
            sock.local_addr = None;
            sock.state = SocketState::Unbound;
        }
        return map_net_err(err);
    }

    if let Some((identifier, reuse_addr)) = icmp_bind_args
        && let Err(err) = crate::icmp::icmp_bind(sock_idx, identifier, reuse_addr)
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        if let Some(sock) = table.get_mut(sock_idx as usize)
            && socket_is_icmp(sock)
            && sock
                .local_addr
                .map(|a| a.port.0 == identifier)
                .unwrap_or(false)
            && sock.state == SocketState::Bound
        {
            sock.local_addr = None;
            sock.state = SocketState::Unbound;
            if let SocketInner::Icmp(icmp) = &mut sock.inner {
                icmp.identifier = 0;
            }
        }
        return map_net_err(err);
    }

    0
}

pub fn socket_listen(sock_idx: u32, backlog: u32) -> i32 {
    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    let local = match sock.local_addr {
        Some(addr) => addr,
        None => return errno_i32(ERRNO_EINVAL),
    };
    if !matches!(sock.inner, SocketInner::Tcp(_)) {
        return errno_i32(ERRNO_EPROTONOSUPPORT);
    }
    if sock.state != SocketState::Bound {
        return errno_i32(ERRNO_EINVAL);
    }

    match tcp::listen(local.ip.0, local.port.0) {
        Ok(tcp_idx) => {
            if let SocketInner::Tcp(tcp_inner) = &mut sock.inner {
                tcp_inner.conn_id = Some(tcp_idx);
                // Create TcpListenState with two-queue model.
                tcp_inner.listen = Some(tcp_listener::TcpListenState::new(backlog as usize, local));
            }
            sock.state = SocketState::Listening;

            // Set bidirectional link on the connection.
            // (Listener is already registered in TCP_LISTENERS by tcp::listen.)
            tcp::set_socket_idx(tcp_idx, Some(tcp::SocketId(sock_idx)));

            0
        }
        Err(e) => map_tcp_err(e),
    }
}

pub fn socket_accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32 {
    loop {
        let (nonblocking, timeout_ms) = {
            let table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK);
            };
            if sock.state != SocketState::Listening {
                return errno_i32(ERRNO_EINVAL);
            }
            (
                sock.is_nonblocking(),
                sock.options.recv_timeout.unwrap_or(0),
            )
        };

        {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(listen_sock) = table.get_mut(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK);
            };
            // Captured before the accepted socket is allocated: `alloc` takes
            // `&mut table`, so the listener borrow cannot still be live.
            let listen_owner = listen_sock.owner;
            let listen_opts = SocketOptions {
                reuse_addr: listen_sock.options.reuse_addr,
                recv_buf_size: listen_sock.options.recv_buf_size,
                send_buf_size: listen_sock.options.send_buf_size,
                recv_timeout: listen_sock.options.recv_timeout,
                send_timeout: listen_sock.options.send_timeout,
                keepalive: listen_sock.options.keepalive,
                tcp_nodelay: listen_sock.options.tcp_nodelay,
            };
            let is_nonblocking = listen_sock.is_nonblocking();

            // Dequeue from the TcpListenState accept queue.
            let accepted = if let SocketInner::Tcp(ref mut tcp_inner) = listen_sock.inner {
                tcp_inner.listen.as_mut().and_then(|ls| ls.accept())
            } else {
                None
            };

            if let Some(accepted_conn) = accepted {
                // Find the TCP connection index for this accepted connection.
                let tcp_idx = tcp::find(&accepted_conn.tuple);

                let Some(tcp_idx) = tcp_idx else {
                    continue;
                };

                if !matches!(
                    tcp::get_state(tcp_idx),
                    Some(
                        TcpState::Established
                            | TcpState::CloseWait
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                ) {
                    continue;
                }

                // The accepted socket belongs to whoever owns the listener:
                // nobody else was ever in a position to ask for it.
                let owner = listen_owner;
                let Some(new_idx) = table.alloc(
                    SocketInner::Tcp(TcpSocketInner {
                        conn_id: Some(tcp_idx),
                        listen: None,
                    }),
                    owner,
                ) else {
                    return errno_i32(ERRNO_ENOMEM);
                };

                let Some(sock) = table.get_mut(new_idx) else {
                    return errno_i32(ERRNO_ENOMEM);
                };
                sock.state = SocketState::Connected;
                sock.local_addr = Some(SockAddr::new(
                    Ipv4Addr(accepted_conn.tuple.local_ip),
                    Port(accepted_conn.tuple.local_port),
                ));
                sock.remote_addr = Some(SockAddr::new(
                    Ipv4Addr(accepted_conn.tuple.remote_ip),
                    Port(accepted_conn.tuple.remote_port),
                ));
                sock.options = listen_opts;
                sock.set_nonblocking(is_nonblocking);

                slopos_ostd::util::ptr_buf::write_if_non_null(
                    peer_addr,
                    accepted_conn.tuple.remote_ip,
                );
                slopos_ostd::util::ptr_buf::write_if_non_null(
                    peer_port,
                    accepted_conn.tuple.remote_port,
                );

                // Set bidirectional socket↔connection link.
                tcp::set_socket_idx(tcp_idx, Some(tcp::SocketId(new_idx as u32)));

                return new_idx as i32;
            }
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN);
        }

        // Wait for accept queue to become non-empty.
        match wait_socket_event(
            sock_accept_ev(sock_idx),
            || {
                let table = NEW_SOCKET_TABLE.lock();
                let Some(sock) = table.get(sock_idx as usize) else {
                    return true;
                };
                if let SocketInner::Tcp(ref tcp_inner) = sock.inner {
                    tcp_inner
                        .listen
                        .as_ref()
                        .map(|ls| ls.accept_queue_len() > 0)
                        .unwrap_or(false)
                } else {
                    true
                }
            },
            timeout_ms,
        ) {
            SockWait::Ready => {}
            SockWait::Timeout => return errno_i32(ERRNO_EAGAIN),
            SockWait::Signal => return errno_i32(ERRNO_EINTR),
        }
    }
}

/// How a socket family handles `connect`.
enum ConnectFamily {
    /// TCP: a SYN handshake that may complete later.
    Tcp,
    /// UDP/ICMP: "connect" just records the peer and completes inline.
    Datagram,
    /// Raw/Unix: connect is not supported via this entry point.
    Unsupported,
}

fn socket_connect_family(inner: &SocketInner) -> ConnectFamily {
    match inner {
        SocketInner::Tcp(_) => ConnectFamily::Tcp,
        SocketInner::Udp(_) | SocketInner::Icmp(_) => ConnectFamily::Datagram,
        SocketInner::Raw(_) | SocketInner::Unix(_) => ConnectFamily::Unsupported,
    }
}

/// Run the locked half of a fresh TCP connect: resolve the local IP, allocate
/// the ephemeral port + PCB via [`tcp::connect`], stamp the socket's
/// local/remote address, conn id, and `Connecting` state, and return the data
/// the caller needs to emit the SYN **after dropping the table lock** (the RX
/// path also takes the table lock, so sending under it would deadlock). The
/// caller owns the `EISCONN`-on-already-connecting guard. Shared by the blocking
/// [`socket_connect`] and the non-blocking [`socket_connect_nonblock`].
fn connect_initiate_tcp_locked(
    sock: &mut Socket,
    sock_idx: u32,
    addr: [u8; 4],
    port: u16,
) -> Result<(tcp::ConnId, bool, TcpOutSegment), i32> {
    let local_ip = sock.local_addr.map(|a| a.ip.0).unwrap_or_else(|| {
        crate::iface::source_ip_for(Ipv4Addr(addr))
            .map(|ip| ip.0)
            .unwrap_or([0; 4])
    });

    match tcp::connect(local_ip, addr, port) {
        Ok((tcp_idx, syn)) => {
            sock.local_addr = Some(SockAddr::new(
                Ipv4Addr(syn.tuple.local_ip),
                Port(syn.tuple.local_port),
            ));
            sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
            if let SocketInner::Tcp(tcp_inner) = &mut sock.inner {
                tcp_inner.conn_id = Some(tcp_idx);
            }
            sock.state = SocketState::Connecting;
            tcp::set_socket_idx(tcp_idx, Some(tcp::SocketId(sock_idx)));
            let nb = sock.is_nonblocking();
            Ok((tcp_idx, nb, syn))
        }
        Err(e) => Err(map_tcp_err(e)),
    }
}

pub fn socket_connect(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    let (tcp_idx, nonblocking, syn_seg) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        match socket_connect_family(&sock.inner) {
            ConnectFamily::Tcp => {
                if matches!(sock.state, SocketState::Connected | SocketState::Connecting) {
                    return errno_i32(ERRNO_EISCONN);
                }
                match connect_initiate_tcp_locked(sock, sock_idx, addr, port) {
                    Ok(v) => v,
                    Err(rc) => return rc,
                }
            }
            ConnectFamily::Datagram => {
                sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
                sock.state = SocketState::Connected;
                return 0;
            }
            ConnectFamily::Unsupported => return errno_i32(ERRNO_EPROTONOSUPPORT),
        }
    };
    // Table lock released — send the SYN without holding the socket table lock,
    // so the NAPI RX path can call socket_notify_tcp_activity without deadlocking.
    let send_rc = socket_send_tcp_segment(&syn_seg, &[]);
    if send_rc != 0 {
        let _ = tcp::abort(tcp_idx);
        return send_rc;
    }

    if nonblocking {
        return errno_i32(ERRNO_EINPROGRESS);
    }

    let deadline_ms = slopos_kernel_services::clock::uptime_ms().saturating_add(30_000);

    loop {
        if slopos_kernel_services::driver_runtime::current_task_wait_aborted() {
            let _ = tcp::abort(tcp_idx);
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize) {
                sock.state = SocketState::Closed;
            }
            return errno_i32(ERRNO_EINTR);
        }

        match tcp::get_state(tcp_idx) {
            Some(TcpState::Established) => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Connected;
                }
                return 0;
            }
            Some(TcpState::SynSent) => {}
            None => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Closed;
                }
                return errno_i32(ERRNO_ECONNREFUSED);
            }
            _ => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Closed;
                }
                return errno_i32(ERRNO_ECONNREFUSED);
            }
        }

        if slopos_kernel_services::clock::uptime_ms() >= deadline_ms {
            let _ = tcp::abort(tcp_idx);
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize) {
                sock.state = SocketState::Closed;
            }
            return errno_i32(ERRNO_ETIMEDOUT);
        }

        slopos_kernel_services::driver_runtime::sleep_current_task_ms(50);
        // Sync-drain on the connecting task's CPU so the next retry
        // observes the most recent committed used-ring state without
        // waiting for the netpoll kthread to be scheduled.
        crate::napi::kick();
    }
}

/// Idempotent, non-blocking connect for the ring's async connect probe.
///
/// Initiates the connection on the first call (socket `Unbound`/`Bound`) and
/// polls the handshake on every subsequent re-probe (socket `Connecting`), so it
/// is safe to call repeatedly without re-sending a SYN or re-allocating a port.
/// Returns:
///   * `0` — connected (TCP `Established`, or UDP/ICMP peer recorded);
///   * `-EAGAIN` — handshake in flight (the ring records an in-flight row and
///     re-probes); **never `-EINPROGRESS`** — the ring has no `-EINPROGRESS`
///     handling and would post it as an inline failed completion;
///   * another negated errno — a real error (`-ECONNREFUSED`, `-ENOTSOCK`, …).
///
/// Never blocks: the SYN is emitted once (outside the table lock, like
/// [`socket_connect`]) and the handshake is observed via [`tcp::get_state`].
pub fn socket_connect_nonblock(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    enum Action {
        /// First probe: emit this SYN (after dropping the lock), then defer.
        Syn(tcp::ConnId, TcpOutSegment),
        /// Re-probe: poll this connection's handshake state.
        Poll(tcp::ConnId),
    }

    let action = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        match socket_connect_family(&sock.inner) {
            ConnectFamily::Tcp => match sock.state {
                SocketState::Connected => return errno_i32(ERRNO_EISCONN),
                SocketState::Connecting => match socket_tcp_conn_id(sock) {
                    Some(id) => Action::Poll(id),
                    None => return errno_i32(ERRNO_ECONNREFUSED),
                },
                _ => match connect_initiate_tcp_locked(sock, sock_idx, addr, port) {
                    Ok((tcp_idx, _nb, syn)) => Action::Syn(tcp_idx, syn),
                    Err(rc) => return rc,
                },
            },
            ConnectFamily::Datagram => {
                sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
                sock.state = SocketState::Connected;
                return 0;
            }
            ConnectFamily::Unsupported => return errno_i32(ERRNO_EPROTONOSUPPORT),
        }
    };

    // Table lock dropped: emit the SYN / poll handshake state without holding it
    // (the NAPI RX path takes the same lock).
    match action {
        Action::Syn(tcp_idx, syn) => {
            let rc = socket_send_tcp_segment(&syn, &[]);
            if rc != 0 {
                let _ = tcp::abort(tcp_idx);
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Closed;
                }
                return rc;
            }
            errno_i32(ERRNO_EAGAIN)
        }
        Action::Poll(tcp_idx) => match tcp::get_state(tcp_idx) {
            Some(TcpState::Established) => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Connected;
                }
                0
            }
            Some(TcpState::SynSent) => errno_i32(ERRNO_EAGAIN),
            _ => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Closed;
                }
                errno_i32(ERRNO_ECONNREFUSED)
            }
        },
    }
}

/// The resolved transport target of a send, after validation + UDP/ICMP
/// auto-bind. Shared by the slice path ([`socket_send`]) and the
/// single-direct-copy pinned path ([`socket_send_pinned`]) so the load-bearing
/// auto-bind / ephemeral-port-rollback / state-check logic exists once.
enum SendTarget {
    Udp {
        local: SockAddr,
        remote: SockAddr,
    },
    Icmp {
        remote_ip: [u8; 4],
        identifier: u16,
        sequence: u16,
    },
    Tcp {
        tcp_idx: tcp::ConnId,
        nonblocking: bool,
        timeout_ms: u64,
    },
}

/// Validate the socket, perform UDP/ICMP auto-bind (with ephemeral-port
/// rollback on bind failure), and resolve the transport target. `payload_len`
/// is the would-be datagram length (for the UDP datagram-size check). On any
/// error returns the negated errno already widened to `i64`.
fn socket_send_resolve(sock_idx: u32, payload_len: usize) -> Result<SendTarget, i64> {
    let (is_udp, is_icmp) = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
        };
        if sock.is_write_shutdown() {
            return Err(errno_i32(ERRNO_EPIPE) as i64);
        }
        (socket_is_udp(sock), socket_is_icmp(sock))
    };

    if is_udp || is_icmp {
        if payload_len > UDP_DGRAM_MAX_PAYLOAD {
            return Err(errno_i32(ERRNO_EINVAL) as i64);
        }

        let mut auto_bind_udp: Option<(SockAddr, bool)> = None;
        let mut auto_bind_icmp: Option<(u16, bool)> = None;
        let (local, remote, state, identifier) = {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get_mut(sock_idx as usize) else {
                return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
            };

            if sock.local_addr.is_none() || sock.local_addr.map(|a| a.port.0 == 0).unwrap_or(true) {
                let Some(port) = alloc_ephemeral_port() else {
                    return Err(errno_i32(ERRNO_ENOMEM) as i64);
                };
                let remote_for_src = sock
                    .remote_addr
                    .map(|a| a.ip)
                    .unwrap_or(Ipv4Addr::UNSPECIFIED);
                let local_ip = crate::iface::source_ip_for(remote_for_src)
                    .map(|ip| ip.0)
                    .unwrap_or([0; 4]);
                let local = SockAddr::new(Ipv4Addr(local_ip), port);
                sock.local_addr = Some(local);
                if sock.state == SocketState::Unbound {
                    sock.state = SocketState::Bound;
                }
                if socket_is_udp(sock) {
                    auto_bind_udp = Some((local, sock.options.reuse_addr));
                } else {
                    auto_bind_icmp = Some((local.port.0, sock.options.reuse_addr));
                    if let SocketInner::Icmp(icmp) = &mut sock.inner {
                        icmp.identifier = local.port.0;
                    }
                }
            }

            let local = match sock.local_addr {
                Some(v) => v,
                None => return Err(errno_i32(ERRNO_ENOTCONN) as i64),
            };
            let remote = match sock.remote_addr {
                Some(v) => v,
                None => return Err(errno_i32(ERRNO_ENOTCONN) as i64),
            };
            let identifier = if let SocketInner::Icmp(icmp) = &mut sock.inner {
                if icmp.identifier == 0 {
                    icmp.identifier = local.port.0;
                }
                icmp.identifier
            } else {
                0
            };
            (local, remote, sock.state, identifier)
        };

        if let Some((bind_addr, reuse_addr)) = auto_bind_udp
            && let Err(err) =
                crate::udp::udp_bind(sock_idx, bind_addr.ip, bind_addr.port, reuse_addr)
        {
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize)
                && socket_is_udp(sock)
                && sock.local_addr == Some(bind_addr)
                && sock.state == SocketState::Bound
            {
                sock.local_addr = None;
                sock.state = SocketState::Unbound;
            }
            EPHEMERAL_PORTS.lock().release(bind_addr.port);
            return Err(map_net_err(err) as i64);
        }

        if let Some((identifier, reuse_addr)) = auto_bind_icmp
            && let Err(err) = crate::icmp::icmp_bind(sock_idx, identifier, reuse_addr)
        {
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize)
                && socket_is_icmp(sock)
                && sock
                    .local_addr
                    .map(|a| a.port.0 == identifier)
                    .unwrap_or(false)
                && sock.state == SocketState::Bound
            {
                sock.local_addr = None;
                sock.state = SocketState::Unbound;
                if let SocketInner::Icmp(icmp) = &mut sock.inner {
                    icmp.identifier = 0;
                }
            }
            EPHEMERAL_PORTS.lock().release(Port(identifier));
            return Err(map_net_err(err) as i64);
        }

        if state != SocketState::Connected {
            return Err(errno_i32(ERRNO_ENOTCONN) as i64);
        }

        if is_udp {
            return Ok(SendTarget::Udp { local, remote });
        }
        return Ok(SendTarget::Icmp {
            remote_ip: remote.ip.0,
            identifier,
            sequence: remote.port.0,
        });
    }

    let (tcp_idx, state, nonblocking, timeout_ms) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
        };
        sync_socket_state(sock);
        (
            socket_tcp_conn_id(sock),
            sock.state,
            sock.is_nonblocking(),
            sock.options.send_timeout.unwrap_or(0),
        )
    };

    if !matches!(state, SocketState::Connected) {
        return Err(errno_i32(ERRNO_ENOTCONN) as i64);
    }
    let Some(tcp_idx) = tcp_idx else {
        return Err(errno_i32(ERRNO_ENOTCONN) as i64);
    };
    Ok(SendTarget::Tcp {
        tcp_idx,
        nonblocking,
        timeout_ms,
    })
}

/// Drain all currently-transmittable segments for `tcp_idx` to the wire.
/// Returns `0` on success, or a negative errno (`i64`) on a segment-send or
/// scratch-alloc failure. Shared by the slice and pinned TCP send paths.
fn tcp_drain_segments(tcp_idx: tcp::ConnId) -> i64 {
    // Heap-allocate the per-segment scratch so the 1460 B buffer
    // doesn't pad this function's frame above the stack-sizes gate.
    let mut tx_payload_box = match slopos_ostd::KBox::<[u8; TCP_TX_MAX]>::zeroed() {
        Ok(b) => b,
        Err(_) => return errno_i32(ERRNO_ENOMEM) as i64,
    };
    let tx_payload: &mut [u8; TCP_TX_MAX] = &mut *tx_payload_box;
    let now_ms = slopos_kernel_services::clock::uptime_ms();
    loop {
        let Some((seg, n, zc)) = tcp::poll_transmit(tcp_idx, &mut tx_payload[..], now_ms) else {
            break;
        };
        let rc = match zc {
            None => socket_send_tcp_segment(&seg, &tx_payload[..n]),
            // Zero-copy segment: DMA straight from the pinned pages (re-DMA on
            // retransmit), copy-falling-back from them on a cold neighbor.
            Some(z) => socket_send_tcp_segment_zerocopy(&seg, z, &mut tx_payload[..]),
        };
        if rc != 0 {
            return rc as i64;
        }
    }
    0
}

/// TCP send loop, slice source: buffer `payload` (blocking on send-buffer space
/// per the socket's nonblock/timeout), then drain segments to the wire.
fn socket_send_tcp_slice(
    sock_idx: u32,
    tcp_idx: tcp::ConnId,
    nonblocking: bool,
    timeout_ms: u64,
    payload: &[u8],
) -> i64 {
    let mut total_wrote = 0usize;
    while total_wrote < payload.len() {
        let space = tcp::send_buffer_space(tcp_idx);
        if space == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            match wait_socket_event(
                sock_send_ev(sock_idx),
                || tcp::send_buffer_space(tcp_idx) > 0,
                timeout_ms,
            ) {
                SockWait::Ready => {}
                SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
                SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
            }
            continue;
        }

        let remaining = payload.len() - total_wrote;
        let chunk_len = cmp::min(space, remaining);
        let chunk = &payload[total_wrote..total_wrote + chunk_len];
        let wrote = match tcp::send(tcp_idx, chunk) {
            Ok(n) => n,
            Err(e) => {
                if total_wrote > 0 {
                    break;
                }
                return map_tcp_err_i64(e);
            }
        };

        if wrote == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            match wait_socket_event(
                sock_send_ev(sock_idx),
                || tcp::send_buffer_space(tcp_idx) > 0,
                timeout_ms,
            ) {
                SockWait::Ready => {}
                SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
                SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
            }
            continue;
        }
        total_wrote += wrote;
    }

    let drain = tcp_drain_segments(tcp_idx);
    if drain != 0 {
        return drain;
    }
    total_wrote as i64
}

/// TCP send loop, single-direct-copy source: the same blocking/space/drain
/// shape as [`socket_send_tcp_slice`], but each enqueue pulls bytes straight
/// from the pinned user pages (via `reader`) into the send ring with one
/// volatile copy — no kernel scratch.
fn socket_send_tcp_pinned(
    sock_idx: u32,
    tcp_idx: tcp::ConnId,
    nonblocking: bool,
    timeout_ms: u64,
    reader: &mut VmReader<'_>,
) -> i64 {
    let mut total_wrote = 0usize;
    while reader.has_remain() {
        let space = tcp::send_buffer_space(tcp_idx);
        if space == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            match wait_socket_event(
                sock_send_ev(sock_idx),
                || tcp::send_buffer_space(tcp_idx) > 0,
                timeout_ms,
            ) {
                SockWait::Ready => {}
                SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
                SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
            }
            continue;
        }

        let wrote = match tcp::send_from(tcp_idx, reader) {
            Ok(n) => n,
            Err(e) => {
                if total_wrote > 0 {
                    break;
                }
                return map_tcp_err_i64(e);
            }
        };

        if wrote == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            match wait_socket_event(
                sock_send_ev(sock_idx),
                || tcp::send_buffer_space(tcp_idx) > 0,
                timeout_ms,
            ) {
                SockWait::Ready => {}
                SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
                SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
            }
            continue;
        }
        total_wrote += wrote;
    }

    let drain = tcp_drain_segments(tcp_idx);
    if drain != 0 {
        return drain;
    }
    total_wrote as i64
}

/// Send `payload` on a connected socket.
///
/// `payload` is a kernel staging buffer — every caller stages user bytes
/// through one before calling, so no user address reaches this function and
/// the bytes cannot change under it.
pub fn socket_send(sock_idx: u32, payload: &[u8]) -> i64 {
    let target = match socket_send_resolve(sock_idx, payload.len()) {
        Ok(t) => t,
        Err(e) => return e,
    };

    match target {
        SendTarget::Udp { local, remote } => {
            match crate::udp::udp_sendto(
                local.ip.0,
                remote.ip.0,
                local.port.0,
                remote.port.0,
                payload,
            ) {
                Ok(n) => n as i64,
                Err(err) => map_net_err(err) as i64,
            }
        }
        SendTarget::Icmp {
            remote_ip,
            identifier,
            sequence,
        } => match crate::icmp::send_echo_request(remote_ip, identifier, sequence, payload) {
            Ok(n) => n as i64,
            Err(err) => map_net_err(err) as i64,
        },
        SendTarget::Tcp {
            tcp_idx,
            nonblocking,
            timeout_ms,
        } => socket_send_tcp_slice(sock_idx, tcp_idx, nonblocking, timeout_ms, payload),
    }
}

/// Single-direct-copy `socket_send` (SlopRing registered/provided buffers): the
/// payload is volatile-copied **once**, straight from the pinned user pages
/// (via `reader`) into the socket buffer — no kernel staging scratch. Shares
/// [`socket_send_resolve`] with the slice path so auto-bind / state handling is
/// identical.
pub fn socket_send_pinned(sock_idx: u32, reader: &mut VmReader<'_>) -> i64 {
    let target = match socket_send_resolve(sock_idx, reader.remain()) {
        Ok(t) => t,
        Err(e) => return e,
    };

    match target {
        SendTarget::Udp { local, remote } => {
            match crate::udp::udp_sendto_from(
                local.ip.0,
                remote.ip.0,
                local.port.0,
                remote.port.0,
                reader,
            ) {
                Ok(n) => n as i64,
                Err(err) => map_net_err(err) as i64,
            }
        }
        SendTarget::Icmp {
            remote_ip,
            identifier,
            sequence,
        } => match crate::icmp::send_echo_request_from(remote_ip, identifier, sequence, reader) {
            Ok(n) => n as i64,
            Err(err) => map_net_err(err) as i64,
        },
        SendTarget::Tcp {
            tcp_idx,
            nonblocking,
            timeout_ms,
        } => socket_send_tcp_pinned(sock_idx, tcp_idx, nonblocking, timeout_ms, reader),
    }
}

/// Outcome of a zero-copy send attempt (SlopRing `OP_SEND_ZC`).
pub enum ZcSendOutcome {
    /// Queued to the NIC for direct DMA from the pinned pages — `usize` payload
    /// bytes. The `SLOPRING_CQE_F_NOTIF` is **deferred** until the device
    /// reclaims the TX descriptor (the `TxReclaimToken` flips).
    Submitted(usize),
    /// Not a zero-copy candidate (TCP/unix, cold ARP, no TX checksum offload,
    /// oversized, …) — the caller falls back to the single-direct-copy leaf,
    /// which queues + drives ARP and delivers the authoritative result/error.
    NotEligible,
    /// The device TX ring is full — the ring defers and re-attempts the send.
    WouldBlock,
}

/// True NIC-DMA zero-copy `socket_send` (SlopRing `OP_SEND_ZC`). Shares
/// [`socket_send_resolve`] with the slice / single-copy paths (identical
/// auto-bind + connected-state handling), then routes connected **UDP** and
/// **ICMP echo** through the NIC-DMA leaves; every other case (TCP, unix, any
/// resolve error) is [`ZcSendOutcome::NotEligible`] so the caller uses the
/// single-copy leaf. `runs` are the coalesced pinned `(paddr, len)` physical
/// runs (summing to `total_len`); `reader` is the same pinned range as a
/// volatile cursor (used only by ICMP for its CPU-side checksum);
/// `keepalive`/`token` are handed to the driver to hold across the DMA.
pub fn socket_send_zerocopy(
    sock_idx: u32,
    runs: &[(u64, u32)],
    reader: &mut VmReader<'_>,
    total_len: usize,
    keepalive: KVec<UFrame<AnonymousMeta>>,
    token: TxReclaimToken,
) -> ZcSendOutcome {
    let target = match socket_send_resolve(sock_idx, total_len) {
        Ok(t) => t,
        Err(_) => return ZcSendOutcome::NotEligible,
    };
    match target {
        SendTarget::Udp { local, remote } => crate::udp::udp_sendto_zerocopy(
            local.ip.0,
            remote.ip.0,
            local.port.0,
            remote.port.0,
            runs,
            total_len,
            keepalive,
            token,
        ),
        SendTarget::Icmp {
            remote_ip,
            identifier,
            sequence,
        } => crate::icmp::send_echo_request_zerocopy(
            remote_ip, identifier, sequence, runs, reader, total_len, keepalive, token,
        ),
        SendTarget::Tcp { .. } => ZcSendOutcome::NotEligible,
    }
}

/// TCP `MSG_ZEROCOPY` send (SlopRing `OP_SEND_ZC` on a connected TCP socket).
/// Unlike the UDP/ICMP one-shot NIC-DMA leaf, this enqueues a zero-copy chunk
/// onto the send queue (holding the pinned pages `keepalive` — data at the pin's
/// `base_off` — and the refcounted `token`), then kicks the send pump. The bytes
/// DMA straight from the pinned pages as the congestion window allows, re-DMA on
/// retransmit, and the deferred `F_NOTIF` fires once they are cumulatively ACKed
/// and every in-flight DMA is reclaimed. Returns `Submitted` once queued, or
/// `NotEligible` (not a connected TCP socket / does not fit SO_SNDBUF) so the
/// caller uses the single-direct-copy leaf.
pub fn socket_send_zerocopy_tcp(
    sock_idx: u32,
    keepalive: KVec<UFrame<AnonymousMeta>>,
    base_off: usize,
    len: usize,
    token: ZcNotifToken,
) -> ZcSendOutcome {
    let target = match socket_send_resolve(sock_idx, len) {
        Ok(t) => t,
        Err(_) => return ZcSendOutcome::NotEligible,
    };
    let SendTarget::Tcp { tcp_idx, .. } = target else {
        return ZcSendOutcome::NotEligible;
    };
    match tcp::enqueue_zerocopy(tcp_idx, keepalive, base_off, len, token) {
        Some(n) => {
            // Kick the pump so the first segments transmit immediately; anything
            // the window/device defers follows on the ACK clock / RTO.
            let _ = tcp_drain_segments(tcp_idx);
            ZcSendOutcome::Submitted(n)
        }
        None => ZcSendOutcome::NotEligible,
    }
}

/// The resolved transport kind of a recv, after validation. Shared by the
/// slice path ([`socket_recv`]) and the single-direct-copy pinned path
/// ([`socket_recv_pinned`]).
enum RecvKind {
    /// `SHUT_RD` — return EOF (0) for both families.
    Eof,
    Udp {
        peer_filter: Option<SockAddr>,
        nonblocking: bool,
        timeout_ms: u64,
    },
    Tcp {
        tcp_idx: tcp::ConnId,
        nonblocking: bool,
        timeout_ms: u64,
    },
}

/// Validate the socket and resolve the recv kind (no data movement). On error
/// returns the negated errno widened to `i64`.
fn socket_recv_resolve(sock_idx: u32) -> Result<RecvKind, i64> {
    let (is_udp, is_icmp, is_shut_rd) = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
        };
        (
            socket_is_udp(sock),
            socket_is_icmp(sock),
            sock.is_read_shutdown(),
        )
    };

    if is_shut_rd {
        return Ok(RecvKind::Eof);
    }

    if is_udp || is_icmp {
        let (nonblocking, timeout_ms, peer_filter) = {
            let table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get(sock_idx as usize) else {
                return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
            };
            let peer = if sock.state == SocketState::Connected {
                sock.remote_addr
            } else {
                None
            };
            (
                sock.is_nonblocking(),
                sock.options.recv_timeout.unwrap_or(0),
                peer,
            )
        };
        return Ok(RecvKind::Udp {
            peer_filter,
            nonblocking,
            timeout_ms,
        });
    }

    let (tcp_idx, state, nonblocking, timeout_ms) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return Err(errno_i32(ERRNO_ENOTSOCK) as i64);
        };
        sync_socket_state(sock);
        (
            socket_tcp_conn_id(sock),
            sock.state,
            sock.is_nonblocking(),
            sock.options.recv_timeout.unwrap_or(0),
        )
    };

    if !matches!(state, SocketState::Connected | SocketState::Connecting) {
        return Err(errno_i32(ERRNO_ENOTCONN) as i64);
    }
    let Some(tcp_idx) = tcp_idx else {
        return Err(errno_i32(ERRNO_ENOTCONN) as i64);
    };
    Ok(RecvKind::Tcp {
        tcp_idx,
        nonblocking,
        timeout_ms,
    })
}

/// UDP/ICMP datagram recv loop. `deliver` copies the matched datagram payload
/// into the caller's sink (a kernel slice, or — for the single-direct-copy
/// path — straight into the pinned user pages via a `VmWriter`) and returns the
/// number of bytes delivered.
fn udp_recv_loop(
    sock_idx: u32,
    peer_filter: Option<SockAddr>,
    nonblocking: bool,
    timeout_ms: u64,
    mut deliver: impl FnMut(&[u8]) -> usize,
) -> i64 {
    loop {
        let packet = {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get_mut(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            };

            let mut found = None;
            while let Some((pkt, src)) = sock.recv_queue.pop() {
                if let Some(peer) = peer_filter
                    && src != peer
                {
                    continue;
                }
                found = Some((pkt, src));
                break;
            }
            found
        };

        if let Some((pkt, _src)) = packet {
            let payload = pkt.payload();
            return deliver(payload) as i64;
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN) as i64;
        }

        let queued = || {
            let table = NEW_SOCKET_TABLE.lock();
            table
                .get(sock_idx as usize)
                .map(|sock| !sock.recv_queue.is_empty())
                .unwrap_or(true)
        };
        let sub = BUS.subscribe(sock_recv_ev(sock_idx));
        let observed = if timeout_ms > 0 {
            sub.wait_event_interruptible_timeout(queued, timeout_ms)
        } else {
            sub.wait_event_interruptible(queued)
        };

        match observed {
            Ok(()) => {}
            Err(WaitAbort::Killed | WaitAbort::Interrupted) => {
                return errno_i32(ERRNO_EINTR) as i64;
            }
            Err(WaitAbort::Timeout | WaitAbort::NoRuntime) => {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
        }
    }
}

/// TCP stream recv loop. `recv_once` performs one drain attempt from the recv
/// ring into the caller's sink (`tcp::recv` for a kernel slice, or
/// `tcp::recv_into` straight into pinned user pages); the loop owns the
/// EOF / nonblock / wait / napi-kick policy, identical for both sinks.
fn tcp_recv_loop(
    sock_idx: u32,
    tcp_idx: tcp::ConnId,
    nonblocking: bool,
    timeout_ms: u64,
    mut recv_once: impl FnMut() -> Result<usize, TcpError>,
) -> i64 {
    loop {
        match recv_once() {
            Ok(n) => {
                if n > 0 {
                    return n as i64;
                }

                // EOF: recv buffer empty AND peer has closed their side.
                // FIN_WAIT_1/2 mean WE sent FIN (write-shutdown) but can
                // still receive — only return EOF when the peer also closed
                // (Closing, TimeWait, Closed, LastAck) or tcp reports peer FIN.
                if !matches!(
                    tcp::get_state(tcp_idx),
                    Some(
                        TcpState::Established
                            | TcpState::CloseWait
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                ) || tcp::is_peer_closed(tcp_idx)
                {
                    return 0;
                }

                if nonblocking {
                    return errno_i32(ERRNO_EAGAIN) as i64;
                }

                match wait_socket_event(
                    sock_recv_ev(sock_idx),
                    || {
                        tcp::recv_available(tcp_idx) > 0
                            || tcp::is_peer_closed(tcp_idx)
                            || !matches!(
                                tcp::get_state(tcp_idx),
                                Some(
                                    TcpState::Established
                                        | TcpState::CloseWait
                                        | TcpState::FinWait1
                                        | TcpState::FinWait2
                                )
                            )
                    },
                    timeout_ms,
                ) {
                    SockWait::Ready => {}
                    SockWait::Timeout => return errno_i32(ERRNO_EAGAIN) as i64,
                    SockWait::Signal => return errno_i32(ERRNO_EINTR) as i64,
                }
                // Sync-drain on the recv task's CPU so the post-wait
                // ring read observes the most recent committed
                // used-ring state. Resolves an IRQ-delivery edge in
                // the current driver where the kthread's drain can
                // lag the woken user task.
                crate::napi::kick();
            }
            Err(e) => return map_tcp_err_i64(e),
        }
    }
}

/// Receive into `out`, which is a kernel staging buffer — every caller copies
/// to user memory itself, so no user address reaches this function.
pub fn socket_recv(sock_idx: u32, out: &mut [u8]) -> i64 {
    match socket_recv_resolve(sock_idx) {
        Err(e) => e,
        Ok(RecvKind::Eof) => 0,
        Ok(RecvKind::Udp {
            peer_filter,
            nonblocking,
            timeout_ms,
        }) => udp_recv_loop(sock_idx, peer_filter, nonblocking, timeout_ms, |payload| {
            let copy_len = cmp::min(out.len(), payload.len());
            out[..copy_len].copy_from_slice(&payload[..copy_len]);
            copy_len
        }),
        Ok(RecvKind::Tcp {
            tcp_idx,
            nonblocking,
            timeout_ms,
        }) => tcp_recv_loop(sock_idx, tcp_idx, nonblocking, timeout_ms, || {
            tcp::recv(tcp_idx, out)
        }),
    }
}

/// Single-direct-copy `socket_recv` (SlopRing registered/provided buffers): the
/// received bytes are volatile-copied **once**, straight from the socket buffer
/// into the pinned user pages (via `writer`) — no kernel staging scratch.
/// Shares [`socket_recv_resolve`] and the recv loops with the slice path.
pub fn socket_recv_pinned(sock_idx: u32, writer: &mut VmWriter<'_>) -> i64 {
    match socket_recv_resolve(sock_idx) {
        Err(e) => e,
        Ok(RecvKind::Eof) => 0,
        Ok(RecvKind::Udp {
            peer_filter,
            nonblocking,
            timeout_ms,
        }) => udp_recv_loop(sock_idx, peer_filter, nonblocking, timeout_ms, |payload| {
            writer.write(payload)
        }),
        Ok(RecvKind::Tcp {
            tcp_idx,
            nonblocking,
            timeout_ms,
        }) => tcp_recv_loop(sock_idx, tcp_idx, nonblocking, timeout_ms, || {
            tcp::recv_into(tcp_idx, writer)
        }),
    }
}

pub fn socket_close(sock_idx: u32) -> i32 {
    let (tcp_idx, udp_unbind, icmp_unbind, _was_listener) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        let tcp_idx = socket_tcp_conn_id(sock);
        let udp_unbind = if socket_is_udp(sock) {
            sock.local_addr
        } else {
            None
        };
        let icmp_unbind = if socket_is_icmp(sock) {
            if let SocketInner::Icmp(icmp) = &sock.inner {
                Some(icmp.identifier)
            } else {
                None
            }
        } else {
            None
        };
        let was_listener = sock.state == SocketState::Listening;
        sock.recv_queue.clear();

        // Clean up TcpListenState (cancels SYN-ACK retransmit timers).
        if let SocketInner::Tcp(ref mut tcp_inner) = sock.inner {
            if let Some(ref mut listen_state) = tcp_inner.listen {
                listen_state.clear();
            }
            tcp_inner.listen = None;
        }

        table.free(sock_idx as usize);
        (tcp_idx, udp_unbind, icmp_unbind, was_listener)
    };

    // Release the TCP connection (if any).  The sharded table handles
    // cleanup internally via release(); no separate demux unregister needed.
    if let Some(tcp_idx) = tcp_idx {
        tcp::set_socket_idx(tcp_idx, None);
    }

    if let Some(local) = udp_unbind {
        crate::udp::udp_unbind(sock_idx, local.ip, local.port);
        EPHEMERAL_PORTS.lock().release(local.port);
    }

    if let Some(identifier) = icmp_unbind {
        crate::icmp::icmp_unbind(sock_idx, identifier);
        EPHEMERAL_PORTS.lock().release(Port(identifier));
    }

    BUS.publish(sock_recv_ev(sock_idx));
    BUS.publish(sock_send_ev(sock_idx));
    BUS.publish(sock_accept_ev(sock_idx));

    if let Some(tcp_idx) = tcp_idx {
        match tcp::close(tcp_idx) {
            Ok(Some(seg)) => {
                let _ = socket_send_tcp_segment(&seg, &[]);
                socket_notify_tcp_idx_waiters(tcp_idx);
                0
            }
            Ok(None) => 0,
            Err(e) => map_tcp_err(e),
        }
    } else {
        0
    }
}

pub fn socket_poll_readable(sock_idx: u32) -> u32 {
    // Sync-drain so the readiness probe observes the most recent
    // committed used-ring state. Required for the userland poll
    // path (`select`/`poll`) whose semantics demand a fresh-edge
    // readiness sample on every call.
    crate::napi::kick();

    let (state, is_datagram, tcp_idx, has_dgram_data) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return 0;
        };
        sync_socket_state(sock);
        (
            sock.state,
            socket_is_udp(sock) || socket_is_icmp(sock),
            socket_tcp_conn_id(sock),
            !sock.recv_queue.is_empty(),
        )
    };

    if state == SocketState::Listening {
        // Check accept queue in TcpListenState.
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return 0;
        };
        let has_pending = if let SocketInner::Tcp(ref tcp_inner) = sock.inner {
            tcp_inner
                .listen
                .as_ref()
                .map(|ls| ls.accept_queue_len() > 0)
                .unwrap_or(false)
        } else {
            false
        };
        if has_pending {
            return POLLIN as u32;
        }
        return 0;
    }

    if is_datagram {
        return if has_dgram_data { POLLIN as u32 } else { 0 };
    }

    let Some(tcp_idx) = tcp_idx else {
        return 0;
    };

    let mut flags = 0u32;
    let recv_available = tcp::recv_available(tcp_idx);
    if recv_available > 0 {
        flags |= POLLIN as u32;
    }

    if tcp::is_reset(tcp_idx) && recv_available == 0 {
        return (POLLERR | POLLHUP) as u32;
    }

    match tcp::get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {
            if tcp::is_peer_closed(tcp_idx) && recv_available == 0 {
                flags |= (POLLIN | POLLHUP) as u32;
            }
        }
        Some(
            TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::LastAck
            | TcpState::TimeWait,
        ) => {
            flags |= POLLHUP as u32;
        }
        None => {
            flags |= (POLLERR | POLLHUP) as u32;
        }
        _ => {}
    }

    flags
}

pub fn socket_poll_enqueue_recv(sock_idx: u32) -> bool {
    BUS.subscribe_current(sock_recv_ev(sock_idx))
}

pub fn socket_poll_dequeue_recv(sock_idx: u32) {
    BUS.unsubscribe_current(sock_recv_ev(sock_idx));
}

pub fn socket_poll_enqueue_send(sock_idx: u32) -> bool {
    BUS.subscribe_current(sock_send_ev(sock_idx))
}

pub fn socket_poll_dequeue_send(sock_idx: u32) {
    BUS.unsubscribe_current(sock_send_ev(sock_idx));
}

pub fn socket_poll_writable(sock_idx: u32) -> u32 {
    let (is_datagram, tcp_idx, state) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return 0;
        };
        sync_socket_state(sock);
        (
            socket_is_udp(sock) || socket_is_icmp(sock),
            socket_tcp_conn_id(sock),
            sock.state,
        )
    };

    if is_datagram {
        return POLLOUT as u32;
    }

    let Some(tcp_idx) = tcp_idx else {
        return 0;
    };

    let mut flags = 0u32;
    if matches!(state, SocketState::Connected) && tcp::send_buffer_space(tcp_idx) > 0 {
        flags |= POLLOUT as u32;
    }

    match tcp::get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {}
        None => {
            flags |= (POLLERR | POLLHUP) as u32;
        }
        Some(
            TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::LastAck
            | TcpState::TimeWait,
        ) => {
            flags |= POLLHUP as u32;
        }
        _ => {}
    }

    flags
}

pub fn socket_get_state(sock_idx: u32) -> Option<SocketState> {
    NEW_SOCKET_TABLE
        .lock()
        .get(sock_idx as usize)
        .map(|s| s.state)
}

pub fn socket_set_nonblocking(sock_idx: u32, nonblocking: bool) -> i32 {
    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    sock.set_nonblocking(nonblocking);
    0
}

/// Read a socket's stored non-blocking flag. Returns `None` for a stale /
/// out-of-range index. Used by the SlopRing `OP_ACCEPT` glue to restore
/// the listener's original mode after a forced-nonblocking probe.
pub fn socket_is_nonblocking(sock_idx: u32) -> Option<bool> {
    let table = NEW_SOCKET_TABLE.lock();
    table
        .get(sock_idx as usize)
        .map(|sock| sock.is_nonblocking())
}

pub fn socket_set_timeouts(sock_idx: u32, recv_timeout_ms: u64, send_timeout_ms: u64) -> i32 {
    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    sock.options.recv_timeout = if recv_timeout_ms == 0 {
        None
    } else {
        Some(recv_timeout_ms)
    };
    sock.options.send_timeout = if send_timeout_ms == 0 {
        None
    } else {
        Some(send_timeout_ms)
    };
    0
}

pub fn socket_reset_all() {
    {
        let mut table = NEW_SOCKET_TABLE.lock();
        table.init_if_needed();
        let cap = table.capacity();
        for idx in 0..cap {
            if table.get(idx).is_some() {
                BUS.publish(sock_recv_ev(idx as u32));
                BUS.publish(sock_accept_ev(idx as u32));
                BUS.publish(sock_send_ev(idx as u32));
            }
            table.free(idx);
        }
    }

    for idx in 0..MAX_SOCKETS {
        BUS.publish(sock_recv_ev(idx as u32));
        BUS.publish(sock_accept_ev(idx as u32));
        BUS.publish(sock_send_ev(idx as u32));
    }

    // Reset the allocation bitmap to match the cleared table.
    {
        let table = NEW_SOCKET_TABLE.lock();
        let mut alloc = SOCKET_ALLOC.lock();
        alloc.clear();
        alloc.set_capacity(table.capacity());
    }

    EPHEMERAL_PORTS.lock().reset();
    crate::icmp::ICMP_DEMUX.lock().clear();
    crate::udp::UDP_DEMUX.lock().clear();
    tcp::reset_all();
    crate::neighbor::NEIGHBOR_CACHE.reset();
}

#[derive(Clone, Copy)]
pub struct SocketSnapshot {
    pub state: SocketState,
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub nonblocking: bool,
}

/// How many slots the socket table currently has.
///
/// The bound a caller must size a [`collect_sockets`] buffer to. Not
/// `MAX_SOCKETS`: that is the ABI's idea of a reasonable number, while the slab
/// grows from 64 to 1024 as sockets are opened, and the two diverge on exactly
/// the busy system where the answer matters most.
pub fn socket_table_capacity() -> usize {
    NEW_SOCKET_TABLE.lock().capacity()
}

/// One row of `NET_Q_SOCKETS`, in network-domain terms.
///
/// `conn` is not part of the answer — it is how the two collection phases hand
/// a TCP connection to each other without either holding the other's lock.
#[derive(Clone, Copy)]
pub struct SocketRow {
    pub sock_idx: u32,
    pub owner: SocketOwner,
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub sock_type: u8,
    pub protocol: u8,
    pub state: u8,
    pub rx_queue: u32,
    pub tx_queue: u32,
    conn: Option<tcp::ConnId>,
}

/// Collect every socket in the table into `out`.
///
/// **Two phases, and the split is load-bearing.** `NEW_SOCKET_TABLE` is a
/// `LOCK_LEVEL_REGISTRY` lock and the TCP PCB slots are `LOCK_LEVEL_RESOURCE`;
/// resolving a connection's state from inside the table lock would nest one
/// under the other for no reason. Phase one copies what the socket table
/// itself knows and remembers the `ConnId`; phase two resolves those with the
/// table lock released. Same shape as `poll_carrier`'s read-then-announce
/// split.
///
/// `out` must be pre-allocated by the caller — nothing here allocates, because
/// phase one runs under a lock and the allocator is where every subsystem
/// meets. Rows beyond its capacity are dropped; the caller sizes it from
/// [`socket_table_capacity`].
pub fn collect_sockets(out: &mut KVec<SocketRow>) {
    {
        let table = NEW_SOCKET_TABLE.lock();
        // The table's *current* capacity, not `MAX_SOCKETS`: a constant bound
        // silently stops enumerating exactly when a busy system has the most to
        // report, and a missing row reads as "no such socket".
        let capacity = table.capacity();
        for idx in 0..capacity {
            let Some(sock) = table.get(idx) else {
                continue;
            };
            if out.len() == out.capacity() {
                break;
            }
            let local = sock
                .local_addr
                .unwrap_or(SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0)));
            let remote = sock
                .remote_addr
                .unwrap_or(SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0)));
            let (sock_type, protocol, conn) = match &sock.inner {
                SocketInner::Udp(_) => (SOCK_DGRAM as u8, 0, None),
                SocketInner::Icmp(_) => (SOCK_DGRAM as u8, IPPROTO_ICMP as u8, None),
                SocketInner::Tcp(tcp_inner) => (SOCK_STREAM as u8, 0, tcp_inner.conn_id),
                SocketInner::Raw(_) => (SOCK_RAW as u8, 0, None),
                // AF_UNIX sockets are not part of the AF_INET answer.
                SocketInner::Unix(_) => continue,
            };
            // A listening socket's state is the socket's, not a connection's:
            // its `conn_id` names the listener, and `LISTEN` is already the
            // answer.
            let state = match sock.state {
                SocketState::Listening => NET_SOCK_LISTEN,
                SocketState::Closed => NET_SOCK_CLOSED,
                _ if sock_type == SOCK_STREAM as u8 => NET_SOCK_CLOSED,
                _ => NET_SOCK_UNCONN,
            };
            let _ = out.push(SocketRow {
                sock_idx: idx as u32,
                owner: sock.owner,
                local_ip: local.ip.0,
                local_port: local.port.0,
                remote_ip: remote.ip.0,
                remote_port: remote.port.0,
                sock_type,
                protocol,
                state,
                rx_queue: sock.recv_queue.len() as u32,
                tx_queue: 0,
                conn: if sock.state == SocketState::Listening {
                    None
                } else {
                    conn
                },
            });
        }
    }

    // Phase two: the socket table lock is released. A `ConnId` may have gone
    // stale in between — `with_pcb` returns `None` for a slot whose occupant
    // changed, which leaves the row at the state phase one recorded rather
    // than reporting another connection's.
    for row in out.iter_mut() {
        let Some(conn) = row.conn else {
            continue;
        };
        if let Some(state) = tcp::table::with_pcb(conn, |pcb| pcb.state.tcp_state()) {
            row.state = tcp_state_to_abi(state);
        }
        // What `ss` calls Recv-Q and Send-Q: bytes a reader has not taken, and
        // bytes the stack has not yet handed to the network.
        if let Some((rx, tx)) = tcp::table::with_bufs(conn, |bufs| {
            (
                bufs.recv().available() as u32,
                bufs.send().buffered_len() as u32,
            )
        }) {
            row.rx_queue = rx;
            row.tx_queue = tx;
        }
    }
}

/// Map a TCP state to its `NET_SOCK_*` value.
const fn tcp_state_to_abi(state: tcp::TcpState) -> u8 {
    match state {
        tcp::TcpState::Listen => NET_SOCK_LISTEN,
        tcp::TcpState::SynSent => NET_SOCK_SYN_SENT,
        tcp::TcpState::SynReceived => NET_SOCK_SYN_RECV,
        tcp::TcpState::Established => NET_SOCK_ESTABLISHED,
        tcp::TcpState::FinWait1 => NET_SOCK_FIN_WAIT1,
        tcp::TcpState::FinWait2 => NET_SOCK_FIN_WAIT2,
        tcp::TcpState::CloseWait => NET_SOCK_CLOSE_WAIT,
        tcp::TcpState::Closing => NET_SOCK_CLOSING,
        tcp::TcpState::LastAck => NET_SOCK_LAST_ACK,
        tcp::TcpState::TimeWait => NET_SOCK_TIME_WAIT,
    }
}

pub fn socket_snapshot(sock_idx: u32) -> Option<SocketSnapshot> {
    NEW_SOCKET_TABLE.lock().get(sock_idx as usize).map(|sock| {
        let local = sock
            .local_addr
            .unwrap_or(SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0)));
        let remote = sock
            .remote_addr
            .unwrap_or(SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0)));
        SocketSnapshot {
            state: sock.state,
            local_ip: local.ip.0,
            local_port: local.port.0,
            remote_ip: remote.ip.0,
            remote_port: remote.port.0,
            nonblocking: sock.is_nonblocking(),
        }
    })
}

pub fn socket_lookup_tcp_idx(sock_idx: u32) -> Option<tcp::ConnId> {
    NEW_SOCKET_TABLE
        .lock()
        .get(sock_idx as usize)
        .and_then(socket_tcp_conn_id)
}

pub fn socket_count_active() -> usize {
    // Fast path: use the allocation bitmap instead of scanning the full table.
    SOCKET_ALLOC.lock().count_active()
}

pub fn socket_setsockopt(sock_idx: u32, level: i32, optname: i32, val: &[u8]) -> i32 {
    use slopos_abi::syscall::*;

    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    match level {
        SOL_SOCKET => match optname {
            SO_REUSEADDR => {
                if val.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                sock.options.reuse_addr = v != 0;
                0
            }
            SO_RCVBUF => {
                if val.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]) as usize;
                let Ok(size) = SocketOptions::validate_recv_buf_size(v) else {
                    return errno_i32(ERRNO_EINVAL);
                };
                if sock.recv_queue.resize(recv_queue_slots(size)).is_err() {
                    return errno_i32(ERRNO_ENOMEM);
                }
                sock.options.recv_buf_size = size;
                if let SocketInner::Tcp(tcp_inner) = &sock.inner {
                    if let Some(conn_id) = tcp_inner.conn_id {
                        tcp::set_rcvbuf(conn_id, size);
                    }
                }
                0
            }
            SO_SNDBUF => {
                if val.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]) as usize;
                let Ok(size) = SocketOptions::validate_send_buf_size(v) else {
                    return errno_i32(ERRNO_EINVAL);
                };
                sock.options.send_buf_size = size;
                if let SocketInner::Tcp(tcp_inner) = &sock.inner {
                    if let Some(conn_id) = tcp_inner.conn_id {
                        tcp::set_sndbuf(conn_id, size);
                    }
                }
                0
            }
            SO_RCVTIMEO => {
                let tv = match slopos_abi::syscall::Timeval::from_bytes(val) {
                    Some(tv) => tv,
                    None => return errno_i32(ERRNO_EINVAL),
                };
                let ms = tv.as_millis();
                sock.options.recv_timeout = if ms == 0 { None } else { Some(ms) };
                0
            }
            SO_SNDTIMEO => {
                let tv = match slopos_abi::syscall::Timeval::from_bytes(val) {
                    Some(tv) => tv,
                    None => return errno_i32(ERRNO_EINVAL),
                };
                let ms = tv.as_millis();
                sock.options.send_timeout = if ms == 0 { None } else { Some(ms) };
                0
            }
            SO_KEEPALIVE => {
                if val.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                sock.options.keepalive = v != 0;
                0
            }
            _ => errno_i32(ERRNO_EINVAL),
        },
        IPPROTO_TCP => match optname {
            TCP_NODELAY => {
                if val.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = i32::from_ne_bytes([val[0], val[1], val[2], val[3]]);
                sock.options.tcp_nodelay = v != 0;
                if let SocketInner::Tcp(tcp_inner) = &sock.inner {
                    if let Some(conn_id) = tcp_inner.conn_id {
                        tcp::set_nodelay(conn_id, v != 0);
                    }
                }
                0
            }
            _ => errno_i32(ERRNO_EINVAL),
        },
        _ => errno_i32(ERRNO_EINVAL),
    }
}

pub fn socket_getsockopt(sock_idx: u32, level: i32, optname: i32, out: &mut [u8]) -> i32 {
    use slopos_abi::syscall::*;

    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    match level {
        SOL_SOCKET => match optname {
            SO_REUSEADDR => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v: i32 = if sock.options.reuse_addr { 1 } else { 0 };
                out[..4].copy_from_slice(&v.to_ne_bytes());
                4
            }
            SO_ERROR => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let err = sock.take_pending_error().map(map_net_err).unwrap_or(0);
                out[..4].copy_from_slice(&err.to_ne_bytes());
                4
            }
            SO_RCVBUF => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = sock.options.recv_buf_size as u32;
                out[..4].copy_from_slice(&v.to_ne_bytes());
                4
            }
            SO_SNDBUF => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v = sock.options.send_buf_size as u32;
                out[..4].copy_from_slice(&v.to_ne_bytes());
                4
            }
            SO_RCVTIMEO => {
                let tv = slopos_abi::syscall::Timeval::from_millis(
                    sock.options.recv_timeout.unwrap_or(0),
                );
                if !tv.to_bytes(out) {
                    return errno_i32(ERRNO_EINVAL);
                }
                core::mem::size_of::<slopos_abi::syscall::Timeval>() as i32
            }
            SO_SNDTIMEO => {
                let tv = slopos_abi::syscall::Timeval::from_millis(
                    sock.options.send_timeout.unwrap_or(0),
                );
                if !tv.to_bytes(out) {
                    return errno_i32(ERRNO_EINVAL);
                }
                core::mem::size_of::<slopos_abi::syscall::Timeval>() as i32
            }
            SO_KEEPALIVE => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v: i32 = if sock.options.keepalive { 1 } else { 0 };
                out[..4].copy_from_slice(&v.to_ne_bytes());
                4
            }
            _ => errno_i32(ERRNO_EINVAL),
        },
        IPPROTO_TCP => match optname {
            TCP_NODELAY => {
                if out.len() < 4 {
                    return errno_i32(ERRNO_EINVAL);
                }
                let v: i32 = if sock.options.tcp_nodelay { 1 } else { 0 };
                out[..4].copy_from_slice(&v.to_ne_bytes());
                4
            }
            _ => errno_i32(ERRNO_EINVAL),
        },
        _ => errno_i32(ERRNO_EINVAL),
    }
}

pub fn socket_shutdown(sock_idx: u32, how: i32) -> i32 {
    use slopos_abi::syscall::*;

    let tcp_idx = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        let tcp_idx = socket_tcp_conn_id(sock);

        match how {
            SHUT_RD => {
                sock.flags.set(SocketFlags::SHUT_RD);
                if socket_is_udp(sock) || socket_is_icmp(sock) {
                    sock.recv_queue.clear();
                }
            }
            SHUT_WR => {
                sock.flags.set(SocketFlags::SHUT_WR);
            }
            SHUT_RDWR => {
                sock.flags.set(SocketFlags::SHUT_RD);
                sock.flags.set(SocketFlags::SHUT_WR);
                if socket_is_udp(sock) || socket_is_icmp(sock) {
                    sock.recv_queue.clear();
                }
            }
            _ => return errno_i32(ERRNO_EINVAL),
        }

        tcp_idx
    };

    // For TCP sockets, perform protocol-level shutdown actions.
    if let Some(tcp_idx) = tcp_idx {
        let shut_wr = how == SHUT_WR || how == SHUT_RDWR;
        let shut_rd = how == SHUT_RD || how == SHUT_RDWR;

        if shut_wr {
            if let Ok(Some(seg)) = tcp::shutdown_write(tcp_idx) {
                let _ = socket_send_tcp_segment(&seg, &[]);
            }
        }

        if shut_rd {
            tcp::recv_discard(tcp_idx);
            // Wake recv waiters so they see EOF.
            BUS.publish(sock_recv_ev(sock_idx));
        }
    }

    0
}

pub fn socket_send_queued(sock_idx: u32) -> i32 {
    let tcp_idx = match socket_lookup_tcp_idx(sock_idx) {
        Some(i) => i,
        None => return errno_i32(ERRNO_ENOTCONN),
    };

    let mut tx_payload_box = match slopos_ostd::KBox::<[u8; TCP_TX_MAX]>::zeroed() {
        Ok(b) => b,
        Err(_) => return errno_i32(ERRNO_ENOMEM),
    };
    let tx_payload: &mut [u8; TCP_TX_MAX] = &mut *tx_payload_box;
    let now_ms = slopos_kernel_services::clock::uptime_ms();
    loop {
        let Some((seg, n, zc)) = tcp::poll_transmit(tcp_idx, &mut tx_payload[..], now_ms) else {
            break;
        };
        let rc = match zc {
            None => socket_send_tcp_segment(&seg, &tx_payload[..n]),
            Some(z) => socket_send_tcp_segment_zerocopy(&seg, z, &mut tx_payload[..]),
        };
        if rc != 0 {
            return rc;
        }
    }
    0
}

pub fn socket_process_timers() {
    // Retransmit timers now fire exclusively via NET_TIMER_WHEEL → tcp::on_retransmit;
    // the polling path used to shadow this and was a known race hazard.
    let now_ms = slopos_kernel_services::clock::uptime_ms();
    if let Some((_idx, seg)) = tcp::delayed_ack_check(now_ms) {
        let _ = socket_send_tcp_segment(&seg, &[]);
    }
}

/// Dispatch a SYN-ACK retransmit timer to the correct listening socket.
/// Returns the SYN-ACK segment to retransmit, or None if not found.
pub fn socket_dispatch_syn_ack_retransmit(key: u32) -> Option<tcp::TcpOutSegment> {
    let mut table = NEW_SOCKET_TABLE.lock();
    for sock in table.slots.iter_mut().flatten() {
        if sock.state != SocketState::Listening {
            continue;
        }

        if let SocketInner::Tcp(ref mut tcp_inner) = sock.inner
            && let Some(ref mut listen_state) = tcp_inner.listen
            && listen_state.has_syn_entry_for_key(key)
        {
            return listen_state.on_retransmit(key);
        }
    }
    None
}

/// Public wrapper for socket_from_tcp_idx (used by timer dispatch).
pub fn socket_from_tcp_idx_pub(tcp_idx: tcp::ConnId) -> Option<u32> {
    socket_from_tcp_idx(tcp_idx)
}

pub fn socket_keepalive_enabled_by_index(sock_idx: usize) -> bool {
    let table = NEW_SOCKET_TABLE.lock();
    table
        .get(sock_idx)
        .map(|sock| sock.options.keepalive)
        .unwrap_or(false)
}

fn socket_from_tcp_idx(tcp_idx: tcp::ConnId) -> Option<u32> {
    let table = NEW_SOCKET_TABLE.lock();
    for (idx, sock) in table.slots.iter().enumerate() {
        if let Some(sock) = sock
            && socket_tcp_conn_id(sock) == Some(tcp_idx)
        {
            return Some(idx as u32);
        }
    }
    None
}

pub fn socket_debug_set_connected(sock_idx: u32, remote_ip: [u8; 4], remote_port: u16) -> i32 {
    let mut table = NEW_SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx as usize) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    let Some(tcp_idx) = socket_tcp_conn_id(sock) else {
        return errno_i32(ERRNO_ENOTCONN);
    };

    if tcp::get_state(tcp_idx) == Some(TcpState::Established) {
        sock.state = SocketState::Connected;
        sock.remote_addr = Some(SockAddr::new(Ipv4Addr(remote_ip), Port(remote_port)));
        return 0;
    }
    errno_i32(ERRNO_ENOTCONN)
}

pub fn socket_host_to_be_port(port: u16) -> u16 {
    u16::from_be_bytes(be_port(port))
}

pub fn socket_be_to_host_port(port: u16) -> u16 {
    u16::from_be(port)
}

pub fn socket_max_send_probe(sock_idx: u32, max_len: usize) -> i32 {
    let Some(tcp_idx) = socket_lookup_tcp_idx(sock_idx) else {
        return errno_i32(ERRNO_ENOTCONN);
    };
    let space = tcp::send_buffer_space(tcp_idx);
    cmp::min(space, max_len) as i32
}

/// Return the peer (remote) address for socket `sock_idx`, if connected.
pub fn socket_get_peer_addr(sock_idx: u32) -> Option<SockAddr> {
    let table = NEW_SOCKET_TABLE.lock();
    let sock = table.get(sock_idx as usize)?;
    sock.remote_addr
}

/// Return the local (bound) address for socket `sock_idx`, if bound.
pub fn socket_get_local_addr(sock_idx: u32) -> Option<SockAddr> {
    let table = NEW_SOCKET_TABLE.lock();
    let sock = table.get(sock_idx as usize)?;
    sock.local_addr
}
