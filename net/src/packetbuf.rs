//! Pool-backed packet buffer with zero-copy header push/pull and layer tracking.
//!
//! `PacketBuf` is the single currency exchanged between the driver layer and the
//! protocol stack.  It carries both the raw frame data and metadata (layer offsets,
//! head/tail pointers) that let each protocol layer access its headers without
//! reparsing from scratch.
//!
//! # Ownership
//!
//! `PacketBuf` is **move-only** — it deliberately does not implement `Clone`.
//! Dropping a pooled buffer automatically returns its slot to the global
//! [`PacketPool`](super::pool::PacketPool) via the `Drop` impl.
//!
//! # Layout
//!
//! ```text
//! |<-- headroom -->|<-- payload (head..tail) -->|<-- tailroom -->|
//! 0            head                          tail           capacity
//! ```
//!
//! * TX path: `alloc()` starts with `head = tail = HEADROOM`.  Headers are
//!   prepended via [`push_header`](PacketBuf::push_header); payload is appended
//!   via [`append`](PacketBuf::append).
//! * RX path: `from_raw_copy()` starts with `head = 0`, `tail = data.len()`.
//!   Headers are consumed via [`pull_header`](PacketBuf::pull_header).

use core::fmt;
use slopos_ostd::KVec;
use slopos_ostd::mm::frame::{Frame, PacketMeta};

use super::pool::{BUF_SIZE, PACKET_POOL};
use super::types::{Ipv4Addr, NetError};

/// Reserved headroom in each pooled buffer (bytes).
///
/// 128 bytes covers: Ethernet (14) + IP (20) + TCP max (60) + 34 spare.
/// Headers are prepended by decrementing `head`.
pub const HEADROOM: u16 = 128;

// =============================================================================
// PacketBufInner
// =============================================================================

/// Internal storage backing for a [`PacketBuf`].
enum PacketBufInner {
    /// Backed by the global [`PacketPool`](super::pool::PacketPool) —
    /// the fast-path allocation. Owns its frame by value; `Drop`
    /// returns the frame to the pool. `frame` is `Option` only so it
    /// can be moved out in `Drop`; it is `Some` for the whole of a live
    /// buffer's lifetime.
    Pooled {
        slot: u16,
        frame: Option<Frame<PacketMeta>>,
    },
    /// Heap-allocated fallback for oversized reassembly buffers.
    Oversized { data: KVec<u8> },
}

// =============================================================================
// PacketBuf
// =============================================================================

/// A network packet buffer with zero-copy header push/pull and layer offset
/// tracking.
///
/// See [module documentation](self) for layout and ownership semantics.
pub struct PacketBuf {
    inner: PacketBufInner,
    /// Start of the active data region within the backing buffer.
    head: u16,
    /// End of the active data region (exclusive).
    tail: u16,
    /// Byte offset of the L2 (Ethernet) header within the backing buffer.
    l2_offset: u16,
    /// Byte offset of the L3 (IPv4) header within the backing buffer.
    l3_offset: u16,
    /// Byte offset of the L4 (TCP/UDP) header within the backing buffer.
    l4_offset: u16,
}

// -- Drop: return pooled buffers automatically --------------------------------

impl Drop for PacketBuf {
    fn drop(&mut self) {
        if let PacketBufInner::Pooled { slot, frame } = &mut self.inner {
            if let Some(f) = frame.take() {
                PACKET_POOL.restore(*slot, f);
            }
        }
        // Oversized: the KVec<u8> is dropped implicitly.
    }
}

// -- Debug: metadata only, never dump raw buffer contents ---------------------

impl fmt::Debug for PacketBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            PacketBufInner::Pooled { slot, .. } => {
                write!(f, "PacketBuf::Pooled(slot={})", slot)?;
            }
            PacketBufInner::Oversized { data } => {
                write!(f, "PacketBuf::Oversized(cap={})", data.capacity())?;
            }
        }
        write!(
            f,
            " {{ head={}, tail={}, len={}, l2={}, l3={}, l4={} }}",
            self.head,
            self.tail,
            self.len(),
            self.l2_offset,
            self.l3_offset,
            self.l4_offset
        )
    }
}

// =============================================================================
// 1B.3 — Constructors
// =============================================================================

impl PacketBuf {
    /// Allocate an empty buffer from the global pool with [`HEADROOM`] reserved.
    ///
    /// Used by the **TX path** to build outgoing packets.  Push headers backward
    /// via [`push_header`](Self::push_header), append payload via
    /// [`append`](Self::append).
    ///
    /// Returns `None` if the pool is exhausted.
    pub fn alloc() -> Option<Self> {
        let (slot, frame) = PACKET_POOL.acquire()?;
        Some(Self {
            inner: PacketBufInner::Pooled {
                slot,
                frame: Some(frame),
            },
            head: HEADROOM,
            tail: HEADROOM,
            l2_offset: 0,
            l3_offset: 0,
            l4_offset: 0,
        })
    }

    /// Allocate a buffer and copy raw frame data into it.
    ///
    /// Used by the **RX path** when copying from a DMA ring buffer.  The data
    /// starts at offset 0 (no headroom) so that layer offsets match raw wire
    /// positions.
    ///
    /// Returns `None` if the pool is exhausted or `data.len() > BUF_SIZE`.
    pub fn from_raw_copy(data: &[u8]) -> Option<Self> {
        if data.len() > BUF_SIZE {
            return None;
        }
        let (slot, mut frame) = PACKET_POOL.acquire()?;
        // We own this frame exclusively after acquire().
        frame.as_bytes_mut()[..data.len()].copy_from_slice(data);
        Some(Self {
            inner: PacketBufInner::Pooled {
                slot,
                frame: Some(frame),
            },
            head: 0,
            tail: data.len() as u16,
            l2_offset: 0,
            l3_offset: 0,
            l4_offset: 0,
        })
    }

    /// Allocate an oversized buffer from the heap.
    ///
    /// Used **only** for IP reassembly buffers that exceed the pool's
    /// `BUF_SIZE`.  Normal packet allocation should always use [`alloc`](Self::alloc).
    pub fn oversized(capacity: usize) -> Self {
        Self {
            inner: PacketBufInner::Oversized {
                data: KVec::<u8>::zeroed(capacity).expect("packetbuf oversized: alloc"),
            },
            head: 0,
            tail: 0,
            l2_offset: 0,
            l3_offset: 0,
            l4_offset: 0,
        }
    }
}

// =============================================================================
// Internal buffer access
// =============================================================================

impl PacketBuf {
    /// Total capacity of the backing buffer.
    #[inline]
    pub fn capacity(&self) -> usize {
        match &self.inner {
            PacketBufInner::Pooled { .. } => BUF_SIZE,
            PacketBufInner::Oversized { data } => data.len(),
        }
    }

    /// Shared reference to the usable region of the backing buffer.
    #[inline]
    fn data(&self) -> &[u8] {
        match &self.inner {
            PacketBufInner::Pooled { frame, .. } => {
                // The pool lends each frame to exactly one PacketBuf, so
                // this handle is the only view of the bytes. The frame
                // is a full 4 KiB page; expose only the usable region.
                &frame.as_ref().expect("pooled frame present").as_bytes()[..BUF_SIZE]
            }
            PacketBufInner::Oversized { data } => data.as_slice(),
        }
    }

    /// Mutable reference to the usable region of the backing buffer.
    #[inline]
    fn data_mut(&mut self) -> &mut [u8] {
        match &mut self.inner {
            PacketBufInner::Pooled { frame, .. } => {
                // `&mut self` plus the one-handle-per-slot pool invariant
                // make this the only mutable view of the page bytes.
                &mut frame.as_mut().expect("pooled frame present").as_bytes_mut()[..BUF_SIZE]
            }
            PacketBufInner::Oversized { data } => data.as_mut_slice(),
        }
    }
}

// =============================================================================
// 1B.4 — Header push/pull and payload access
// =============================================================================

impl PacketBuf {
    /// Number of active payload bytes (`tail - head`).
    #[inline]
    pub fn len(&self) -> usize {
        (self.tail - self.head) as usize
    }

    /// `true` if the active region is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// Active data region `data[head..tail]`.
    #[inline]
    pub fn payload(&self) -> &[u8] {
        &self.data()[self.head as usize..self.tail as usize]
    }

    /// Mutable active data region `data[head..tail]`.
    #[inline]
    pub fn payload_mut(&mut self) -> &mut [u8] {
        let h = self.head as usize;
        let t = self.tail as usize;
        &mut self.data_mut()[h..t]
    }

    /// Prepend `len` bytes of header space by extending `head` backward into
    /// the headroom.
    ///
    /// Returns a mutable slice over the newly exposed bytes (caller fills in
    /// the header).  Fails with [`NoBufferSpace`](NetError::NoBufferSpace) if
    /// the headroom is insufficient.
    pub fn push_header(&mut self, len: usize) -> Result<&mut [u8], NetError> {
        let len16 = len as u16;
        if self.head < len16 {
            return Err(NetError::NoBufferSpace);
        }
        self.head -= len16;
        let h = self.head as usize;
        Ok(&mut self.data_mut()[h..h + len])
    }

    /// Consume `len` bytes from the front of the active region.
    ///
    /// Returns a shared slice over the consumed bytes (the header that was
    /// removed).  Fails with [`InvalidArgument`](NetError::InvalidArgument) if
    /// `len > self.len()`.
    pub fn pull_header(&mut self, len: usize) -> Result<&[u8], NetError> {
        if len > self.len() {
            return Err(NetError::InvalidArgument);
        }
        let old_head = self.head as usize;
        self.head += len as u16;
        Ok(&self.data()[old_head..old_head + len])
    }

    /// Trim the active region to at most `len` bytes from the current head.
    pub fn trim(&mut self, len: usize) {
        let max_tail = (self.head as usize) + len;
        if (self.tail as usize) > max_tail {
            self.tail = max_tail as u16;
        }
    }

    /// Append `src` bytes at the tail end of the active region.
    ///
    /// Fails with [`NoBufferSpace`](NetError::NoBufferSpace) if the remaining
    /// tailroom cannot hold `src`.
    pub fn append(&mut self, src: &[u8]) -> Result<(), NetError> {
        let new_tail = self.tail as usize + src.len();
        if new_tail > self.capacity() {
            return Err(NetError::NoBufferSpace);
        }
        let t = self.tail as usize;
        self.data_mut()[t..new_tail].copy_from_slice(src);
        self.tail = new_tail as u16;
        Ok(())
    }

    /// Append up to `len` bytes pulled directly from a volatile
    /// [`VmReader`](slopos_ostd::mm::VmReader) over pinned user pages (the
    /// SlopRing single-direct-copy path) at the tail end of the active region.
    /// The bytes are volatile-copied straight into the packet buffer with no
    /// intermediate kernel scratch. Returns the number of bytes appended (which
    /// may be short only if the reader runs dry). Fails with
    /// [`NoBufferSpace`](NetError::NoBufferSpace) if the tailroom cannot hold
    /// `len`.
    pub fn append_from(
        &mut self,
        reader: &mut slopos_ostd::mm::VmReader<'_>,
        len: usize,
    ) -> Result<usize, NetError> {
        let new_tail = self.tail as usize + len;
        if new_tail > self.capacity() {
            return Err(NetError::NoBufferSpace);
        }
        let t = self.tail as usize;
        let got = reader.read(&mut self.data_mut()[t..new_tail]);
        self.tail = (t + got) as u16;
        Ok(got)
    }
}

// =============================================================================
// 1B.5 — Layer offset helpers
// =============================================================================

impl PacketBuf {
    /// Record the byte offset of the L2 (Ethernet) header.
    #[inline]
    pub fn set_l2(&mut self, offset: u16) {
        self.l2_offset = offset;
    }

    /// Record the byte offset of the L3 (IPv4) header.
    #[inline]
    pub fn set_l3(&mut self, offset: u16) {
        self.l3_offset = offset;
    }

    /// Record the byte offset of the L4 (TCP/UDP) header.
    #[inline]
    pub fn set_l4(&mut self, offset: u16) {
        self.l4_offset = offset;
    }

    /// Raw L2 offset value.
    #[inline]
    pub fn l2_offset(&self) -> u16 {
        self.l2_offset
    }

    /// Raw L3 offset value.
    #[inline]
    pub fn l3_offset(&self) -> u16 {
        self.l3_offset
    }

    /// Raw L4 offset value.
    #[inline]
    pub fn l4_offset(&self) -> u16 {
        self.l4_offset
    }

    /// L2 (Ethernet) header bytes: `data[l2_offset..l3_offset]`.
    ///
    /// Returns `&[]` if `l3_offset` has not been set (i.e., the L2 end is
    /// not yet known).
    pub fn l2_header(&self) -> &[u8] {
        let start = self.l2_offset as usize;
        let end = self.l3_offset as usize;
        if end == 0 || end <= start {
            return &[];
        }
        let buf = self.data();
        let end = end.min(buf.len());
        &buf[start..end]
    }

    /// L3 (IPv4) header bytes: `data[l3_offset..l4_offset]`.
    ///
    /// Returns `&[]` if either `l3_offset` or `l4_offset` has not been set.
    pub fn l3_header(&self) -> &[u8] {
        let start = self.l3_offset as usize;
        let end = self.l4_offset as usize;
        if start == 0 || end == 0 || end <= start {
            return &[];
        }
        let buf = self.data();
        let end = end.min(buf.len());
        &buf[start..end]
    }

    /// L4 (TCP/UDP) header + payload bytes: `data[l4_offset..tail]`.
    ///
    /// Returns `&[]` if `l4_offset` has not been set.
    pub fn l4_header(&self) -> &[u8] {
        let start = self.l4_offset as usize;
        let end = self.tail as usize;
        if start == 0 || end <= start {
            return &[];
        }
        let buf = self.data();
        let end = end.min(buf.len());
        &buf[start..end]
    }

    /// Raw `head` value (useful for setting layer offsets during parsing).
    #[inline]
    pub fn head(&self) -> u16 {
        self.head
    }

    /// Raw `tail` value.
    #[inline]
    pub fn tail(&self) -> u16 {
        self.tail
    }
}

use super::checksum;

impl PacketBuf {
    /// Prepend a standard IPv4 header (IHL=5, TTL=64, no options) and compute
    /// the header checksum.  `l4_len` is the total size of everything already
    /// pushed after this point (L4 header + L4 payload).
    pub fn prepend_ipv4(
        &mut self,
        src_ip: [u8; 4],
        dst_ip: [u8; 4],
        protocol: u8,
        l4_len: usize,
    ) -> Result<(), NetError> {
        let total_len = (super::IPV4_HEADER_LEN + l4_len) as u16;
        let ip_hdr = self.push_header(super::IPV4_HEADER_LEN)?;
        ip_hdr[0] = 0x45;
        ip_hdr[1] = 0;
        ip_hdr[2..4].copy_from_slice(&total_len.to_be_bytes());
        ip_hdr[4..8].copy_from_slice(&[0; 4]);
        ip_hdr[8] = 64;
        ip_hdr[9] = protocol;
        ip_hdr[10..12].copy_from_slice(&[0; 2]);
        ip_hdr[12..16].copy_from_slice(&src_ip);
        ip_hdr[16..20].copy_from_slice(&dst_ip);
        let csum = checksum::internet_checksum(ip_hdr);
        ip_hdr[10..12].copy_from_slice(&csum.to_be_bytes());
        Ok(())
    }

    /// Prepend an Ethernet header for an IPv4 frame.
    pub fn prepend_eth(&mut self, src_mac: [u8; 6], dst_mac: [u8; 6]) -> Result<(), NetError> {
        let eth_hdr = self.push_header(super::ETH_HEADER_LEN)?;
        eth_hdr[0..6].copy_from_slice(&dst_mac);
        eth_hdr[6..12].copy_from_slice(&src_mac);
        eth_hdr[12..14].copy_from_slice(&super::EtherType::Ipv4.to_be_bytes());
        Ok(())
    }

    /// Set L2/L3/L4 layer offsets for a standard ETH+IPv4 frame.
    /// Call after `prepend_eth()` + `prepend_ipv4()` + L4 header push.
    pub fn set_ipv4_offsets(&mut self) {
        let head = self.head();
        self.set_l2(head);
        self.set_l3(head + super::ETH_HEADER_LEN as u16);
        self.set_l4(head + (super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN) as u16);
    }

    pub fn compute_ipv4_checksum(&self) -> u16 {
        let header = self.l3_header();
        if header.len() < 20 {
            return 0;
        }
        let ihl = ((header[0] & 0x0F) as usize) * 4;
        let header = &header[..ihl.min(header.len())];

        let mut sum = 0u32;
        sum = sum.wrapping_add(checksum::ones_complement_sum(&header[..10]));
        if header.len() > 12 {
            sum = sum.wrapping_add(checksum::ones_complement_sum(&header[12..]));
        }
        checksum::fold(sum)
    }

    pub fn compute_tcp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16 {
        let segment = self.l4_header();
        if segment.len() < 20 {
            return 0;
        }

        let mut sum = 0u32;
        checksum::add_pseudo_header(&mut sum, src.0, dst.0, 6, segment.len());
        sum = sum.wrapping_add(checksum::ones_complement_sum(&segment[..16]));
        if segment.len() > 18 {
            sum = sum.wrapping_add(checksum::ones_complement_sum(&segment[18..]));
        }
        checksum::fold(sum)
    }

    /// Per RFC 768, a computed checksum of zero is transmitted as `0xFFFF`.
    pub fn compute_udp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16 {
        let segment = self.l4_header();
        if segment.len() < 8 {
            return 0;
        }

        let mut sum = 0u32;
        checksum::add_pseudo_header(&mut sum, src.0, dst.0, 17, segment.len());
        sum = sum.wrapping_add(checksum::ones_complement_sum(&segment[..6]));
        if segment.len() > 8 {
            sum = sum.wrapping_add(checksum::ones_complement_sum(&segment[8..]));
        }

        let csum = checksum::fold(sum);
        if csum == 0 { 0xFFFF } else { csum }
    }
}
