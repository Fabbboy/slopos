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

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use super::pool::{BUF_SIZE, PACKET_POOL, PacketPool};
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
    /// Backed by the global [`PacketPool`] — the fast-path allocation.
    Pooled {
        pool: &'static PacketPool,
        slot: u16,
    },
    /// Heap-allocated fallback for oversized reassembly buffers.
    Oversized { data: Vec<u8> },
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
        if let PacketBufInner::Pooled { pool, slot } = &self.inner {
            pool.release(*slot);
        }
        // Oversized: the Vec<u8> is dropped implicitly.
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
        let slot = PACKET_POOL.alloc()?;
        Some(Self {
            inner: PacketBufInner::Pooled {
                pool: &PACKET_POOL,
                slot,
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
        let slot = PACKET_POOL.alloc()?;
        // SAFETY: We own this slot exclusively after alloc().
        unsafe {
            let dst = PACKET_POOL.slot_data(slot);
            core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
        }
        Some(Self {
            inner: PacketBufInner::Pooled {
                pool: &PACKET_POOL,
                slot,
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
                data: alloc::vec![0u8; capacity],
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

    /// Shared reference to the entire backing buffer.
    #[inline]
    fn data(&self) -> &[u8] {
        match &self.inner {
            PacketBufInner::Pooled { pool, slot } => {
                // SAFETY: We own this slot — exclusive access guaranteed by
                // move-only semantics (no Clone).
                unsafe { core::slice::from_raw_parts(pool.slot_data(*slot), BUF_SIZE) }
            }
            PacketBufInner::Oversized { data } => data.as_slice(),
        }
    }

    /// Mutable reference to the entire backing buffer.
    #[inline]
    fn data_mut(&mut self) -> &mut [u8] {
        match &mut self.inner {
            PacketBufInner::Pooled { pool, slot } => {
                // SAFETY: We own this slot and hold &mut self — exclusive access.
                unsafe { core::slice::from_raw_parts_mut(pool.slot_data(*slot), BUF_SIZE) }
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

// =============================================================================
// 1B.6 — Checksum helpers
// =============================================================================

/// Accumulate the one's-complement sum over a byte slice.
///
/// Used internally by the checksum methods.  The caller must fold the result
/// via [`fold_checksum`] after accumulating all data.
fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        sum = sum.wrapping_add(word);
        i += 2;
    }
    // Odd trailing byte — pad with zero on the right.
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }
    sum
}

/// Fold a 32-bit running sum into a 16-bit one's-complement checksum.
fn fold_checksum(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Accumulate the IPv4 pseudo-header into `sum`.
fn add_pseudo_header(sum: &mut u32, src: &Ipv4Addr, dst: &Ipv4Addr, protocol: u8, l4_len: usize) {
    *sum = sum.wrapping_add(u16::from_be_bytes([src.0[0], src.0[1]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([src.0[2], src.0[3]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([dst.0[0], dst.0[1]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([dst.0[2], dst.0[3]]) as u32);
    *sum = sum.wrapping_add(protocol as u32);
    *sum = sum.wrapping_add(l4_len as u32);
}

impl PacketBuf {
    /// Compute the IPv4 header checksum over the L3 header bytes.
    ///
    /// The checksum field (bytes 10–11) is treated as zero during computation.
    /// Requires `l3_offset` and `l4_offset` to be set.
    pub fn compute_ipv4_checksum(&self) -> u16 {
        let header = self.l3_header();
        if header.len() < 20 {
            return 0;
        }
        // Use IHL to determine actual header length (may include options).
        let ihl = ((header[0] & 0x0F) as usize) * 4;
        let header = &header[..ihl.min(header.len())];

        let mut sum = 0u32;
        // Bytes before the checksum field (0..10).
        sum = sum.wrapping_add(ones_complement_sum(&header[..10]));
        // Skip bytes 10–11 (checksum field — treated as zero).
        if header.len() > 12 {
            sum = sum.wrapping_add(ones_complement_sum(&header[12..]));
        }
        fold_checksum(sum)
    }

    /// Compute the TCP checksum (pseudo-header + L4 segment).
    ///
    /// The checksum field at TCP header bytes 16–17 is treated as zero.
    /// The L4 segment includes both the TCP header and its payload.
    ///
    /// Software checksum is always computed.  If `NetDeviceFeatures::CHECKSUM_TX`
    /// is set, the driver may offload — but the stack does not skip computation
    /// (simplicity over performance for now).
    pub fn compute_tcp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16 {
        let segment = self.l4_header();
        if segment.len() < 20 {
            return 0;
        }

        let mut sum = 0u32;
        add_pseudo_header(&mut sum, &src, &dst, 6, segment.len());

        // TCP header bytes before the checksum field (0..16).
        sum = sum.wrapping_add(ones_complement_sum(&segment[..16]));
        // Skip bytes 16–17 (checksum field).
        if segment.len() > 18 {
            sum = sum.wrapping_add(ones_complement_sum(&segment[18..]));
        }
        fold_checksum(sum)
    }

    /// Compute the UDP checksum (pseudo-header + L4 datagram).
    ///
    /// The checksum field at UDP header bytes 6–7 is treated as zero.
    /// Per RFC 768, a computed checksum of zero is transmitted as `0xFFFF`.
    pub fn compute_udp_checksum(&self, src: Ipv4Addr, dst: Ipv4Addr) -> u16 {
        let segment = self.l4_header();
        if segment.len() < 8 {
            return 0;
        }

        let mut sum = 0u32;
        add_pseudo_header(&mut sum, &src, &dst, 17, segment.len());

        // UDP header bytes before the checksum field (0..6).
        sum = sum.wrapping_add(ones_complement_sum(&segment[..6]));
        // Skip bytes 6–7 (checksum field).
        if segment.len() > 8 {
            sum = sum.wrapping_add(ones_complement_sum(&segment[8..]));
        }

        let csum = fold_checksum(sum);
        // RFC 768: transmitted checksum of 0 is encoded as 0xFFFF.
        if csum == 0 { 0xFFFF } else { csum }
    }
}
