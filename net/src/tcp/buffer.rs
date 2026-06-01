//! Per-connection send and receive buffers.
//!
//! Each active connection owns a pair of [`RingBuffer`]s plus a small amount
//! of delayed-ACK state.  The buffers are fixed-size today; resizable
//! buffers backed by `SO_SNDBUF` / `SO_RCVBUF` are a future feature, and
//! the `cap` parameter on [`TcpSendState::new`] / [`TcpRecvState::new`] /
//! [`TcpBufferPair::new`] is the hook it will plug into.  For now every
//! caller passes [`TCP_BUFFER_SIZE`].
//!
//! Buffers are lazily allocated as `Option<TcpBufferPair>` in the parallel
//! array inside [`super::table::PcbTable`].  Only Data-phase connections
//! have a buffer (`Some`); Listen, SynSent, SynRecv, and TimeWait keep
//! `None`.
//!
//! # Heap-backed ring buffers
//!
//! The 32 KiB send and receive ring buffers (`TcpBuffer`) live behind
//! `KBox<TcpBuffer>` inside [`TcpSendState`] / [`TcpRecvState`] — that is,
//! through `slopos-ostd`'s kernel-blessed allocation surface.  The
//! `KBox<T: Zeroable>::zeroed()` path routes to `alloc_zeroed` with no
//! stack temporary, so no function along
//! `alloc_buffer_for` → `TcpBufferPair::new` → `TcpSendState::new` /
//! `TcpRecvState::new` ever reserves 32 KiB on its frame.  `TcpBuffer`
//! satisfies `Zeroable` through the blanket impl on `RingBuffer<u8, N>`
//! shipped by `slopos-utils`.

use slopos_ostd::RingBuffer;
use slopos_ostd::{AllocError, KBox};

/// Size of per-connection send/receive ring buffers.
pub const TCP_BUFFER_SIZE: usize = 32768;

/// Delayed ACK timeout in milliseconds (RFC 1122 §4.2.3.2).
pub const DELAYED_ACK_MS: u64 = 200;

/// Send ACK after this many unacknowledged data segments.
pub const DELAYED_ACK_SEGMENTS: u8 = 2;

/// Zero-window probe interval in milliseconds.
pub const ZWP_INTERVAL_MS: u64 = 5000;

pub type TcpBuffer = RingBuffer<u8, TCP_BUFFER_SIZE>;

// -----------------------------------------------------------------------------
// Send state
// -----------------------------------------------------------------------------

pub struct TcpSendState {
    pub(crate) buf: KBox<TcpBuffer>,
    pub(crate) inflight: usize,
    pub(crate) rto_deadline_ms: u64,
    /// Soft cap on usable buffer capacity (SO_SNDBUF).
    /// Defaults to `TCP_BUFFER_SIZE`; values above that are silently capped
    /// by the caller.
    pub(crate) effective_capacity: usize,
}

impl TcpSendState {
    /// Allocate a zero-filled send state.  `cap` becomes the initial
    /// `effective_capacity`; all current callers pass `TCP_BUFFER_SIZE`,
    /// the parameter exists so future per-connection send-buffer sizing
    /// (SO_SNDBUF) has a hook.
    pub(crate) fn new(cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            buf: KBox::<TcpBuffer>::zeroed()?,
            inflight: 0,
            rto_deadline_ms: 0,
            effective_capacity: cap,
        })
    }

    pub fn enqueue(&mut self, data: &[u8]) -> usize {
        let avail = self.free_space();
        let n = core::cmp::min(data.len(), avail);
        if n == 0 {
            return 0;
        }
        self.buf.write(&data[..n])
    }

    /// Single-direct-copy [`enqueue`](Self::enqueue): pull up to
    /// `min(free_space, reader.remain())` bytes straight from the pinned user
    /// pages into the send ring with one volatile copy (no kernel scratch). The
    /// `free_space` cap honours `SO_SNDBUF` exactly like `enqueue`. Returns the
    /// number of bytes buffered.
    pub fn enqueue_from(&mut self, reader: &mut slopos_ostd::mm::VmReader<'_>) -> usize {
        let avail = self.free_space();
        if avail == 0 {
            return 0;
        }
        self.buf.write_from(reader, avail)
    }

    pub fn unsent_len(&self) -> usize {
        self.buf.len().saturating_sub(self.inflight)
    }

    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    pub fn free_space(&self) -> usize {
        let raw = self.buf.free_space();
        let cap_limit = self.effective_capacity.saturating_sub(self.buf.len());
        core::cmp::min(raw, cap_limit)
    }

    pub fn peek_unsent(&self, out: &mut [u8]) -> usize {
        self.buf.peek_at(self.inflight, out)
    }

    /// Read data at an arbitrary offset within the buffered (unacked) range.
    /// Used by the selective retransmit path to re-read lost segment data.
    pub fn peek_retransmit(&self, offset: usize, out: &mut [u8]) -> usize {
        self.buf.peek_at(offset, out)
    }

    pub fn mark_sent(&mut self, n: usize) {
        let unsent = self.unsent_len();
        let sent = core::cmp::min(n, unsent);
        self.inflight += sent;
    }

    pub fn process_ack(&mut self, acked: usize) {
        if acked == 0 {
            return;
        }

        let consumed = core::cmp::min(acked, self.buf.len());
        self.buf.consume(consumed);
        self.inflight = self.inflight.saturating_sub(consumed);
        if self.inflight == 0 {
            self.rto_deadline_ms = 0;
        }
    }

    pub fn clear(&mut self) {
        self.buf.reset();
        self.inflight = 0;
        self.rto_deadline_ms = 0;
    }

    pub fn inflight(&self) -> usize {
        self.inflight
    }

    pub fn rto_deadline_ms(&self) -> u64 {
        self.rto_deadline_ms
    }

    pub fn set_rto_deadline_ms(&mut self, deadline: u64) {
        self.rto_deadline_ms = deadline;
    }

    pub fn effective_capacity(&self) -> usize {
        self.effective_capacity
    }
}

// -----------------------------------------------------------------------------
// Receive state
// -----------------------------------------------------------------------------

pub struct TcpRecvState {
    pub(crate) buf: KBox<TcpBuffer>,
    pub(crate) segments_since_ack: u8,
    pub(crate) ack_pending: bool,
    pub(crate) delayed_ack_deadline_ms: u64,
    /// Soft cap on usable buffer capacity (SO_RCVBUF).
    pub(crate) effective_capacity: usize,
}

impl TcpRecvState {
    pub(crate) fn new(cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            buf: KBox::<TcpBuffer>::zeroed()?,
            segments_since_ack: 0,
            ack_pending: false,
            delayed_ack_deadline_ms: 0,
            effective_capacity: cap,
        })
    }

    pub fn enqueue(&mut self, data: &[u8], now_ms: u64) -> usize {
        if data.is_empty() {
            return 0;
        }

        let wrote = self.buf.write(data);
        if wrote > 0 {
            self.ack_pending = true;
            self.segments_since_ack = self.segments_since_ack.saturating_add(1);
            if self.segments_since_ack == 1 {
                self.delayed_ack_deadline_ms = now_ms.saturating_add(DELAYED_ACK_MS);
            }
        }
        wrote
    }

    pub fn dequeue(&mut self, out: &mut [u8]) -> usize {
        self.buf.read(out)
    }

    /// Single-direct-copy [`dequeue`](Self::dequeue): drain up to
    /// `min(available, writer.remain())` bytes straight from the recv ring into
    /// the pinned user pages with one volatile copy (no kernel scratch). Returns
    /// the number of bytes drained.
    pub fn dequeue_into(&mut self, writer: &mut slopos_ostd::mm::VmWriter<'_>) -> usize {
        self.buf.read_into(writer)
    }

    pub fn available(&self) -> usize {
        self.buf.len()
    }

    pub fn window(&self) -> u16 {
        let raw_free = self.buf.free_space();
        let cap_limit = self.effective_capacity.saturating_sub(self.buf.len());
        core::cmp::min(core::cmp::min(raw_free, cap_limit), u16::MAX as usize) as u16
    }

    pub fn should_ack_now(&self, now_ms: u64) -> bool {
        self.ack_pending
            && (self.segments_since_ack >= DELAYED_ACK_SEGMENTS
                || (self.delayed_ack_deadline_ms != 0 && now_ms >= self.delayed_ack_deadline_ms))
    }

    pub fn ack_sent(&mut self) {
        self.segments_since_ack = 0;
        self.ack_pending = false;
        self.delayed_ack_deadline_ms = 0;
    }

    pub fn clear(&mut self) {
        self.buf.reset();
        self.segments_since_ack = 0;
        self.ack_pending = false;
        self.delayed_ack_deadline_ms = 0;
    }

    pub fn ack_pending(&self) -> bool {
        self.ack_pending
    }

    pub fn segments_since_ack(&self) -> u8 {
        self.segments_since_ack
    }

    pub fn delayed_ack_deadline_ms(&self) -> u64 {
        self.delayed_ack_deadline_ms
    }

    pub fn effective_capacity(&self) -> usize {
        self.effective_capacity
    }
}

// -----------------------------------------------------------------------------
// Bundled send+recv+OOO
// -----------------------------------------------------------------------------

pub struct TcpBufferPair {
    pub(crate) send: TcpSendState,
    pub(crate) recv: TcpRecvState,
    pub(crate) ooo: super::reasm::Assembler,
}

impl TcpBufferPair {
    /// Allocate a fresh buffer pair.  Both ring buffers are zero-filled
    /// via `slopos-ostd::KBox::zeroed`; the whole chain is heap-direct.
    pub(crate) fn new(cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            send: TcpSendState::new(cap)?,
            recv: TcpRecvState::new(cap)?,
            ooo: super::reasm::Assembler::new(),
        })
    }

    pub fn clear(&mut self) {
        self.send.clear();
        self.recv.clear();
        self.ooo.clear();
    }

    pub fn send(&self) -> &TcpSendState {
        &self.send
    }

    pub fn send_mut(&mut self) -> &mut TcpSendState {
        &mut self.send
    }

    pub fn recv(&self) -> &TcpRecvState {
        &self.recv
    }

    pub fn recv_mut(&mut self) -> &mut TcpRecvState {
        &mut self.recv
    }

    pub fn ooo(&self) -> &super::reasm::Assembler {
        &self.ooo
    }

    pub fn ooo_mut(&mut self) -> &mut super::reasm::Assembler {
        &mut self.ooo
    }
}

// Size tripwires: the point of routing buffer allocation through
// `KBox` is to keep these state types small so every function along
// the buffer-allocation chain has a tiny frame.  If these grow,
// bring out a bigger rewrite — don't paper it over.
const _: () = assert!(core::mem::size_of::<TcpSendState>() <= 64);
const _: () = assert!(core::mem::size_of::<TcpRecvState>() <= 64);
const _: () = assert!(core::mem::size_of::<TcpBufferPair>() <= 256);
