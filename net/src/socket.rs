use core::fmt;
use slopos_fs::fileio::FdTable;

use slopos_ostd::AllocError;
use slopos_ostd::KVec;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::mm::uframe::{coalesce_io_runs, copy_out_frames};
use slopos_ostd::mm::{VmReader, VmWriter};
use slopos_ostd::write_field;
use slopos_ostd::{Bitmap, words_for};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use crate::packetbuf::PacketBuf;
use crate::tcp;
use crate::tcp::listener as tcp_listener;
use crate::types::{Ipv4Addr, NetError, Port, SockAddr};

pub enum SocketInner {
    Udp(UdpSocketInner),
    Icmp(IcmpSocketInner),
    Tcp(TcpSocketInner),
    Raw(RawSocketInner),
    Unix(UnixSocketInner),
}

pub struct UdpSocketInner;

pub struct IcmpSocketInner {
    pub identifier: u16,
}

pub struct TcpSocketInner {
    pub conn_id: Option<tcp::ConnId>,
    pub listen: Option<tcp_listener::TcpListenState>,
}

pub struct RawSocketInner;

/// AF_UNIX socket — the real state (ring buffers, wait queues) lives in
/// `unix_socket::UNIX_STATE`.
pub struct UnixSocketInner {
    pub unix_idx: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SocketFlags(u32);

impl SocketFlags {
    pub const NONE: Self = Self(0);
    pub const O_NONBLOCK: Self = Self(1 << 0);
    pub const SHUT_RD: Self = Self(1 << 1);
    pub const SHUT_WR: Self = Self(1 << 2);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn set(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    pub fn clear(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }
}

pub struct SocketOptions {
    pub reuse_addr: bool,
    pub recv_buf_size: usize,
    pub send_buf_size: usize,
    /// Receive timeout in milliseconds (`None` means infinite).
    pub recv_timeout: Option<u64>,
    /// Send timeout in milliseconds (`None` means infinite).
    pub send_timeout: Option<u64>,
    /// TCP only.
    pub keepalive: bool,
    pub tcp_nodelay: bool,
}

impl SocketOptions {
    pub const RECV_BUF_DEFAULT: usize = 16_384;
    pub const SEND_BUF_DEFAULT: usize = 16_384;
    pub const RECV_BUF_MIN: usize = 256;
    pub const RECV_BUF_MAX: usize = 262_144;
    pub const SEND_BUF_MIN: usize = 256;
    pub const SEND_BUF_MAX: usize = 262_144;

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

    pub fn validate_recv_buf_size(size: usize) -> Result<usize, NetError> {
        if !(Self::RECV_BUF_MIN..=Self::RECV_BUF_MAX).contains(&size) {
            return Err(NetError::InvalidArgument);
        }
        Ok(size)
    }

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

/// Push never overwrites; it returns `false` when full.
pub struct BoundedQueue<T> {
    slots: KVec<Option<T>>,
    head: usize,
    len: usize,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let slots: KVec<Option<T>> = core::iter::repeat_with(|| None).take(capacity).collect();
        Self {
            slots,
            head: 0,
            len: 0,
        }
    }

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

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            let _ = slot.take();
        }
        self.head = 0;
        self.len = 0;
    }

    /// Resize capacity, preserving item order; the queue is left untouched if
    /// either allocation fails. Shrinking keeps the oldest items, drops the rest.
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
/// `process` decides disclosure, and is an [`FdTable`] rather than a pid because
/// a recycled id would let the next holder of that number read the previous
/// one's socket ownership. `task_id` is what gets reported, because it is the
/// number the userland ABI speaks.
#[derive(Clone, Copy)]
pub struct SocketOwner {
    pub process: Option<FdTable>,
    pub task_id: u32,
}

impl SocketOwner {
    /// A socket no process opened, which in practice means one a test made.
    pub const UNOWNED: Self = Self {
        process: None,
        task_id: INVALID_PROCESS_ID,
    };
}

pub struct Socket {
    pub inner: SocketInner,
    pub state: SocketState,
    pub flags: SocketFlags,
    pub options: SocketOptions,
    pub local_addr: Option<SockAddr>,
    pub remote_addr: Option<SockAddr>,
    pub recv_queue: BoundedQueue<(PacketBuf, SockAddr)>,
    pub pending_error: Option<NetError>,
    /// Who opened it. Set once, at the single allocation site.
    pub owner: SocketOwner,
}

impl Socket {
    pub const RECV_QUEUE_DEFAULT_CAPACITY: usize = 16;

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
            // Not `0`: that is an id a real task can hold, so the default would
            // attribute the socket to a task that never opened it.
            owner: SocketOwner::UNOWNED,
        }
    }

    pub fn is_nonblocking(&self) -> bool {
        self.flags.contains(SocketFlags::O_NONBLOCK)
    }

    pub fn is_read_shutdown(&self) -> bool {
        self.flags.contains(SocketFlags::SHUT_RD)
    }

    pub fn is_write_shutdown(&self) -> bool {
        self.flags.contains(SocketFlags::SHUT_WR)
    }

    pub fn set_nonblocking(&mut self, nonblocking: bool) {
        if nonblocking {
            self.flags.set(SocketFlags::O_NONBLOCK);
        } else {
            self.flags.clear(SocketFlags::O_NONBLOCK);
        }
    }

    pub fn take_pending_error(&mut self) -> Option<NetError> {
        self.pending_error.take()
    }
}

pub struct SlabSocketTable {
    slots: KVec<Option<Socket>>,
    freelist: KVec<usize>,
    max_capacity: usize,
}

impl SlabSocketTable {
    pub const INITIAL_CAPACITY: usize = 64;
    /// Hard maximum slot count. The per-socket wait-queue spine is sized to
    /// this; a mismatch leaves a high slab index with no queue.
    pub const MAX_CAPACITY: usize = slopos_abi::net::MAX_SOCKET_SLOTS;

    /// Empty table for `static` initialisation; first use must call
    /// [`init_if_needed`](Self::init_if_needed).
    pub const fn empty() -> Self {
        Self {
            slots: KVec::new(),
            freelist: KVec::new(),
            max_capacity: 0,
        }
    }

    pub fn init_if_needed(&mut self) {
        if self.max_capacity == 0 {
            *self = Self::new(Self::INITIAL_CAPACITY, Self::MAX_CAPACITY);
            SOCKET_ALLOC.lock().set_capacity(Self::INITIAL_CAPACITY);
        }
    }

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
    /// The owner is taken here because this is the only place a socket comes
    /// into existence, and [`SocketOwner`] is what `net_query` redacts against,
    /// so a wrong value here is a wrong disclosure.
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

    pub fn get(&self, idx: usize) -> Option<&Socket> {
        self.slots.get(idx)?.as_ref()
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Socket> {
        self.slots.get_mut(idx)?.as_mut()
    }

    pub fn free(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx) {
            if slot.take().is_some() {
                let _ = self.freelist.push(idx);
                SOCKET_ALLOC.lock().free(idx);
            }
        }
    }

    pub fn count_active(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

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

/// Ephemeral port allocator. Access must be serialized by the outer lock
/// (no internal atomics).
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
    pub const EPHEMERAL_PORT_COUNT: usize = 16_384;

    pub const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
            next_port: Self::EPHEMERAL_PORT_START,
            allocated_count: 0,
        }
    }

    /// Equivalent to `*self = Self::new()`, but in place: a fresh `Self` would
    /// materialise the 2 KiB bitmap on the caller's stack.
    pub fn reset(&mut self) {
        self.bitmap.clear_all();
        self.next_port = Self::EPHEMERAL_PORT_START;
        self.allocated_count = 0;
    }

    /// In-place [`Init`] recipe equivalent to [`Self::new`]. The `AllocError`
    /// carrier satisfies `KBox::try_init`'s `E: From<AllocError>` bound; the
    /// closure itself never errors.
    pub fn init_default() -> impl Init<Self, slopos_ostd::mm::AllocError> {
        use slopos_ostd::mm::AllocError;
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                slot.zero_all();
                write_field!(slot, next_port, Self::EPHEMERAL_PORT_START);
                write_field!(slot, allocated_count, 0);
                Ok(slot.finish())
            },
        )
    }

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

    pub fn is_in_use(&self, port: Port) -> bool {
        let p = port.0;
        if !(Self::EPHEMERAL_PORT_START..=Self::EPHEMERAL_PORT_END).contains(&p) {
            return false;
        }
        self.bitmap.test((p - Self::EPHEMERAL_PORT_START) as usize)
    }

    pub fn available(&self) -> usize {
        Self::EPHEMERAL_PORT_COUNT - self.allocated_count
    }
}

impl Default for EphemeralPortAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Socket allocation bitmap, on its own lock so allocation decisions do not
/// contend with the socket table's hot data path.
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

    pub fn set_capacity(&mut self, cap: usize) {
        self.initialized_capacity = cap;
    }

    pub fn alloc(&mut self) -> Option<usize> {
        if self.allocated_count >= self.initialized_capacity {
            return None;
        }
        let idx = self.bitmap.find_next_zero(0, self.initialized_capacity)?;
        self.bitmap.set(idx);
        self.allocated_count += 1;
        Some(idx)
    }

    pub fn free(&mut self, idx: usize) {
        if idx < self.initialized_capacity && self.bitmap.test(idx) {
            self.bitmap.clear(idx);
            self.allocated_count = self.allocated_count.saturating_sub(1);
        }
    }

    pub fn is_allocated(&self, idx: usize) -> bool {
        idx < self.initialized_capacity && self.bitmap.test(idx)
    }

    pub fn count_active(&self) -> usize {
        self.allocated_count
    }

    pub fn clear(&mut self) {
        for i in 0..self.initialized_capacity {
            self.bitmap.clear(i);
        }
        self.allocated_count = 0;
    }
}

pub static SOCKET_ALLOC: slopos_ostd::sync::SpinLock<SocketAllocBitmap> =
    slopos_ostd::sync::SpinLock::new(
        SocketAllocBitmap::new(),
        slopos_ostd::lock_class!("SOCKET_ALLOC", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

pub static NEW_SOCKET_TABLE: slopos_ostd::sync::SpinLock<SlabSocketTable> =
    slopos_ostd::sync::SpinLock::new(
        SlabSocketTable::empty(),
        slopos_ostd::lock_class!("NEW_SOCKET_TABLE", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

pub static EPHEMERAL_PORTS: slopos_ostd::sync::SpinLock<EphemeralPortAllocator> =
    slopos_ostd::sync::SpinLock::new(
        EphemeralPortAllocator::new(),
        slopos_ostd::lock_class!("EPHEMERAL_PORTS", slopos_ostd::sync::LOCK_LEVEL_REGISTRY),
    );

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
/// Clamped to the global [`crate::pool::POOL_SIZE`]: a longer queue names a
/// depth no socket can reach, at real per-slot cost on every socket at once.
fn recv_queue_slots(bytes: usize) -> usize {
    let by_size = bytes / crate::pool::BUF_SIZE;
    by_size.clamp(1, crate::pool::POOL_SIZE)
}

#[inline]
fn sock_recv_ev(idx: u32) -> KernelEvent {
    KernelEvent::SocketRecv {
        sock: SocketSlot(idx),
    }
}

#[inline]
fn sock_send_ev(idx: u32) -> KernelEvent {
    KernelEvent::SocketSend {
        sock: SocketSlot(idx),
    }
}

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
/// pages (TCP `MSG_ZEROCOPY`): the TCP checksum is offloaded to the device and
/// the payload DMAs straight from `z`'s pages. Re-DMA-safe across retransmits —
/// the driver refcounts the pages independently of the send-queue chunk. On any
/// ineligibility or device rejection it copies the bytes into `scratch` and
/// sends them the ordinary way. Returns `0` or a negated errno; the caller
/// treats a nonzero result as a drain stop.
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
        // Patch the checksum field with the pseudo-header seed: the device sums
        // [csum_start..end] and completes it (NEEDS_CSUM).
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
            // Independent keepalive clone for the driver TX slot: it survives a
            // teardown mid-DMA and is released by the driver's own reclaim,
            // while `z.keepalive` stays owned for the copy fallback.
            if let Some(driver_ka) = z.keepalive.redup() {
                match net::DEVICE_REGISTRY.tx_zerocopy_notif_by_index(
                    dev,
                    &hdr[..hlen],
                    &runs,
                    Some(csum),
                    driver_ka,
                    z.token.clone(),
                ) {
                    Ok(()) => return 0,
                    // Ring full / oversize: fall through to the copy fallback.
                    Err(_) => {}
                }
            }
        }
    }

    if len > scratch.len()
        || copy_out_frames(z.keepalive.as_slice(), z.byte_start, &mut scratch[..len]).is_err()
    {
        return errno_i32(ERRNO_EIO);
    }
    socket_send_tcp_segment(seg, &scratch[..len])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SockWait {
    Ready,
    Timeout,
    /// A signal is pending: abort the syscall and let the dispatcher deliver it.
    Signal,
}

/// Block until `pred()` returns true, returning early on a pending signal so
/// the syscall surfaces `EINTR` instead of stalling out the full timeout.
fn wait_socket_event<F: FnMut() -> bool>(
    ev: KernelEvent,
    mut pred: F,
    timeout_ms: u64,
) -> SockWait {
    if slopos_kernel_services::driver_runtime::has_pending_signal() {
        return SockWait::Signal;
    }
    let sub = BUS.subscribe(ev);
    let observed = if timeout_ms > 0 {
        sub.wait_event_interruptible_timeout(&mut pred, timeout_ms)
    } else {
        sub.wait_event_interruptible(&mut pred)
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

/// Lets the ring's `OP_SEND_ZC` dispatch pick the TCP `MSG_ZEROCOPY`
/// send-queue path over the UDP/ICMP one-shot NIC-DMA leaf.
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

        // The child PCB inherits the listener's socket_id at install time.
        if actions.notify.contains(tcp::SocketNotify::NEW_ESTABLISHED) {
            if let Some(tuple) = tcp::with_pcb(conn_id, |pcb| pcb.tuple) {
                // Atomics only, so it is safe despite the locks this path takes.
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
                        // `None`: not a listener — a client's own `connect`
                        // completing. `Some(false)`: listener backlog full.
                        let queued = {
                            let mut table = NEW_SOCKET_TABLE.lock();
                            match table.get_mut(listener_idx as usize) {
                                Some(listener_sock)
                                    if listener_sock.state == SocketState::Listening =>
                                {
                                    match &mut listener_sock.inner {
                                        SocketInner::Tcp(tcp_inner) => tcp_inner
                                            .listen
                                            .as_mut()
                                            .map(|ls| ls.push_accepted(accepted)),
                                        _ => None,
                                    }
                                }
                                _ => None,
                            }
                        };
                        // A handshake nothing will ever accept still holds a
                        // shard slot, so reset the peer rather than leak it.
                        if queued == Some(false) {
                            let rst = tcp::SegmentBuilder::bare_rst(
                                accepted.tuple,
                                accepted.iss.wrapping_add(1),
                            );
                            tcp::table::release(conn_id);
                            let _ = socket_send_tcp_segment(&rst, &[]);
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

    // Off-lock: the per-socket wait-queue spine allocates and the table lock is
    // a cli-spinlock. Idempotent after the first call.
    slopos_ostd::sync::event_bus::ensure_socket_queues_allocated();

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
/// `payload` is a kernel staging buffer: every caller stages user bytes through
/// one first, so no user address reaches the socket layer and the bytes cannot
/// change under it. The same holds for the other slice entry points here.
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
        // ICMP SOCK_DGRAM contract (matches Linux): the user buffer is
        // [type(1)|code(1)|cksum(2)|id(2)|seq(2)|payload…], and the socket's
        // bound identifier overrides the id field.
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
                let Some(listen_state) = tcp_listener::TcpListenState::new(backlog as usize, local)
                else {
                    return errno_i32(ERRNO_ENOMEM);
                };
                tcp_inner.listen = Some(listen_state);
            }
            sock.state = SocketState::Listening;

            tcp::set_socket_idx(tcp_idx, Some(tcp::SocketId(sock_idx)));

            0
        }
        Err(e) => map_tcp_err(e),
    }
}

pub fn socket_accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32 {
    // Off-lock, before the table lock: accept allocates a socket slot too.
    slopos_ostd::sync::event_bus::ensure_socket_queues_allocated();
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
            // Captured before `alloc` takes `&mut table` and ends this borrow.
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

            let accepted = if let SocketInner::Tcp(ref mut tcp_inner) = listen_sock.inner {
                tcp_inner.listen.as_mut().and_then(|ls| ls.accept())
            } else {
                None
            };

            if let Some(accepted_conn) = accepted {
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
                // nobody else was in a position to ask for it.
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

                tcp::set_socket_idx(tcp_idx, Some(tcp::SocketId(new_idx as u32)));

                return new_idx as i32;
            }
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN);
        }

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

enum ConnectFamily {
    Tcp,
    /// UDP/ICMP: `connect` just records the peer and completes inline.
    Datagram,
    Unsupported,
}

fn socket_connect_family(inner: &SocketInner) -> ConnectFamily {
    match inner {
        SocketInner::Tcp(_) => ConnectFamily::Tcp,
        SocketInner::Udp(_) | SocketInner::Icmp(_) => ConnectFamily::Datagram,
        SocketInner::Raw(_) | SocketInner::Unix(_) => ConnectFamily::Unsupported,
    }
}

/// Locked half of a fresh TCP connect. The caller emits the returned SYN only
/// **after dropping the table lock** — the RX path takes it too, so sending
/// under it would deadlock — and owns the `EISCONN`-on-already-connecting guard.
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
    let send_rc = socket_send_tcp_segment(&syn_seg, &[]);
    if send_rc != 0 {
        let _ = tcp::abort(tcp_idx);
        return send_rc;
    }
    // RTO runs from the transmission, and the wheel must not be entered under the table lock.
    tcp::arm_syn_retransmit(tcp_idx);

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
    }
}

/// Idempotent, non-blocking connect for the ring's async connect probe: safe to
/// call repeatedly without re-sending a SYN or re-allocating a port. Returns
/// `0` when connected, `-EAGAIN` while the handshake is in flight — **never
/// `-EINPROGRESS`**, which the ring would post as an inline failed completion —
/// or another negated errno on a real error.
pub fn socket_connect_nonblock(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    enum Action {
        Syn(tcp::ConnId, TcpOutSegment),
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
            tcp::arm_syn_retransmit(tcp_idx);
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
/// auto-bind.
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

/// Validate the socket, perform UDP/ICMP auto-bind (rolling the ephemeral port
/// back on bind failure), and resolve the transport target. On error returns
/// the negated errno already widened to `i64`.
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
/// scratch-alloc failure.
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
/// (via `reader`) into the socket buffer — no kernel staging scratch.
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

/// True NIC-DMA zero-copy `socket_send` (SlopRing `OP_SEND_ZC`). Routes
/// connected **UDP** and **ICMP echo** through the NIC-DMA leaves; every other
/// case (TCP, unix, any resolve error) is [`ZcSendOutcome::NotEligible`] so the
/// caller uses the single-copy leaf. `runs` are the coalesced pinned
/// `(paddr, len)` physical runs (summing to `total_len`); `reader` is the same
/// pinned range as a volatile cursor (used only by ICMP for its CPU-side
/// checksum); `keepalive`/`token` are handed to the driver to hold across the
/// DMA.
pub fn socket_send_zerocopy(
    sock_idx: u32,
    runs: &[(u64, u32)],
    reader: &mut VmReader<'_>,
    total_len: usize,
    keepalive: KeepaliveFrames,
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
/// Enqueues a zero-copy chunk onto the send queue (holding the pinned pages
/// `keepalive` — data at the pin's `base_off` — and the refcounted `token`),
/// then kicks the send pump. The bytes DMA straight from the pinned pages as
/// the congestion window allows, re-DMA on retransmit, and the deferred
/// `F_NOTIF` fires once they are cumulatively ACKed and every in-flight DMA is
/// reclaimed. Returns `Submitted` once queued, or `NotEligible` (not a
/// connected TCP socket / does not fit SO_SNDBUF) so the caller uses the
/// single-direct-copy leaf.
pub fn socket_send_zerocopy_tcp(
    sock_idx: u32,
    keepalive: KeepaliveFrames,
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

/// The resolved transport kind of a recv, after validation.
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
/// EOF / nonblock / wait / napi-kick policy.
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

                // EOF only when the recv buffer is empty AND the peer closed:
                // FIN_WAIT_1/2 mean we sent FIN (write-shutdown) but can still
                // receive.
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
            }
            Err(e) => return map_tcp_err_i64(e),
        }
    }
}

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
    let (tcp_idx, udp_unbind, icmp_unbind, was_listener) = {
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

        // The SYN queue lives in the listener PCB and goes with it when
        // `tcp::close` releases the slot.
        if let SocketInner::Tcp(ref mut tcp_inner) = sock.inner {
            if let Some(ref mut listen_state) = tcp_inner.listen {
                listen_state.clear();
            }
            tcp_inner.listen = None;
        }

        table.free(sock_idx as usize);
        (tcp_idx, udp_unbind, icmp_unbind, was_listener)
    };

    // A listener's established children hold shard slots and name it as their
    // socket; nothing else reclaims them once it is gone, so release them here
    // and reset the peers that were still talking to them.
    if was_listener {
        for (tuple, seq) in tcp::release_children_of(tcp::SocketId(sock_idx)) {
            let rst = tcp::SegmentBuilder::bare_rst(tuple, seq);
            let _ = socket_send_tcp_segment(&rst, &[]);
        }
    }

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
/// table lock released.
///
/// `out` must be pre-allocated by the caller — nothing here allocates, because
/// phase one runs under a lock. Rows beyond its capacity are dropped; the
/// caller sizes it from [`socket_table_capacity`].
pub fn collect_sockets(out: &mut KVec<SocketRow>) {
    {
        let table = NEW_SOCKET_TABLE.lock();
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

    // A `ConnId` may have gone stale since phase one; `with_pcb` returns `None`
    // for a slot whose occupant changed, which leaves the row at the recorded
    // state rather than reporting another connection's.
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
    if crate::ingress::dataplane_quiesced() {
        return;
    }
    // Retransmit timers fire exclusively via NET_TIMER_WHEEL → tcp::on_retransmit.
    // `crate::clock`, not raw uptime: the deadlines compared here are set in that same domain.
    let now_ms = crate::clock::now_ms();
    if let Some((_idx, seg)) = tcp::delayed_ack_check(now_ms) {
        let _ = socket_send_tcp_segment(&seg, &[]);
    }
}

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

pub fn socket_get_peer_addr(sock_idx: u32) -> Option<SockAddr> {
    let table = NEW_SOCKET_TABLE.lock();
    let sock = table.get(sock_idx as usize)?;
    sock.remote_addr
}

pub fn socket_get_local_addr(sock_idx: u32) -> Option<SockAddr> {
    let table = NEW_SOCKET_TABLE.lock();
    let sock = table.get(sock_idx as usize)?;
    sock.local_addr
}
