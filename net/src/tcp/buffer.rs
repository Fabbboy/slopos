//! Per-connection send and receive buffers.
//!
//! Each active connection owns a pair of [`RingBuffer`]s plus a small amount
//! of delayed-ACK state.  The buffers are fixed-size (see [`TCP_BUFFER_SIZE`]);
//! resizable buffers backed by `SO_SNDBUF` / `SO_RCVBUF` land in P4.
//!
//! Buffers are lazily allocated as `Option<TcpBufferPair>` in the parallel
//! array inside [`super::table::PcbTable`].  Only Data-phase connections
//! have a buffer (`Some`); Listen, SynSent, SynRecv, and TimeWait keep
//! `None`.
//!
//! # Heap-backed ring buffers
//!
//! The 32 KiB send and receive ring buffers (`TcpBuffer`) live behind
//! `Box<TcpBuffer>` inside [`TcpSendState`] / [`TcpRecvState`].  That
//! keeps the state structs themselves small (≈48 bytes), so every
//! function along the `alloc_buffer_for` → `TcpBufferPair::new` →
//! `TcpSendState::new` / `TcpRecvState::new` chain has a tiny frame
//! instead of the 64–128 KiB frames produced when each constructor
//! returned the full buffer by value.  The buffers are allocated via
//! `Box::<TcpBuffer>::new_zeroed().assume_init()` — zero is a valid
//! bit-pattern for `RingBuffer<u8, N>` (the head/tail/count indices
//! just mean "empty") so no stack temporary is needed.

extern crate alloc;

use alloc::boxed::Box;

use slopos_utils::RingBuffer;

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

#[derive(Debug)]
pub struct TcpSendState {
    pub buf: Box<TcpBuffer>,
    pub inflight: usize,
    pub rto_deadline_ms: u64,
    /// Soft cap on usable buffer capacity (SO_SNDBUF).
    /// Defaults to TCP_BUFFER_SIZE; values above that are silently capped.
    pub effective_capacity: usize,
}

impl TcpSendState {
    pub fn new() -> Self {
        Self {
            // SAFETY: `TcpBuffer` is `RingBuffer<u8, TCP_BUFFER_SIZE>` —
            // `[u8; N]` plus three `usize` indices.  An all-zero
            // bit-pattern is a valid, empty ring buffer, so `new_zeroed`
            // + `assume_init` is sound and keeps the 32 KiB buffer off
            // the caller's stack.
            buf: unsafe { Box::<TcpBuffer>::new_zeroed().assume_init() },
            inflight: 0,
            rto_deadline_ms: 0,
            effective_capacity: TCP_BUFFER_SIZE,
        }
    }

    pub fn enqueue(&mut self, data: &[u8]) -> usize {
        let avail = self.free_space();
        let n = core::cmp::min(data.len(), avail);
        if n == 0 {
            return 0;
        }
        self.buf.write(&data[..n])
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
}

// -----------------------------------------------------------------------------
// Receive state
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct TcpRecvState {
    pub buf: Box<TcpBuffer>,
    pub segments_since_ack: u8,
    pub ack_pending: bool,
    pub delayed_ack_deadline_ms: u64,
    /// Soft cap on usable buffer capacity (SO_RCVBUF).
    pub effective_capacity: usize,
}

impl TcpRecvState {
    pub fn new() -> Self {
        Self {
            // SAFETY: see `TcpSendState::new` — `TcpBuffer` is
            // zero-valid, so heap-zeroing avoids the 32 KiB stack
            // temporary `TcpBuffer::new_zeroed()` would produce.
            buf: unsafe { Box::<TcpBuffer>::new_zeroed().assume_init() },
            segments_since_ack: 0,
            ack_pending: false,
            delayed_ack_deadline_ms: 0,
            effective_capacity: TCP_BUFFER_SIZE,
        }
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
}

// -----------------------------------------------------------------------------
// Bundled send+recv+OOO
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct TcpBufferPair {
    pub send: TcpSendState,
    pub recv: TcpRecvState,
    pub ooo: super::reasm::Assembler,
}

impl TcpBufferPair {
    pub fn new() -> Self {
        Self {
            send: TcpSendState::new(),
            recv: TcpRecvState::new(),
            ooo: super::reasm::Assembler::new(),
        }
    }

    pub fn clear(&mut self) {
        self.send.clear();
        self.recv.clear();
        self.ooo.clear();
    }
}
