extern crate alloc;

use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::packetbuf::PacketBuf;
use crate::tcp_socket;
use crate::types::{Ipv4Addr, NetError, Port, SockAddr};
use slopos_utils::{Bitmap, words_for};

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
    pub conn_id: Option<u32>,
    /// Two-queue listen state for TCP listening sockets.
    pub listen: Option<tcp_socket::TcpListenState>,
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
    slots: Vec<Option<T>>,
    head: usize,
    len: usize,
}

impl<T> BoundedQueue<T> {
    /// Create a queue with `capacity` slots.
    pub fn new(capacity: usize) -> Self {
        let slots = core::iter::repeat_with(|| None).take(capacity).collect();
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

    /// Resize queue capacity, preserving item order.
    ///
    /// If `new_capacity` is smaller than current length, oldest items are kept
    /// until capacity is reached and the rest are dropped.
    pub fn resize(&mut self, new_capacity: usize) {
        let mut drained = Vec::with_capacity(self.len);
        while let Some(item) = self.pop() {
            drained.push(item);
        }

        self.slots = core::iter::repeat_with(|| None)
            .take(new_capacity)
            .collect::<Vec<Option<T>>>();
        self.head = 0;
        self.len = 0;

        for item in drained {
            if !self.push(item) {
                break;
            }
        }
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

static SOCKET_WQ_HINT: AtomicU16 = AtomicU16::new(0);

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
    /// Owning process identifier.
    pub process_id: u32,
    /// Placeholder receive wait queue index.
    pub recv_wq_idx: u8,
    /// Placeholder accept wait queue index.
    pub accept_wq_idx: u8,
    /// Placeholder send wait queue index.
    pub send_wq_idx: u8,
}

impl Socket {
    /// Default receive queue capacity in packets.
    pub const RECV_QUEUE_DEFAULT_CAPACITY: usize = 16;

    /// Create a new socket object with defaults.
    pub fn new(inner: SocketInner) -> Self {
        let wq_idx = (SOCKET_WQ_HINT.fetch_add(1, Ordering::Relaxed) & 0x00FF) as u8;
        Self {
            inner,
            state: SocketState::Unbound,
            flags: SocketFlags::NONE,
            options: SocketOptions::new(),
            local_addr: None,
            remote_addr: None,
            recv_queue: BoundedQueue::new(Self::RECV_QUEUE_DEFAULT_CAPACITY),
            pending_error: None,
            process_id: 0,
            recv_wq_idx: wq_idx,
            accept_wq_idx: wq_idx,
            send_wq_idx: wq_idx,
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
    slots: Vec<Option<Socket>>,
    freelist: Vec<usize>,
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
            slots: Vec::new(),
            freelist: Vec::new(),
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
        let mut slots = core::iter::repeat_with(|| None)
            .take(init_cap)
            .collect::<Vec<Option<Socket>>>();
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

    /// Allocate a new socket slot.
    ///
    /// Returns the socket index on success. If no free slots are available,
    /// attempts to grow capacity (doubling, capped at `max_capacity`).
    /// Also marks the index in the allocation bitmap.
    pub fn alloc(&mut self, inner: SocketInner) -> Option<usize> {
        self.init_if_needed();
        if self.freelist.is_empty() {
            self.grow();
        }
        let idx = self.freelist.pop()?;
        self.slots[idx] = Some(Socket::new(inner));
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
                self.freelist.push(idx);
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
        self.slots.extend(
            core::iter::repeat_with(|| None)
                .take(add)
                .collect::<Vec<_>>(),
        );
        for idx in (current..new_cap).rev() {
            self.freelist.push(idx);
        }
        SOCKET_ALLOC.lock().set_capacity(new_cap);
    }
}

/// Ephemeral port allocator for dynamic local port selection.
///
/// this allocator and both old/new socket paths may use it.
/// Access must be serialized by the outer lock (no internal atomics).
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
pub static SOCKET_ALLOC: slopos_sync::IrqMutex<SocketAllocBitmap> =
    slopos_sync::IrqMutex::new(SocketAllocBitmap::new(), slopos_sync::LOCK_LEVEL_REGISTRY);

/// Global slab-based socket table.
pub static NEW_SOCKET_TABLE: slopos_sync::IrqMutex<SlabSocketTable> =
    slopos_sync::IrqMutex::new(SlabSocketTable::empty(), slopos_sync::LOCK_LEVEL_REGISTRY);

/// Ephemeral port allocator.
pub static EPHEMERAL_PORTS: slopos_sync::IrqMutex<EphemeralPortAllocator> =
    slopos_sync::IrqMutex::new(
        EphemeralPortAllocator::new(),
        slopos_sync::LOCK_LEVEL_REGISTRY,
    );

// =============================================================================
// Socket operations
// =============================================================================

use core::cmp;

use slopos_abi::net::{AF_INET, IPPROTO_ICMP, MAX_SOCKETS, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::{
    ERRNO_EADDRINUSE, ERRNO_EAFNOSUPPORT, ERRNO_EAGAIN, ERRNO_ECONNREFUSED, ERRNO_ECONNRESET,
    ERRNO_EDESTADDRREQ, ERRNO_EFAULT, ERRNO_EINPROGRESS, ERRNO_EINTR, ERRNO_EINVAL, ERRNO_EISCONN,
    ERRNO_ENETUNREACH, ERRNO_ENOMEM, ERRNO_ENOTCONN, ERRNO_ENOTSOCK, ERRNO_EPIPE,
    ERRNO_EPROTONOSUPPORT, ERRNO_ETIMEDOUT, POLLERR, POLLHUP, POLLIN, POLLOUT,
};
use slopos_sync::WaitQueue;

use crate as net;
use crate::tcp::{self, TCP_HEADER_LEN, TcpError, TcpOutSegment, TcpState};

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

static RECV_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
static ACCEPT_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
static SEND_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
}

fn map_tcp_err(err: TcpError) -> i32 {
    match err {
        TcpError::NotFound => errno_i32(ERRNO_ENOTSOCK),
        TcpError::InvalidState => errno_i32(ERRNO_ENOTCONN),
        TcpError::AddrInUse => errno_i32(ERRNO_EADDRINUSE),
        TcpError::TableFull => errno_i32(ERRNO_ENOMEM),
        TcpError::ConnectionRefused => errno_i32(ERRNO_ECONNREFUSED),
        TcpError::ConnectionReset => errno_i32(ERRNO_ECONNRESET),
        TcpError::TimedOut => errno_i32(ERRNO_EAGAIN),
        TcpError::InvalidSegment => errno_i32(ERRNO_EINVAL),
    }
}

fn map_tcp_err_i64(err: TcpError) -> i64 {
    map_tcp_err(err) as i64
}

fn map_net_err(err: NetError) -> i32 {
    match err {
        NetError::AddressInUse => errno_i32(ERRNO_EADDRINUSE),
        NetError::AddressFamilyNotSupported => errno_i32(ERRNO_EAFNOSUPPORT),
        NetError::WouldBlock => errno_i32(ERRNO_EAGAIN),
        NetError::NotConnected => errno_i32(ERRNO_ENOTCONN),
        NetError::AlreadyConnected => errno_i32(ERRNO_EISCONN),
        NetError::ProtocolNotSupported => errno_i32(ERRNO_EPROTONOSUPPORT),
        NetError::NetworkUnreachable | NetError::HostUnreachable => errno_i32(ERRNO_ENETUNREACH),
        NetError::NoBufferSpace => errno_i32(ERRNO_ENOMEM),
        NetError::Shutdown => errno_i32(ERRNO_EPIPE),
        _ => errno_i32(ERRNO_EINVAL),
    }
}

fn alloc_ephemeral_port() -> Option<Port> {
    EPHEMERAL_PORTS.lock().alloc()
}

fn be_port(port: u16) -> [u8; 2] {
    port.to_be_bytes()
}

fn write_tcp_segment(seg: &TcpOutSegment, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let has_mss = seg.mss != 0;
    let has_wscale = seg.wscale != 255;
    let mut opt_len = 0usize;
    if has_mss {
        opt_len += 4;
    }
    if has_wscale {
        // NOP + Window Scale (3 bytes) = 4 bytes for alignment
        opt_len += 4;
    }
    // Pad to 4-byte boundary
    let padded_opt_len = (opt_len + 3) & !3;
    let data_offset_words = ((TCP_HEADER_LEN + padded_opt_len) / 4) as u8;
    let tcp_len = TCP_HEADER_LEN + padded_opt_len + payload.len();
    if out.len() < tcp_len {
        return None;
    }

    let hdr = tcp::build_header(
        seg.tuple.local_port,
        seg.tuple.remote_port,
        seg.seq_num,
        seg.ack_num,
        seg.flags,
        seg.window_size,
        data_offset_words,
    );
    let hdr_len = tcp::write_header(&hdr, out)?;

    let mut opt_cursor = TCP_HEADER_LEN;
    if has_mss {
        out[opt_cursor] = tcp::TCP_OPT_MSS;
        out[opt_cursor + 1] = tcp::TCP_OPT_MSS_LEN;
        out[opt_cursor + 2..opt_cursor + 4].copy_from_slice(&seg.mss.to_be_bytes());
        opt_cursor += 4;
    }
    if has_wscale {
        out[opt_cursor] = tcp::TCP_OPT_NOP;
        out[opt_cursor + 1] = tcp::TCP_OPT_WINDOW_SCALE;
        out[opt_cursor + 2] = tcp::TCP_OPT_WINDOW_SCALE_LEN;
        out[opt_cursor + 3] = seg.wscale;
        opt_cursor += 4;
    }
    // Zero any padding
    while opt_cursor < TCP_HEADER_LEN + padded_opt_len {
        out[opt_cursor] = tcp::TCP_OPT_END;
        opt_cursor += 1;
    }

    let _ = hdr_len;
    let data_start = TCP_HEADER_LEN + padded_opt_len;
    out[data_start..data_start + payload.len()].copy_from_slice(payload);

    let checksum = tcp::tcp_checksum(seg.tuple.local_ip, seg.tuple.remote_ip, &out[..tcp_len]);
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    Some(tcp_len)
}

pub fn socket_send_tcp_segment(seg: &TcpOutSegment, payload: &[u8]) -> i32 {
    let mut opt_len = 0usize;
    if seg.mss != 0 {
        opt_len += 4;
    }
    if seg.wscale != 255 {
        opt_len += 4;
    }
    let padded_opt_len = (opt_len + 3) & !3;
    let tcp_len = TCP_HEADER_LEN + padded_opt_len + payload.len();

    let mut tcp_segment = Vec::with_capacity(tcp_len);
    tcp_segment.resize(tcp_len, 0u8);
    let tcp_len = match write_tcp_segment(seg, payload, tcp_segment.as_mut_slice()) {
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

fn wq_slot(hint: u8) -> usize {
    (hint as usize) % MAX_SOCKETS
}

fn socket_wake_recv_hint(wq_hint: u8) {
    RECV_WQS[wq_slot(wq_hint)].wake_all();
}

fn socket_wake_send_hint(wq_hint: u8) {
    SEND_WQS[wq_slot(wq_hint)].wake_all();
}

fn socket_wake_accept_hint(wq_hint: u8) {
    ACCEPT_WQS[wq_slot(wq_hint)].wake_all();
}

fn socket_tcp_conn_id(sock: &Socket) -> Option<usize> {
    match &sock.inner {
        SocketInner::Tcp(tcp) => tcp.conn_id.map(|id| id as usize),
        _ => None,
    }
}

fn socket_is_udp(sock: &Socket) -> bool {
    matches!(sock.inner, SocketInner::Udp(_))
}

fn socket_is_icmp(sock: &Socket) -> bool {
    matches!(sock.inner, SocketInner::Icmp(_))
}

fn socket_notify_tcp_idx_waiters(tcp_idx: usize) {
    let table = NEW_SOCKET_TABLE.lock();
    for slot in table.slots.iter().flatten() {
        if socket_tcp_conn_id(slot) != Some(tcp_idx) {
            continue;
        }
        if tcp::tcp_recv_available(tcp_idx) > 0 || tcp::tcp_is_peer_closed(tcp_idx) {
            socket_wake_recv_hint(slot.recv_wq_idx);
        }
        if tcp::tcp_send_buffer_space(tcp_idx) > 0 {
            socket_wake_send_hint(slot.send_wq_idx);
        }
        if !matches!(
            tcp::tcp_get_state(tcp_idx),
            Some(TcpState::Established | TcpState::CloseWait)
        ) {
            socket_wake_recv_hint(slot.recv_wq_idx);
            socket_wake_send_hint(slot.send_wq_idx);
        }
    }
}

fn socket_notify_accept_waiters() {
    let mut table = NEW_SOCKET_TABLE.lock();
    for sock in table.slots.iter_mut().flatten() {
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
            socket_wake_accept_hint(sock.accept_wq_idx);
        }
    }
}

pub fn socket_notify_tcp_activity(result: &tcp::TcpInputResult) {
    if let Some(tcp_idx) = result.conn_idx {
        socket_notify_tcp_idx_waiters(tcp_idx);

        // When a connection transitions to Established, register it
        // in the TCP demux table for fast 4-tuple lookup.
        if result.new_state == Some(TcpState::Established) {
            if let Some(conn) = tcp::tcp_get_connection(tcp_idx) {
                let _ = tcp_socket::TCP_DEMUX.lock().register_established(
                    Ipv4Addr(conn.tuple.local_ip),
                    Port(conn.tuple.local_port),
                    Ipv4Addr(conn.tuple.remote_ip),
                    Port(conn.tuple.remote_port),
                    tcp_idx as u32,
                );

                // Wire completed 3WHS into the listener's accept queue.
                // When a server-side child connection transitions SYN_RECEIVED -> Established,
                // find the parent listener and push an AcceptedConn to its accept queue.
                let listener_sock_idx = tcp_socket::TCP_DEMUX
                    .lock()
                    .lookup_listener(Ipv4Addr(conn.tuple.local_ip), Port(conn.tuple.local_port));
                if let Some(listener_idx) = listener_sock_idx {
                    let mut table = NEW_SOCKET_TABLE.lock();
                    if let Some(listener_sock) = table.get_mut(listener_idx as usize)
                        && listener_sock.state == SocketState::Listening
                        && let SocketInner::Tcp(ref mut tcp_inner) = listener_sock.inner
                        && let Some(ref mut listen_state) = tcp_inner.listen
                    {
                        let accepted = tcp_socket::AcceptedConn {
                            tuple: conn.tuple,
                            iss: conn.iss,
                            irs: conn.irs,
                            peer_mss: conn.peer_mss,
                        };
                        listen_state.push_accepted(accepted);
                    }
                }
            }
        }
    }
    if result.accepted_idx.is_some() || result.new_state == Some(TcpState::Established) {
        socket_notify_accept_waiters();
    }
}

fn sync_socket_state(sock: &mut Socket) {
    if let Some(tcp_idx) = socket_tcp_conn_id(sock) {
        match tcp::tcp_get_state(tcp_idx) {
            Some(
                TcpState::Established
                | TcpState::CloseWait
                | TcpState::FinWait1
                | TcpState::FinWait2,
            ) => {
                sock.state = SocketState::Connected;
            }
            Some(TcpState::SynSent | TcpState::SynReceived) => {
                sock.state = SocketState::Connecting;
            }
            Some(TcpState::Closed | TcpState::TimeWait | TcpState::Closing | TcpState::LastAck) => {
                sock.state = SocketState::Closed;
            }
            Some(TcpState::Listen) => {}
            None => {
                sock.state = SocketState::Closed;
            }
        }
    }
}

pub fn socket_deliver_udp(sock_idx: u32, src_ip: [u8; 4], src_port: u16, payload: &[u8]) {
    let packet = match PacketBuf::from_raw_copy(payload) {
        Some(pkt) => pkt,
        None => return,
    };

    let mut wake_hint = None;
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
            wake_hint = Some(sock.recv_wq_idx);
        }
    }

    if let Some(hint) = wake_hint {
        socket_wake_recv_hint(hint);
    }
}

pub fn socket_deliver_icmp(sock_idx: u32, src_ip: [u8; 4], icmp_message: &[u8]) {
    let packet = match PacketBuf::from_raw_copy(icmp_message) {
        Some(pkt) => pkt,
        None => return,
    };

    let mut wake_hint = None;
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
            wake_hint = Some(sock.recv_wq_idx);
        }
    }

    if let Some(hint) = wake_hint {
        socket_wake_recv_hint(hint);
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

pub fn socket_create(domain: u16, sock_type: u16, protocol: u16) -> i32 {
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
    let Some(idx) = table.alloc(inner) else {
        return errno_i32(ERRNO_ENOMEM);
    };
    if let Some(sock) = table.get_mut(idx) {
        sock.recv_queue.clear();
        sock.set_nonblocking(false);
    }
    idx as i32
}

pub fn socket_sendto(
    sock_idx: u32,
    data: *const u8,
    len: usize,
    dst_ip: [u8; 4],
    dst_port: u16,
) -> i64 {
    if data.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }
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
            let local_ip = crate::netstack::NET_STACK
                .first_ipv4()
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

    let payload = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    };

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

pub fn socket_recvfrom(
    sock_idx: u32,
    buf: *mut u8,
    len: usize,
    src_ip: *mut [u8; 4],
    src_port: *mut u16,
) -> i64 {
    if buf.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let out = if len == 0 {
        &mut [][..]
    } else {
        unsafe { core::slice::from_raw_parts_mut(buf, len) }
    };

    let (nonblocking, timeout_ms, recv_hint) = {
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
            sock.recv_wq_idx,
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

            if !src_ip.is_null() {
                unsafe {
                    *src_ip = src.ip.0;
                }
            }
            if !src_port.is_null() {
                unsafe {
                    *src_port = src.port.0;
                }
            }
            return copy_len as i64;
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN) as i64;
        }

        let wait_ok = if timeout_ms > 0 {
            RECV_WQS[wq_slot(recv_hint)].wait_event_timeout(
                || {
                    let table = NEW_SOCKET_TABLE.lock();
                    table
                        .get(sock_idx as usize)
                        .map(|sock| !sock.recv_queue.is_empty())
                        .unwrap_or(true)
                },
                timeout_ms,
            )
        } else {
            RECV_WQS[wq_slot(recv_hint)].wait_event(|| {
                let table = NEW_SOCKET_TABLE.lock();
                table
                    .get(sock_idx as usize)
                    .map(|sock| !sock.recv_queue.is_empty())
                    .unwrap_or(true)
            })
        };

        if !wait_ok {
            return errno_i32(ERRNO_EAGAIN) as i64;
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

    match tcp::tcp_listen(local.ip.0, local.port.0) {
        Ok(tcp_idx) => {
            if let SocketInner::Tcp(tcp_inner) = &mut sock.inner {
                tcp_inner.conn_id = Some(tcp_idx as u32);
                // Create TcpListenState with two-queue model.
                tcp_inner.listen = Some(tcp_socket::TcpListenState::new(backlog as usize, local));
            }
            sock.state = SocketState::Listening;

            // Register listener in TCP demux table.
            let _ = tcp_socket::TCP_DEMUX
                .lock()
                .register_listener(local.ip, local.port, sock_idx);

            // Set bidirectional link on the connection.
            tcp::tcp_set_socket_idx(tcp_idx, Some(sock_idx as usize));

            0
        }
        Err(e) => map_tcp_err(e),
    }
}

pub fn socket_accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32 {
    loop {
        let (nonblocking, timeout_ms, accept_hint) = {
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
                sock.accept_wq_idx,
            )
        };

        {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(listen_sock) = table.get_mut(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK);
            };
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
                let tcp_idx = tcp::tcp_find(&accepted_conn.tuple);

                let Some(tcp_idx) = tcp_idx else {
                    continue;
                };

                if !matches!(
                    tcp::tcp_get_state(tcp_idx),
                    Some(
                        TcpState::Established
                            | TcpState::CloseWait
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                ) {
                    continue;
                }

                let Some(new_idx) = table.alloc(SocketInner::Tcp(TcpSocketInner {
                    conn_id: Some(tcp_idx as u32),
                    listen: None,
                })) else {
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

                if !peer_addr.is_null() {
                    unsafe {
                        *peer_addr = accepted_conn.tuple.remote_ip;
                    }
                }
                if !peer_port.is_null() {
                    unsafe {
                        *peer_port = accepted_conn.tuple.remote_port;
                    }
                }

                // Set bidirectional socket↔connection link.
                tcp::tcp_set_socket_idx(tcp_idx, Some(new_idx));

                return new_idx as i32;
            }
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN);
        }

        // Wait for accept queue to become non-empty.
        let wait_ok = if timeout_ms > 0 {
            ACCEPT_WQS[wq_slot(accept_hint)].wait_event_timeout(
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
            )
        } else {
            ACCEPT_WQS[wq_slot(accept_hint)].wait_event(|| {
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
            })
        };

        if !wait_ok {
            return errno_i32(ERRNO_EAGAIN);
        }
    }
}

pub fn socket_connect(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    let (tcp_idx, nonblocking, _send_hint, syn_seg) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        match &mut sock.inner {
            SocketInner::Tcp(tcp_inner) => {
                if matches!(sock.state, SocketState::Connected | SocketState::Connecting) {
                    return errno_i32(ERRNO_EISCONN);
                }

                let local_ip = sock.local_addr.map(|a| a.ip.0).unwrap_or_else(|| {
                    crate::netstack::NET_STACK
                        .first_ipv4()
                        .map(|ip| ip.0)
                        .unwrap_or([0; 4])
                });

                match tcp::tcp_connect(local_ip, addr, port) {
                    Ok((tcp_idx, syn)) => {
                        sock.local_addr = Some(SockAddr::new(
                            Ipv4Addr(syn.tuple.local_ip),
                            Port(syn.tuple.local_port),
                        ));
                        sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
                        tcp_inner.conn_id = Some(tcp_idx as u32);
                        sock.state = SocketState::Connecting;

                        tcp::tcp_set_socket_idx(tcp_idx, Some(sock_idx as usize));

                        let nb = sock.is_nonblocking();
                        let hint = sock.send_wq_idx;
                        (tcp_idx, nb, hint, syn)
                    }
                    Err(e) => return map_tcp_err(e),
                }
            }
            SocketInner::Udp(_) => {
                sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
                sock.state = SocketState::Connected;
                return 0;
            }
            SocketInner::Icmp(_) => {
                sock.remote_addr = Some(SockAddr::new(Ipv4Addr(addr), Port(port)));
                sock.state = SocketState::Connected;
                return 0;
            }
            SocketInner::Raw(_) | SocketInner::Unix(_) => return errno_i32(ERRNO_EPROTONOSUPPORT),
        }
    };
    // Table lock released — send the SYN without holding the socket table lock,
    // so the NAPI RX path can call socket_notify_tcp_activity without deadlocking.
    let send_rc = socket_send_tcp_segment(&syn_seg, &[]);
    if send_rc != 0 {
        let _ = tcp::tcp_abort(tcp_idx);
        return send_rc;
    }

    if nonblocking {
        return errno_i32(ERRNO_EINPROGRESS);
    }

    let deadline_ms = slopos_utils::clock::uptime_ms().saturating_add(30_000);

    loop {
        if slopos_kernel_services::driver_runtime::has_pending_signal() {
            let _ = tcp::tcp_abort(tcp_idx);
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize) {
                sock.state = SocketState::Closed;
            }
            return errno_i32(ERRNO_EINTR);
        }

        match tcp::tcp_get_state(tcp_idx) {
            Some(TcpState::Established) => {
                let mut table = NEW_SOCKET_TABLE.lock();
                if let Some(sock) = table.get_mut(sock_idx as usize) {
                    sock.state = SocketState::Connected;
                }
                return 0;
            }
            Some(TcpState::SynSent) => {}
            Some(TcpState::Closed) | None => {
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

        if slopos_utils::clock::uptime_ms() >= deadline_ms {
            let _ = tcp::tcp_abort(tcp_idx);
            let mut table = NEW_SOCKET_TABLE.lock();
            if let Some(sock) = table.get_mut(sock_idx as usize) {
                sock.state = SocketState::Closed;
            }
            return errno_i32(ERRNO_ETIMEDOUT);
        }

        slopos_kernel_services::driver_runtime::sleep_current_task_ms(50);
        crate::napi::kick();
    }
}

pub fn socket_send(sock_idx: u32, data: *const u8, len: usize) -> i64 {
    if data.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let (is_udp, is_icmp) = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        if sock.is_write_shutdown() {
            return errno_i32(ERRNO_EPIPE) as i64;
        }
        (socket_is_udp(sock), socket_is_icmp(sock))
    };

    if is_udp || is_icmp {
        if len > UDP_DGRAM_MAX_PAYLOAD {
            return errno_i32(ERRNO_EINVAL) as i64;
        }

        let mut auto_bind_udp: Option<(SockAddr, bool)> = None;
        let mut auto_bind_icmp: Option<(u16, bool)> = None;
        let (local, remote, state, identifier) = {
            let mut table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get_mut(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            };

            if sock.local_addr.is_none() || sock.local_addr.map(|a| a.port.0 == 0).unwrap_or(true) {
                let Some(port) = alloc_ephemeral_port() else {
                    return errno_i32(ERRNO_ENOMEM) as i64;
                };
                let local_ip = crate::netstack::NET_STACK
                    .first_ipv4()
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
                None => return errno_i32(ERRNO_ENOTCONN) as i64,
            };
            let remote = match sock.remote_addr {
                Some(v) => v,
                None => return errno_i32(ERRNO_ENOTCONN) as i64,
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

        if state != SocketState::Connected {
            return errno_i32(ERRNO_ENOTCONN) as i64;
        }

        let payload = if len == 0 {
            &[][..]
        } else {
            unsafe { core::slice::from_raw_parts(data, len) }
        };

        if is_udp {
            return match crate::udp::udp_sendto(
                local.ip.0,
                remote.ip.0,
                local.port.0,
                remote.port.0,
                payload,
            ) {
                Ok(n) => n as i64,
                Err(err) => map_net_err(err) as i64,
            };
        }

        return match crate::icmp::send_echo_request(remote.ip.0, identifier, remote.port.0, payload)
        {
            Ok(n) => n as i64,
            Err(err) => map_net_err(err) as i64,
        };
    }

    let (tcp_idx, state, nonblocking, timeout_ms, send_hint) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sync_socket_state(sock);
        (
            socket_tcp_conn_id(sock),
            sock.state,
            sock.is_nonblocking(),
            sock.options.send_timeout.unwrap_or(0),
            sock.send_wq_idx,
        )
    };

    if !matches!(state, SocketState::Connected) {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    }
    let Some(tcp_idx) = tcp_idx else {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    };

    let payload = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    };

    let mut total_wrote = 0usize;
    while total_wrote < payload.len() {
        let space = tcp::tcp_send_buffer_space(tcp_idx);
        if space == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            let wait_ok = if timeout_ms > 0 {
                SEND_WQS[wq_slot(send_hint)]
                    .wait_event_timeout(|| tcp::tcp_send_buffer_space(tcp_idx) > 0, timeout_ms)
            } else {
                SEND_WQS[wq_slot(send_hint)].wait_event(|| tcp::tcp_send_buffer_space(tcp_idx) > 0)
            };
            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            continue;
        }

        let remaining = payload.len() - total_wrote;
        let chunk_len = cmp::min(space, remaining);
        let chunk = &payload[total_wrote..total_wrote + chunk_len];
        let wrote = match tcp::tcp_send(tcp_idx, chunk) {
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
            let wait_ok = if timeout_ms > 0 {
                SEND_WQS[wq_slot(send_hint)]
                    .wait_event_timeout(|| tcp::tcp_send_buffer_space(tcp_idx) > 0, timeout_ms)
            } else {
                SEND_WQS[wq_slot(send_hint)].wait_event(|| tcp::tcp_send_buffer_space(tcp_idx) > 0)
            };
            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            continue;
        }
        total_wrote += wrote;
    }

    let mut tx_payload = [0u8; TCP_TX_MAX];
    let now_ms = slopos_utils::clock::uptime_ms();
    loop {
        let Some((seg, n)) = tcp::tcp_poll_transmit(tcp_idx, &mut tx_payload, now_ms) else {
            break;
        };
        let rc = socket_send_tcp_segment(&seg, &tx_payload[..n]);
        if rc != 0 {
            return rc as i64;
        }
    }

    total_wrote as i64
}

pub fn socket_recv(sock_idx: u32, buf: *mut u8, len: usize) -> i64 {
    if buf.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let out = if len == 0 {
        &mut [][..]
    } else {
        unsafe { core::slice::from_raw_parts_mut(buf, len) }
    };

    let (is_udp, is_icmp, is_shut_rd) = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        (
            socket_is_udp(sock),
            socket_is_icmp(sock),
            sock.is_read_shutdown(),
        )
    };

    if is_shut_rd {
        // SHUT_RD: return EOF (0) for both UDP and TCP.
        return 0;
    }

    if is_udp || is_icmp {
        let (nonblocking, timeout_ms, recv_hint, peer_filter) = {
            let table = NEW_SOCKET_TABLE.lock();
            let Some(sock) = table.get(sock_idx as usize) else {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            };
            let peer = if sock.state == SocketState::Connected {
                sock.remote_addr
            } else {
                None
            };
            (
                sock.is_nonblocking(),
                sock.options.recv_timeout.unwrap_or(0),
                sock.recv_wq_idx,
                peer,
            )
        };

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
                let copy_len = cmp::min(out.len(), payload.len());
                out[..copy_len].copy_from_slice(&payload[..copy_len]);
                return copy_len as i64;
            }

            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }

            let wait_ok = if timeout_ms > 0 {
                RECV_WQS[wq_slot(recv_hint)].wait_event_timeout(
                    || {
                        let table = NEW_SOCKET_TABLE.lock();
                        table
                            .get(sock_idx as usize)
                            .map(|sock| !sock.recv_queue.is_empty())
                            .unwrap_or(true)
                    },
                    timeout_ms,
                )
            } else {
                RECV_WQS[wq_slot(recv_hint)].wait_event(|| {
                    let table = NEW_SOCKET_TABLE.lock();
                    table
                        .get(sock_idx as usize)
                        .map(|sock| !sock.recv_queue.is_empty())
                        .unwrap_or(true)
                })
            };

            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
        }
    }

    let (tcp_idx, state, nonblocking, timeout_ms, recv_hint) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sync_socket_state(sock);
        (
            socket_tcp_conn_id(sock),
            sock.state,
            sock.is_nonblocking(),
            sock.options.recv_timeout.unwrap_or(0),
            sock.recv_wq_idx,
        )
    };

    if !matches!(state, SocketState::Connected | SocketState::Connecting) {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    }

    let Some(tcp_idx) = tcp_idx else {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    };

    loop {
        match tcp::tcp_recv(tcp_idx, out) {
            Ok(n) => {
                if n > 0 {
                    return n as i64;
                }

                // EOF: recv buffer empty AND peer has closed their side.
                // FIN_WAIT_1/2 mean WE sent FIN (write-shutdown) but can
                // still receive — only return EOF when the peer also closed
                // (Closing, TimeWait, Closed, LastAck) or tcp reports peer FIN.
                if !matches!(
                    tcp::tcp_get_state(tcp_idx),
                    Some(
                        TcpState::Established
                            | TcpState::CloseWait
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                ) || tcp::tcp_is_peer_closed(tcp_idx)
                {
                    return 0;
                }

                if nonblocking {
                    return errno_i32(ERRNO_EAGAIN) as i64;
                }

                let wait_ok = if timeout_ms > 0 {
                    RECV_WQS[wq_slot(recv_hint)].wait_event_timeout(
                        || {
                            tcp::tcp_recv_available(tcp_idx) > 0
                                || tcp::tcp_is_peer_closed(tcp_idx)
                                || !matches!(
                                    tcp::tcp_get_state(tcp_idx),
                                    Some(
                                        TcpState::Established
                                            | TcpState::CloseWait
                                            | TcpState::FinWait1
                                            | TcpState::FinWait2
                                    )
                                )
                        },
                        timeout_ms,
                    )
                } else {
                    RECV_WQS[wq_slot(recv_hint)].wait_event(|| {
                        tcp::tcp_recv_available(tcp_idx) > 0
                            || tcp::tcp_is_peer_closed(tcp_idx)
                            || !matches!(
                                tcp::tcp_get_state(tcp_idx),
                                Some(
                                    TcpState::Established
                                        | TcpState::CloseWait
                                        | TcpState::FinWait1
                                        | TcpState::FinWait2
                                )
                            )
                    })
                };

                if !wait_ok {
                    return errno_i32(ERRNO_EAGAIN) as i64;
                }

                crate::napi::kick();
            }
            Err(e) => return map_tcp_err_i64(e),
        }
    }
}

pub fn socket_close(sock_idx: u32) -> i32 {
    let (tcp_idx, udp_unbind, icmp_unbind, recv_hint, send_hint, accept_hint, was_listener) = {
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
        let recv_hint = sock.recv_wq_idx;
        let send_hint = sock.send_wq_idx;
        let accept_hint = sock.accept_wq_idx;
        sock.recv_queue.clear();

        // Clean up TcpListenState (cancels SYN-ACK retransmit timers).
        if let SocketInner::Tcp(ref mut tcp_inner) = sock.inner {
            if let Some(ref mut listen_state) = tcp_inner.listen {
                listen_state.clear();
            }
            tcp_inner.listen = None;
        }

        table.free(sock_idx as usize);
        (
            tcp_idx,
            udp_unbind,
            icmp_unbind,
            recv_hint,
            send_hint,
            accept_hint,
            was_listener,
        )
    };

    // Unregister from TCP demux table.
    if was_listener {
        tcp_socket::TCP_DEMUX.lock().unregister_listener(sock_idx);
    }
    if let Some(tcp_idx) = tcp_idx {
        tcp_socket::TCP_DEMUX
            .lock()
            .unregister_established(tcp_idx as u32);
        // Clear the bidirectional link.
        tcp::tcp_set_socket_idx(tcp_idx, None);
    }

    if let Some(local) = udp_unbind {
        crate::udp::udp_unbind(sock_idx, local.ip, local.port);
        EPHEMERAL_PORTS.lock().release(local.port);
    }

    if let Some(identifier) = icmp_unbind {
        crate::icmp::icmp_unbind(sock_idx, identifier);
        EPHEMERAL_PORTS.lock().release(Port(identifier));
    }

    socket_wake_recv_hint(recv_hint);
    socket_wake_send_hint(send_hint);
    socket_wake_accept_hint(accept_hint);

    if let Some(tcp_idx) = tcp_idx {
        match tcp::tcp_close(tcp_idx) {
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
    slopos_kernel_services::driver_runtime::run_bottom_halves();

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
    let recv_available = tcp::tcp_recv_available(tcp_idx);
    if recv_available > 0 {
        flags |= POLLIN as u32;
    }

    if tcp::tcp_is_reset(tcp_idx) && recv_available == 0 {
        return (POLLERR | POLLHUP) as u32;
    }

    match tcp::tcp_get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {
            if tcp::tcp_is_peer_closed(tcp_idx) && recv_available == 0 {
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
        Some(TcpState::Closed) | None => {
            flags |= (POLLERR | POLLHUP) as u32;
        }
        _ => {}
    }

    flags
}

pub fn socket_poll_enqueue_recv(sock_idx: u32) -> bool {
    let wq_hint = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return false;
        };
        sock.recv_wq_idx
    };
    RECV_WQS[wq_slot(wq_hint)].enqueue_current()
}

pub fn socket_poll_dequeue_recv(sock_idx: u32) {
    let wq_hint = {
        let table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx as usize) else {
            return;
        };
        sock.recv_wq_idx
    };
    RECV_WQS[wq_slot(wq_hint)].remove_current();
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
    if matches!(state, SocketState::Connected) && tcp::tcp_send_buffer_space(tcp_idx) > 0 {
        flags |= POLLOUT as u32;
    }

    match tcp::tcp_get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {}
        Some(TcpState::Closed) | None => {
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
            if let Some(sock) = table.get(idx) {
                socket_wake_recv_hint(sock.recv_wq_idx);
                socket_wake_accept_hint(sock.accept_wq_idx);
                socket_wake_send_hint(sock.send_wq_idx);
            }
            table.free(idx);
        }
    }

    for idx in 0..MAX_SOCKETS {
        RECV_WQS[idx].wake_all();
        ACCEPT_WQS[idx].wake_all();
        SEND_WQS[idx].wake_all();
    }

    // Reset the allocation bitmap to match the cleared table.
    {
        let table = NEW_SOCKET_TABLE.lock();
        let mut alloc = SOCKET_ALLOC.lock();
        alloc.clear();
        alloc.set_capacity(table.capacity());
    }

    *EPHEMERAL_PORTS.lock() = EphemeralPortAllocator::new();
    crate::icmp::ICMP_DEMUX.lock().clear();
    crate::udp::UDP_DEMUX.lock().clear();
    tcp::tcp_reset_all();
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

pub fn socket_lookup_tcp_idx(sock_idx: u32) -> Option<usize> {
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
                sock.options.recv_buf_size = size;
                sock.recv_queue.resize(size);
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

    let (tcp_idx, recv_hint) = {
        let mut table = NEW_SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx as usize) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };

        let tcp_idx = socket_tcp_conn_id(sock);
        let recv_hint = sock.recv_wq_idx;

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

        (tcp_idx, recv_hint)
    };

    // For TCP sockets, perform protocol-level shutdown actions.
    if let Some(tcp_idx) = tcp_idx {
        let shut_wr = how == SHUT_WR || how == SHUT_RDWR;
        let shut_rd = how == SHUT_RD || how == SHUT_RDWR;

        if shut_wr {
            if let Ok(Some(seg)) = tcp::tcp_shutdown_write(tcp_idx) {
                let _ = socket_send_tcp_segment(&seg, &[]);
            }
        }

        if shut_rd {
            tcp::tcp_recv_discard(tcp_idx);
            // Wake recv waiters so they see EOF.
            socket_wake_recv_hint(recv_hint);
        }
    }

    0
}

pub fn socket_send_queued(sock_idx: u32) -> i32 {
    let tcp_idx = match socket_lookup_tcp_idx(sock_idx) {
        Some(i) => i,
        None => return errno_i32(ERRNO_ENOTCONN),
    };

    let mut tx_payload = [0u8; TCP_TX_MAX];
    let now_ms = slopos_utils::clock::uptime_ms();
    loop {
        let Some((seg, n)) = tcp::tcp_poll_transmit(tcp_idx, &mut tx_payload, now_ms) else {
            break;
        };
        let rc = socket_send_tcp_segment(&seg, &tx_payload[..n]);
        if rc != 0 {
            return rc;
        }
    }
    0
}

pub fn socket_process_timers() {
    let now_ms = slopos_utils::clock::uptime_ms();
    if let Some(idx) = tcp::tcp_retransmit_check(now_ms)
        && let Some(sock_idx) = socket_from_tcp_idx(idx)
    {
        let _ = socket_send_queued(sock_idx);
    }

    if let Some((_idx, seg)) = tcp::tcp_delayed_ack_check(now_ms) {
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
pub fn socket_from_tcp_idx_pub(tcp_idx: usize) -> Option<u32> {
    socket_from_tcp_idx(tcp_idx)
}

pub fn socket_keepalive_enabled_by_index(sock_idx: usize) -> bool {
    let table = NEW_SOCKET_TABLE.lock();
    table
        .get(sock_idx)
        .map(|sock| sock.options.keepalive)
        .unwrap_or(false)
}

fn socket_from_tcp_idx(tcp_idx: usize) -> Option<u32> {
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

    if let Some(conn) = tcp::tcp_get_connection(tcp_idx)
        && conn.state == TcpState::Established
    {
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
    let space = tcp::tcp_send_buffer_space(tcp_idx);
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
