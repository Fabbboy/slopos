//! TCP (Transmission Control Protocol) implementation — RFC 793 + RFC 7413.
//!
//! Provides TCP header parsing/construction, one's-complement checksum with
//! IPv4 pseudo-header, a full TCP state machine, connection table, three-way
//! handshake (active and passive open), and connection teardown.
//!
//! This module is purely protocol logic — it does **not** drive the NIC
//! directly.  Higher layers wire it into the VirtIO net driver
//! for actual packet I/O.

use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use slopos_lib::{IrqMutex, RingBuffer, klog_debug};

use crate::net::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};

// =============================================================================
// Constants
// =============================================================================

/// Minimum TCP header length (no options).
pub const TCP_HEADER_LEN: usize = 20;

/// Maximum TCP header length (with 40 bytes of options).
pub const TCP_HEADER_MAX_LEN: usize = 60;

/// Maximum number of simultaneous TCP connections.
pub const MAX_CONNECTIONS: usize = 64;

/// Default Maximum Segment Size (Ethernet MTU 1500 − IP 20 − TCP 20).
pub const DEFAULT_MSS: u16 = 1460;

/// Default receive window size advertised in SYN (unscaled, fits in u16).
pub const DEFAULT_WINDOW_SIZE: u16 = 32768;

/// Initial retransmission timeout in milliseconds (RFC 6298 recommends 1s).
pub const INITIAL_RTO_MS: u32 = 1000;

/// Maximum retransmission timeout in milliseconds.
pub const MAX_RTO_MS: u32 = 60_000;

/// TIME_WAIT duration in milliseconds (2 × MSL, MSL = 30s).
pub const TIME_WAIT_MS: u64 = 60_000;

/// Maximum retransmission attempts before giving up.
pub const MAX_RETRANSMITS: u8 = 8;

/// Size of per-connection send/receive ring buffers.
pub const TCP_BUFFER_SIZE: usize = 32768;
/// Delayed ACK timeout in milliseconds (RFC 1122 §4.2.3.2).
pub const DELAYED_ACK_MS: u64 = 200;
/// Send ACK after this many unacknowledged data segments.
pub const DELAYED_ACK_SEGMENTS: u8 = 2;
/// Zero-window probe interval in milliseconds.
pub const ZWP_INTERVAL_MS: u64 = 5000;

// ---------------------------------------------------------------------------
// TCP flag bits (in the flags byte of the header)
// ---------------------------------------------------------------------------

pub const TCP_FLAG_FIN: u8 = 0x01;
pub const TCP_FLAG_SYN: u8 = 0x02;
pub const TCP_FLAG_RST: u8 = 0x04;
pub const TCP_FLAG_PSH: u8 = 0x08;
pub const TCP_FLAG_ACK: u8 = 0x10;
pub const TCP_FLAG_URG: u8 = 0x20;

// ---------------------------------------------------------------------------
// TCP option kinds
// ---------------------------------------------------------------------------

pub const TCP_OPT_END: u8 = 0;
pub const TCP_OPT_NOP: u8 = 1;
pub const TCP_OPT_MSS: u8 = 2;
pub const TCP_OPT_MSS_LEN: u8 = 4;
pub const TCP_OPT_WINDOW_SCALE: u8 = 3;
pub const TCP_OPT_WINDOW_SCALE_LEN: u8 = 3;

/// Maximum number of out-of-order segments buffered per connection.
const OOO_MAX_ENTRIES: usize = 8;
/// Maximum payload bytes stored per out-of-order entry (one MSS).
const OOO_ENTRY_MAX: usize = 1460;

// =============================================================================
// TCP Header
// =============================================================================

/// Parsed TCP header.
///
/// All multi-byte fields are stored in **host** byte order after parsing.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq_num: u32,
    pub ack_num: u32,
    /// Data offset in 32-bit words (5–15).
    pub data_offset: u8,
    pub flags: u8,
    pub window_size: u16,
    pub checksum: u16,
    pub urgent_ptr: u16,
}

impl TcpHeader {
    /// Header length in bytes (data_offset × 4).
    #[inline]
    pub const fn header_len(&self) -> usize {
        (self.data_offset as usize) * 4
    }

    /// Options length in bytes (header_len − 20).
    #[inline]
    pub const fn options_len(&self) -> usize {
        self.header_len().saturating_sub(TCP_HEADER_LEN)
    }

    // --- Flag helpers -------------------------------------------------------

    #[inline]
    pub const fn is_syn(&self) -> bool {
        (self.flags & TCP_FLAG_SYN) != 0
    }

    #[inline]
    pub const fn is_ack(&self) -> bool {
        (self.flags & TCP_FLAG_ACK) != 0
    }

    #[inline]
    pub const fn is_fin(&self) -> bool {
        (self.flags & TCP_FLAG_FIN) != 0
    }

    #[inline]
    pub const fn is_rst(&self) -> bool {
        (self.flags & TCP_FLAG_RST) != 0
    }

    #[inline]
    pub const fn is_psh(&self) -> bool {
        (self.flags & TCP_FLAG_PSH) != 0
    }

    #[inline]
    pub const fn is_urg(&self) -> bool {
        (self.flags & TCP_FLAG_URG) != 0
    }

    #[inline]
    pub const fn is_syn_ack(&self) -> bool {
        self.is_syn() && self.is_ack()
    }

    #[inline]
    pub const fn is_fin_ack(&self) -> bool {
        self.is_fin() && self.is_ack()
    }
}

// =============================================================================
// Parsing
// =============================================================================

/// Parse a TCP header from a byte slice.
///
/// Returns `None` if the slice is too short or the data offset is invalid.
pub fn parse_header(data: &[u8]) -> Option<TcpHeader> {
    if data.len() < TCP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let seq_num = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ack_num = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let data_offset = (data[12] >> 4) & 0x0F;
    let flags = data[13] & 0x3F; // low 6 bits of byte 13

    let window_size = u16::from_be_bytes([data[14], data[15]]);
    let checksum = u16::from_be_bytes([data[16], data[17]]);
    let urgent_ptr = u16::from_be_bytes([data[18], data[19]]);

    // Data offset must be at least 5 (20 bytes) and at most 15 (60 bytes).
    if data_offset < 5 || data_offset > 15 {
        return None;
    }

    let header_len = (data_offset as usize) * 4;
    if data.len() < header_len {
        return None;
    }

    Some(TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum,
        urgent_ptr,
    })
}

/// Parse MSS option from TCP header options region.
///
/// Returns the MSS value if found, otherwise `None`.
pub fn parse_mss_option(options: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            TCP_OPT_END => break,
            TCP_OPT_NOP => {
                i += 1;
            }
            TCP_OPT_MSS => {
                if i + 3 < options.len() && options[i + 1] == TCP_OPT_MSS_LEN {
                    return Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                }
                break;
            }
            _ => {
                // Unknown option: skip using length byte.
                if i + 1 >= options.len() {
                    break;
                }
                let opt_len = options[i + 1] as usize;
                if opt_len < 2 || i + opt_len > options.len() {
                    break;
                }
                i += opt_len;
            }
        }
    }
    None
}

pub struct ParsedTcpOptions {
    pub mss: Option<u16>,
    pub window_scale: Option<u8>,
}

pub fn parse_tcp_options(options: &[u8]) -> ParsedTcpOptions {
    let mut result = ParsedTcpOptions {
        mss: None,
        window_scale: None,
    };
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            TCP_OPT_END => break,
            TCP_OPT_NOP => {
                i += 1;
            }
            TCP_OPT_MSS => {
                if i + 3 < options.len() && options[i + 1] == TCP_OPT_MSS_LEN {
                    result.mss = Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                }
                i += TCP_OPT_MSS_LEN as usize;
            }
            TCP_OPT_WINDOW_SCALE => {
                if i + 2 < options.len() && options[i + 1] == TCP_OPT_WINDOW_SCALE_LEN {
                    // RFC 7323: shift count must be <= 14
                    result.window_scale = Some(options[i + 2].min(14));
                }
                i += TCP_OPT_WINDOW_SCALE_LEN as usize;
            }
            _ => {
                if i + 1 >= options.len() {
                    break;
                }
                let opt_len = options[i + 1] as usize;
                if opt_len < 2 || i + opt_len > options.len() {
                    break;
                }
                i += opt_len;
            }
        }
    }
    result
}

/// Compute our receive-side window scale shift count from TCP_BUFFER_SIZE.
/// RFC 7323 §2.2: shift count = ceil(log2(buffer_size / 65535)).
pub fn our_window_scale() -> u8 {
    let mut shift = 0u8;
    let mut size = TCP_BUFFER_SIZE;
    while size > u16::MAX as usize && shift < 14 {
        size >>= 1;
        shift += 1;
    }
    shift
}

pub fn write_window_scale_option(shift: u8, out: &mut [u8]) -> Option<usize> {
    if out.len() < 3 {
        return None;
    }
    out[0] = TCP_OPT_WINDOW_SCALE;
    out[1] = TCP_OPT_WINDOW_SCALE_LEN;
    out[2] = shift;
    Some(3)
}

// =============================================================================
// Construction
// =============================================================================

/// Write a TCP header into `out[..TCP_HEADER_LEN]`.
///
/// Returns `TCP_HEADER_LEN` on success, `None` if `out` is too short.
/// The checksum field is written as 0 — the caller must compute and patch it
/// afterwards using [`tcp_checksum`].
pub fn write_header(hdr: &TcpHeader, out: &mut [u8]) -> Option<usize> {
    let header_len = hdr.header_len();
    if out.len() < header_len || header_len < TCP_HEADER_LEN {
        return None;
    }

    out[0..2].copy_from_slice(&hdr.src_port.to_be_bytes());
    out[2..4].copy_from_slice(&hdr.dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&hdr.seq_num.to_be_bytes());
    out[8..12].copy_from_slice(&hdr.ack_num.to_be_bytes());
    out[12] = (hdr.data_offset << 4) & 0xF0;
    out[13] = hdr.flags & 0x3F;
    out[14..16].copy_from_slice(&hdr.window_size.to_be_bytes());
    out[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[18..20].copy_from_slice(&hdr.urgent_ptr.to_be_bytes());

    // Zero any options area beyond the minimum header.
    if header_len > TCP_HEADER_LEN {
        out[TCP_HEADER_LEN..header_len].fill(0);
    }

    Some(header_len)
}

/// Build a minimal TCP header with the given parameters.
pub fn build_header(
    src_port: u16,
    dst_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    window_size: u16,
    data_offset: u8,
) -> TcpHeader {
    TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// Write MSS option into `out` buffer.  Returns bytes written (4) or `None`.
pub fn write_mss_option(mss: u16, out: &mut [u8]) -> Option<usize> {
    if out.len() < 4 {
        return None;
    }
    out[0] = TCP_OPT_MSS;
    out[1] = TCP_OPT_MSS_LEN;
    out[2..4].copy_from_slice(&mss.to_be_bytes());
    Some(4)
}

// =============================================================================
// Checksum
// =============================================================================

/// Compute the one's-complement sum over a byte slice (for checksum accumulation).
fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < data.len() {
        sum = sum.wrapping_add(u16::from_be_bytes([data[i], data[i + 1]]) as u32);
        i += 2;
    }
    // Handle trailing odd byte.
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }
    sum
}

/// Fold a 32-bit accumulator into a 16-bit one's-complement value.
fn fold_checksum(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Compute the TCP checksum over a pseudo-header + TCP header + payload.
///
/// The pseudo-header includes `src_ip`, `dst_ip`, protocol (6 = TCP), and the
/// TCP segment length (header + payload).
///
/// `tcp_segment` must contain the full TCP segment (header + payload) with the
/// checksum field set to 0.
pub fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> u16 {
    let tcp_len = tcp_segment.len() as u16;

    // Pseudo-header: src_ip (4) + dst_ip (4) + zero (1) + proto (1) + tcp_len (2) = 12 bytes
    let mut sum = 0u32;
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[0], src_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[2], src_ip[3]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[0], dst_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[2], dst_ip[3]]) as u32);
    sum = sum.wrapping_add(6u32); // Protocol = TCP
    sum = sum.wrapping_add(tcp_len as u32);

    // Add TCP segment bytes.
    sum = sum.wrapping_add(ones_complement_sum(tcp_segment));

    fold_checksum(sum)
}

/// Verify a received TCP segment's checksum.
///
/// Returns `true` if the checksum is valid (folds to 0).
pub fn verify_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> bool {
    let tcp_len = tcp_segment.len() as u16;

    let mut sum = 0u32;
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[0], src_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[2], src_ip[3]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[0], dst_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[2], dst_ip[3]]) as u32);
    sum = sum.wrapping_add(6u32);
    sum = sum.wrapping_add(tcp_len as u32);
    sum = sum.wrapping_add(ones_complement_sum(tcp_segment));

    fold_checksum(sum) == 0
}

// =============================================================================
// TCP State Machine (RFC 793)
// =============================================================================

/// TCP connection state per RFC 793 §3.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl TcpState {
    /// Human-readable name for logging.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Listen => "LISTEN",
            Self::SynSent => "SYN_SENT",
            Self::SynReceived => "SYN_RECEIVED",
            Self::Established => "ESTABLISHED",
            Self::FinWait1 => "FIN_WAIT_1",
            Self::FinWait2 => "FIN_WAIT_2",
            Self::CloseWait => "CLOSE_WAIT",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST_ACK",
            Self::TimeWait => "TIME_WAIT",
        }
    }

    /// Is this state "open" (capable of data transfer or about to be)?
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Established | Self::FinWait1 | Self::FinWait2 | Self::CloseWait
        )
    }

    /// Is this state a closing/teardown state?
    pub const fn is_closing(self) -> bool {
        matches!(
            self,
            Self::FinWait1
                | Self::FinWait2
                | Self::CloseWait
                | Self::Closing
                | Self::LastAck
                | Self::TimeWait
        )
    }
}

// =============================================================================
// TCP Connection
// =============================================================================

/// Four-tuple identifying a TCP connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpTuple {
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
}

impl TcpTuple {
    pub const ZERO: Self = Self {
        local_ip: [0; 4],
        local_port: 0,
        remote_ip: [0; 4],
        remote_port: 0,
    };

    /// Check if this tuple matches a specific remote endpoint (for listen sockets,
    /// `remote_ip`/`remote_port` may be zero = wildcard).
    pub fn matches(&self, other: &TcpTuple) -> bool {
        self.local_ip == other.local_ip
            && self.local_port == other.local_port
            && (self.remote_ip == [0; 4] || self.remote_ip == other.remote_ip)
            && (self.remote_port == 0 || self.remote_port == other.remote_port)
    }
}

/// Error type for TCP operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Connection table is full.
    TableFull,
    /// No connection found for the given tuple.
    NotFound,
    /// Connection is in wrong state for the requested operation.
    InvalidState,
    /// Port already in use.
    AddrInUse,
    /// Connection was reset by peer.
    ConnectionReset,
    /// Connection timed out.
    TimedOut,
    /// Connection refused by peer (RST received in SYN_SENT).
    ConnectionRefused,
    /// Invalid segment or parameter.
    InvalidSegment,
}

/// Outgoing segment generated by the state machine.
///
/// The TCP layer produces these; the network layer converts them to
/// wire-format packets and hands them to the NIC.
#[derive(Clone, Copy, Debug)]
pub struct TcpOutSegment {
    pub tuple: TcpTuple,
    pub seq_num: u32,
    pub ack_num: u32,
    pub flags: u8,
    pub window_size: u16,
    /// MSS option to include (0 = no MSS option).
    pub mss: u16,
    /// Window Scale option shift count (255 = don't include option).
    pub wscale: u8,
}

/// Per-connection state.
#[derive(Clone, Copy, Debug)]
pub struct TcpConnection {
    pub tuple: TcpTuple,
    pub state: TcpState,

    // --- Send sequence variables (RFC 793 §3.2) ---
    /// Send unacknowledged.
    pub snd_una: u32,
    /// Send next.
    pub snd_nxt: u32,
    /// Send window (scaled value after window scaling is negotiated).
    pub snd_wnd: u32,
    /// Initial send sequence number.
    pub iss: u32,

    // --- Receive sequence variables ---
    /// Receive next.
    pub rcv_nxt: u32,
    /// Receive window.
    pub rcv_wnd: u16,
    /// Initial receive sequence number.
    pub irs: u32,

    /// Peer's advertised MSS (or DEFAULT_MSS if not specified).
    pub peer_mss: u16,

    /// Window scale shift count we send (our receive window scaling).
    pub rcv_wscale: u8,
    /// Window scale shift count the peer sends (their receive window scaling).
    pub snd_wscale: u8,
    /// Whether window scaling was negotiated during the handshake.
    pub wscale_enabled: bool,

    /// Retransmission timeout (ms).
    pub rto_ms: u32,
    /// Retransmit counter.
    pub retransmits: u8,

    /// Timer token for the pending retransmit timer.
    pub retransmit_timer_token: Option<TimerToken>,

    /// Timestamp (ms) when TIME_WAIT entered (for 2×MSL expiry).
    pub time_wait_start_ms: u64,

    /// Timer token for the TIME_WAIT 2×MSL timer.
    pub time_wait_timer_token: Option<TimerToken>,

    /// Whether the connection slot is in use.
    pub active: bool,

    /// Socket table index that owns this connection.
    pub socket_idx: Option<usize>,

    pub reset_received: bool,
}

impl TcpConnection {
    pub const fn empty() -> Self {
        Self {
            tuple: TcpTuple::ZERO,
            state: TcpState::Closed,
            snd_una: 0,
            snd_nxt: 0,
            snd_wnd: 0,
            iss: 0,
            rcv_nxt: 0,
            rcv_wnd: DEFAULT_WINDOW_SIZE,
            irs: 0,
            peer_mss: DEFAULT_MSS,
            rcv_wscale: 0,
            snd_wscale: 0,
            wscale_enabled: false,
            rto_ms: INITIAL_RTO_MS,
            retransmits: 0,
            retransmit_timer_token: None,
            time_wait_start_ms: 0,
            time_wait_timer_token: None,
            active: false,
            socket_idx: None,
            reset_received: false,
        }
    }
}

// =============================================================================
// Out-of-Order Reassembly Queue (RFC 793 §3.9)
// =============================================================================

#[derive(Clone, Copy)]
struct OooEntry {
    seq: u32,
    len: u16,
    data: [u8; OOO_ENTRY_MAX],
}

impl OooEntry {
    const fn empty() -> Self {
        Self {
            seq: 0,
            len: 0,
            data: [0; OOO_ENTRY_MAX],
        }
    }

    fn end_seq(&self) -> u32 {
        self.seq.wrapping_add(self.len as u32)
    }
}

impl core::fmt::Debug for OooEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OooEntry")
            .field("seq", &self.seq)
            .field("len", &self.len)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TcpOooQueue {
    entries: [OooEntry; OOO_MAX_ENTRIES],
    count: usize,
}

impl TcpOooQueue {
    pub const fn new() -> Self {
        Self {
            entries: [OooEntry::empty(); OOO_MAX_ENTRIES],
            count: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn insert(&mut self, seq: u32, data: &[u8]) {
        if data.is_empty() || data.len() > OOO_ENTRY_MAX {
            return;
        }

        // Drop duplicates: skip if we already cover this range.
        for i in 0..self.count {
            let e = &self.entries[i];
            if e.seq == seq && e.len as usize >= data.len() {
                return;
            }
            // If new segment is fully contained within an existing entry, skip.
            if seq_ge(seq, e.seq) && seq_le(seq.wrapping_add(data.len() as u32), e.end_seq()) {
                return;
            }
        }

        if self.count >= OOO_MAX_ENTRIES {
            // Evict the entry with the highest sequence number (furthest from rcv_nxt)
            // to keep near-gap segments which are more likely to complete reassembly.
            let mut evict_idx = 0;
            let mut evict_seq = self.entries[0].seq;
            for i in 1..self.count {
                if seq_gt(self.entries[i].seq, evict_seq) {
                    evict_idx = i;
                    evict_seq = self.entries[i].seq;
                }
            }
            // Only evict if the new entry has a lower seq (closer to gap).
            if seq_ge(seq, evict_seq) {
                return;
            }
            self.entries[evict_idx] = self.entries[self.count - 1];
            self.count -= 1;
        }

        let entry = &mut self.entries[self.count];
        entry.seq = seq;
        entry.len = data.len() as u16;
        entry.data[..data.len()].copy_from_slice(data);
        self.count += 1;
    }

    /// Drain contiguous segments starting at `rcv_nxt` into `recv`.
    /// Returns the total number of bytes drained (for advancing `rcv_nxt`).
    pub fn drain_contiguous(
        &mut self,
        rcv_nxt: u32,
        recv: &mut TcpRecvState,
        now_ms: u64,
    ) -> usize {
        let mut total = 0usize;
        let mut next_seq = rcv_nxt;

        loop {
            let mut found = None;
            for i in 0..self.count {
                let e = &self.entries[i];
                // Exact match or overlapping start (segment starts at or before next_seq
                // but extends past it).
                if e.seq == next_seq {
                    found = Some(i);
                    break;
                }
                // Partial overlap: segment starts before next_seq but data extends beyond.
                if seq_lt(e.seq, next_seq) && seq_gt(e.end_seq(), next_seq) {
                    found = Some(i);
                    break;
                }
            }

            let Some(idx) = found else { break };

            let entry = self.entries[idx];
            let skip = if seq_lt(entry.seq, next_seq) {
                next_seq.wrapping_sub(entry.seq) as usize
            } else {
                0
            };
            let usable = &entry.data[skip..entry.len as usize];
            let wrote = recv.enqueue(usable, now_ms);
            if wrote == 0 {
                break; // Receive buffer full
            }
            total += wrote;
            next_seq = next_seq.wrapping_add(wrote as u32);

            self.count -= 1;
            if idx < self.count {
                self.entries[idx] = self.entries[self.count];
            }
        }

        total
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }
}

pub type TcpBuffer = RingBuffer<u8, TCP_BUFFER_SIZE>;

#[derive(Clone, Copy, Debug)]
pub struct TcpSendState {
    pub buf: TcpBuffer,
    pub inflight: usize,
    pub rto_deadline_ms: u64,
    pub needs_retransmit: bool,
}

impl TcpSendState {
    pub const fn new() -> Self {
        Self {
            buf: TcpBuffer::new_zeroed(),
            inflight: 0,
            rto_deadline_ms: 0,
            needs_retransmit: false,
        }
    }

    pub fn enqueue(&mut self, data: &[u8]) -> usize {
        self.buf.write(data)
    }

    pub fn unsent_len(&self) -> usize {
        self.buf.len().saturating_sub(self.inflight)
    }

    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    pub fn free_space(&self) -> usize {
        self.buf.free_space()
    }

    pub fn peek_unsent(&self, out: &mut [u8]) -> usize {
        self.buf.peek_at(self.inflight, out)
    }

    pub fn mark_sent(&mut self, n: usize) {
        let unsent = self.unsent_len();
        let sent = core::cmp::min(n, unsent);
        self.inflight += sent;
        if sent > 0 {
            self.needs_retransmit = false;
        }
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

    pub fn retransmit_timeout(&mut self) {
        self.inflight = 0;
        self.needs_retransmit = true;
    }

    pub fn clear(&mut self) {
        self.buf.reset();
        self.inflight = 0;
        self.rto_deadline_ms = 0;
        self.needs_retransmit = false;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TcpRecvState {
    pub buf: TcpBuffer,
    pub segments_since_ack: u8,
    pub ack_pending: bool,
    pub delayed_ack_deadline_ms: u64,
}

impl TcpRecvState {
    pub const fn new() -> Self {
        Self {
            buf: TcpBuffer::new_zeroed(),
            segments_since_ack: 0,
            ack_pending: false,
            delayed_ack_deadline_ms: 0,
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
        core::cmp::min(self.buf.free_space(), u16::MAX as usize) as u16
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

#[derive(Clone, Copy, Debug)]
pub struct TcpBufferPair {
    pub send: TcpSendState,
    pub recv: TcpRecvState,
    pub ooo: TcpOooQueue,
}

impl TcpBufferPair {
    pub const fn new() -> Self {
        Self {
            send: TcpSendState::new(),
            recv: TcpRecvState::new(),
            ooo: TcpOooQueue::new(),
        }
    }

    pub fn clear(&mut self) {
        self.send.clear();
        self.recv.clear();
        self.ooo.clear();
    }
}

// =============================================================================
// Sequence number arithmetic (RFC 793 §3.3)
// =============================================================================

/// `a` is before `b` in sequence space (wrapping comparison).
#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// `a` is before or equal to `b` in sequence space.
#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// `a` is after `b` in sequence space.
#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// `a` is after or equal to `b` in sequence space.
#[inline]
pub fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

// =============================================================================
// ISN (Initial Sequence Number) generator
// =============================================================================

/// Simple monotonic ISN counter — incremented by 64000 per connection (RFC 6528
/// recommends a clock-based or hash-based ISN; this is a minimal starting point).
static ISN_COUNTER: AtomicU32 = AtomicU32::new(0x4F50_534C); // "OPSL"

pub(crate) fn generate_isn() -> u32 {
    ISN_COUNTER.fetch_add(64000, Ordering::Relaxed)
}

// =============================================================================
// Ephemeral port allocator
// =============================================================================

static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

/// Allocate the next ephemeral port (49152–65535).
pub fn alloc_ephemeral_port() -> u16 {
    loop {
        let port = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        if port >= 49152 {
            return port;
        }
        // Wrapped around — reset.
        EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
    }
}

// =============================================================================
// Connection Table
// =============================================================================

/// Global TCP connection table.
static TCP_TABLE: IrqMutex<TcpConnectionTable> = IrqMutex::new(TcpConnectionTable::new());

pub struct TcpConnectionTable {
    connections: [TcpConnection; MAX_CONNECTIONS],
    buffers: [TcpBufferPair; MAX_CONNECTIONS],
}

impl TcpConnectionTable {
    pub const fn new() -> Self {
        Self {
            connections: [TcpConnection::empty(); MAX_CONNECTIONS],
            buffers: unsafe { core::mem::zeroed() },
        }
    }

    /// Find a connection matching the given tuple.  Exact match first, then
    /// wildcard listen sockets.
    pub fn find(&self, tuple: &TcpTuple) -> Option<usize> {
        // First pass: exact match.
        for (i, conn) in self.connections.iter().enumerate() {
            if conn.active
                && conn.tuple.local_ip == tuple.local_ip
                && conn.tuple.local_port == tuple.local_port
                && conn.tuple.remote_ip == tuple.remote_ip
                && conn.tuple.remote_port == tuple.remote_port
            {
                return Some(i);
            }
        }
        // Second pass: wildcard listen sockets (remote = 0).
        for (i, conn) in self.connections.iter().enumerate() {
            if conn.active
                && conn.state == TcpState::Listen
                && conn.tuple.local_port == tuple.local_port
                && (conn.tuple.local_ip == [0; 4] || conn.tuple.local_ip == tuple.local_ip)
            {
                return Some(i);
            }
        }
        None
    }

    /// Find a free slot in the table.
    fn alloc_slot(&self) -> Option<usize> {
        for (i, conn) in self.connections.iter().enumerate() {
            if !conn.active {
                return Some(i);
            }
        }
        None
    }

    /// Count of active connections.
    pub fn active_count(&self) -> usize {
        self.connections.iter().filter(|c| c.active).count()
    }

    /// Check if a local port is already bound.
    pub fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.connections.iter().any(|c| {
            c.active
                && c.tuple.local_port == local_port
                && (c.tuple.local_ip == [0; 4]
                    || local_ip == [0; 4]
                    || c.tuple.local_ip == local_ip)
        })
    }

    /// Get a reference to a connection by index.
    pub fn get(&self, idx: usize) -> Option<&TcpConnection> {
        self.connections.get(idx).filter(|c| c.active)
    }

    /// Get a mutable reference to a connection by index.
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut TcpConnection> {
        self.connections.get_mut(idx).filter(|c| c.active)
    }

    /// Release a connection slot.
    pub fn release(&mut self, idx: usize) {
        if let Some(conn) = self.connections.get_mut(idx) {
            if let Some(token) = conn.retransmit_timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
            if let Some(token) = conn.time_wait_timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
            *conn = TcpConnection::empty();
        }
        if let Some(bufs) = self.buffers.get_mut(idx) {
            bufs.clear();
        }
    }
}

// =============================================================================
// Public API — connection lifecycle
// =============================================================================

/// Open an active connection (client: SYN → SYN_SENT).
///
/// Returns `(connection_index, outgoing_SYN_segment)`.
pub fn tcp_connect(
    local_ip: [u8; 4],
    remote_ip: [u8; 4],
    remote_port: u16,
) -> Result<(usize, TcpOutSegment), TcpError> {
    let local_port = alloc_ephemeral_port();
    let iss = generate_isn();

    let mut table = TCP_TABLE.lock();

    let idx = table.alloc_slot().ok_or(TcpError::TableFull)?;

    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip,
        remote_port,
    };

    let conn = &mut table.connections[idx];
    conn.tuple = tuple;
    conn.state = TcpState::SynSent;
    conn.iss = iss;
    conn.snd_una = iss;
    conn.snd_nxt = iss.wrapping_add(1); // SYN consumes one sequence number
    conn.snd_wnd = 0;
    conn.rcv_wnd = DEFAULT_WINDOW_SIZE;
    conn.peer_mss = DEFAULT_MSS;
    conn.rto_ms = INITIAL_RTO_MS;
    conn.retransmits = 0;
    conn.active = true;

    klog_debug!(
        "tcp: CONNECT {}:{} -> {}:{} ISS={} idx={}",
        local_ip[0],
        local_ip[1],
        local_port,
        remote_ip[0],
        remote_port,
        idx
    );

    let wscale = our_window_scale();
    conn.rcv_wscale = wscale;

    let seg = TcpOutSegment {
        tuple,
        seq_num: iss,
        ack_num: 0,
        flags: TCP_FLAG_SYN,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: DEFAULT_MSS,
        wscale: if wscale > 0 { wscale } else { 255 },
    };

    Ok((idx, seg))
}

/// Open a passive connection (server: → LISTEN).
///
/// Binds to `local_ip:local_port` and waits for incoming SYNs.
pub fn tcp_listen(local_ip: [u8; 4], local_port: u16) -> Result<usize, TcpError> {
    let mut table = TCP_TABLE.lock();

    if table.port_in_use(local_ip, local_port) {
        return Err(TcpError::AddrInUse);
    }

    let idx = table.alloc_slot().ok_or(TcpError::TableFull)?;

    let conn = &mut table.connections[idx];
    conn.tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    conn.state = TcpState::Listen;
    conn.rcv_wnd = DEFAULT_WINDOW_SIZE;
    conn.active = true;

    klog_debug!("tcp: LISTEN on port {} idx={}", local_port, idx);
    Ok(idx)
}

/// Close a connection (initiate graceful teardown).
///
/// Returns the outgoing FIN segment if one should be sent.
pub fn tcp_close(idx: usize) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = TCP_TABLE.lock();
    let conn = table.get_mut(idx).ok_or(TcpError::NotFound)?;

    match conn.state {
        TcpState::Closed => Err(TcpError::InvalidState),
        TcpState::Listen | TcpState::SynSent => {
            // No connection established — just release.
            let state = conn.state;
            table.release(idx);
            klog_debug!("tcp: CLOSE idx={} from {} — released", idx, state.name());
            Ok(None)
        }
        TcpState::SynReceived | TcpState::Established => {
            // Send FIN, move to FIN_WAIT_1.
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1); // FIN consumes one sequence number
            let prev = conn.state;
            conn.state = TcpState::FinWait1;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };

            klog_debug!(
                "tcp: CLOSE idx={} {} -> FIN_WAIT_1, FIN seq={}",
                idx,
                prev.name(),
                seq
            );
            Ok(Some(seg))
        }
        TcpState::CloseWait => {
            // Peer already sent FIN — send our FIN, move to LAST_ACK.
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1);
            conn.state = TcpState::LastAck;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };

            klog_debug!(
                "tcp: CLOSE idx={} CLOSE_WAIT -> LAST_ACK, FIN seq={}",
                idx,
                seq
            );
            Ok(Some(seg))
        }
        // Already closing — ignore.
        TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::Closing
        | TcpState::LastAck
        | TcpState::TimeWait => {
            klog_debug!(
                "tcp: CLOSE idx={} already closing ({})",
                idx,
                conn.state.name()
            );
            Ok(None)
        }
    }
}

/// Abort a connection (send RST, release immediately).
pub fn tcp_abort(idx: usize) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = TCP_TABLE.lock();
    let conn = table.get_mut(idx).ok_or(TcpError::NotFound)?;

    let seg = if conn.state != TcpState::Listen && conn.state != TcpState::Closed {
        Some(TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: 0,
            flags: TCP_FLAG_RST,
            window_size: 0,
            mss: 0,
            wscale: 255,
        })
    } else {
        None
    };

    klog_debug!("tcp: ABORT idx={} from {}", idx, conn.state.name());
    table.release(idx);
    Ok(seg)
}

/// Shutdown the write half of a connection (send FIN without releasing).
///
/// Like `tcp_close`, but keeps the connection alive for further reading.
/// Transitions: Established → FinWait1, CloseWait → LastAck.
pub fn tcp_shutdown_write(idx: usize) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = TCP_TABLE.lock();
    let conn = table.get_mut(idx).ok_or(TcpError::NotFound)?;

    match conn.state {
        TcpState::Established | TcpState::SynReceived => {
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1);
            let prev = conn.state;
            conn.state = TcpState::FinWait1;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };

            klog_debug!(
                "tcp: SHUTDOWN_WR idx={} {} -> FIN_WAIT_1, FIN seq={}",
                idx,
                prev.name(),
                seq
            );
            Ok(Some(seg))
        }
        TcpState::CloseWait => {
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1);
            conn.state = TcpState::LastAck;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };

            klog_debug!(
                "tcp: SHUTDOWN_WR idx={} CLOSE_WAIT -> LAST_ACK, FIN seq={}",
                idx,
                seq
            );
            Ok(Some(seg))
        }
        // Already sent FIN or not connected — no-op.
        TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::Closing
        | TcpState::LastAck
        | TcpState::TimeWait => {
            klog_debug!(
                "tcp: SHUTDOWN_WR idx={} already closing ({})",
                idx,
                conn.state.name()
            );
            Ok(None)
        }
        TcpState::Closed | TcpState::Listen | TcpState::SynSent => Err(TcpError::InvalidState),
    }
}

/// Discard all data in the receive buffer (for SHUT_RD).
pub fn tcp_recv_discard(idx: usize) {
    let mut table = TCP_TABLE.lock();
    if table.get(idx).is_some() {
        table.buffers[idx].recv.clear();
        klog_debug!("tcp: RECV_DISCARD idx={} — recv buffer cleared", idx);
    }
}

/// Check whether the peer has closed their write half (sent FIN).
///
/// Returns true when the connection is in CloseWait, LastAck, Closing,
/// TimeWait, or Closed — i.e. the peer's FIN has been received.
pub fn tcp_is_peer_closed(idx: usize) -> bool {
    let table = TCP_TABLE.lock();
    table
        .get(idx)
        .map(|c| {
            matches!(
                c.state,
                TcpState::CloseWait
                    | TcpState::LastAck
                    | TcpState::Closing
                    | TcpState::TimeWait
                    | TcpState::Closed
            )
        })
        .unwrap_or(true)
}

pub fn tcp_is_reset(idx: usize) -> bool {
    let table = TCP_TABLE.lock();
    table.get(idx).map(|c| c.reset_received).unwrap_or(true)
}

// =============================================================================
// Incoming segment processing
// =============================================================================

/// Result of processing an incoming TCP segment.
#[derive(Clone, Debug)]
pub struct TcpInputResult {
    /// Outgoing segment(s) to send in response (ACK, SYN+ACK, RST, etc.).
    pub response: Option<TcpOutSegment>,
    /// Index of the connection this segment was processed against.
    pub conn_idx: Option<usize>,
    /// New state after processing.
    pub new_state: Option<TcpState>,
    /// If a new connection was accepted from a listen socket, its index.
    pub accepted_idx: Option<usize>,
    /// If the connection was reset.
    pub reset: bool,
}

impl TcpInputResult {
    const fn empty() -> Self {
        Self {
            response: None,
            conn_idx: None,
            new_state: None,
            accepted_idx: None,
            reset: false,
        }
    }
}

/// Build a RST segment in response to an unexpected incoming segment.
fn build_rst_for(hdr: &TcpHeader, local_ip: [u8; 4], remote_ip: [u8; 4]) -> TcpOutSegment {
    let (seq, ack, flags) = if hdr.is_ack() {
        // RST with seq = incoming ACK number.
        (hdr.ack_num, 0u32, TCP_FLAG_RST)
    } else {
        // RST+ACK with ack = incoming SEQ + segment length.
        let seg_len = if hdr.is_syn() { 1u32 } else { 0u32 };
        (
            0u32,
            hdr.seq_num.wrapping_add(seg_len),
            TCP_FLAG_RST | TCP_FLAG_ACK,
        )
    };

    TcpOutSegment {
        tuple: TcpTuple {
            local_ip,
            local_port: hdr.dst_port,
            remote_ip,
            remote_port: hdr.src_port,
        },
        seq_num: seq,
        ack_num: ack,
        flags,
        window_size: 0,
        mss: 0,
        wscale: 255,
    }
}

/// Process an incoming TCP segment.
///
/// `src_ip` / `dst_ip` are from the IPv4 header.
/// `tcp_data` is the raw TCP segment (header + payload).
/// `options` is the TCP options region (may be empty).
/// `now_ms` is the current monotonic time in milliseconds.
///
/// Returns instructions for the caller (segments to send, state changes, etc.).
pub fn tcp_input(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> TcpInputResult {
    let incoming_tuple = TcpTuple {
        local_ip: dst_ip,
        local_port: hdr.dst_port,
        remote_ip: src_ip,
        remote_port: hdr.src_port,
    };

    let mut table = TCP_TABLE.lock();

    let conn_idx = match table.find(&incoming_tuple) {
        Some(idx) => idx,
        None => {
            // No matching connection — send RST unless it's already a RST.
            if hdr.is_rst() {
                return TcpInputResult::empty();
            }
            return TcpInputResult {
                response: Some(build_rst_for(hdr, dst_ip, src_ip)),
                ..TcpInputResult::empty()
            };
        }
    };

    let conn_state = table.connections[conn_idx].state;

    match conn_state {
        TcpState::Closed => {
            // Should not happen (slot would be inactive).
            TcpInputResult::empty()
        }

        TcpState::Listen => process_listen(&mut table, conn_idx, hdr, options, &incoming_tuple),

        TcpState::SynSent => process_syn_sent(&mut table, conn_idx, hdr, options),

        TcpState::SynReceived => process_syn_received(&mut table, conn_idx, hdr),

        TcpState::Established
        | TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::CloseWait
        | TcpState::Closing
        | TcpState::LastAck => {
            process_established_and_closing(&mut table, conn_idx, hdr, payload, now_ms)
        }

        TcpState::TimeWait => process_time_wait(&mut table, conn_idx, hdr, now_ms),
    }
}

// =============================================================================
// Per-state processing
// =============================================================================

/// LISTEN state: expecting SYN.
fn process_listen(
    table: &mut TcpConnectionTable,
    listen_idx: usize,
    hdr: &TcpHeader,
    options: &[u8],
    incoming_tuple: &TcpTuple,
) -> TcpInputResult {
    // RST in LISTEN — ignore.
    if hdr.is_rst() {
        return TcpInputResult::empty();
    }

    // ACK to a LISTEN — send RST.
    if hdr.is_ack() {
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple: TcpTuple {
                    local_ip: incoming_tuple.local_ip,
                    local_port: incoming_tuple.local_port,
                    remote_ip: incoming_tuple.remote_ip,
                    remote_port: incoming_tuple.remote_port,
                },
                seq_num: hdr.ack_num,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
                wscale: 255,
            }),
            conn_idx: Some(listen_idx),
            ..TcpInputResult::empty()
        };
    }

    // SYN — create a new connection in SYN_RECEIVED.
    if !hdr.is_syn() {
        return TcpInputResult::empty();
    }

    let new_idx = match table.alloc_slot() {
        Some(i) => i,
        None => return TcpInputResult::empty(), // Table full, drop silently.
    };

    let iss = generate_isn();
    let peer_mss = parse_mss_option(options).unwrap_or(DEFAULT_MSS);

    let child = &mut table.connections[new_idx];
    child.tuple = *incoming_tuple;
    child.state = TcpState::SynReceived;
    child.iss = iss;
    child.snd_una = iss;
    child.snd_nxt = iss.wrapping_add(1);
    child.irs = hdr.seq_num;
    child.rcv_nxt = hdr.seq_num.wrapping_add(1);
    child.snd_wnd = hdr.window_size as u32;
    child.rcv_wnd = DEFAULT_WINDOW_SIZE;
    child.peer_mss = peer_mss;
    child.rto_ms = INITIAL_RTO_MS;
    child.retransmits = 0;
    child.active = true;

    klog_debug!(
        "tcp: LISTEN -> SYN_RECEIVED idx={} ISS={} IRS={}",
        new_idx,
        iss,
        hdr.seq_num
    );

    let seg = TcpOutSegment {
        tuple: child.tuple,
        seq_num: iss,
        ack_num: child.rcv_nxt,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: DEFAULT_MSS,
        wscale: 255,
    };

    TcpInputResult {
        response: Some(seg),
        conn_idx: Some(listen_idx),
        new_state: Some(TcpState::SynReceived),
        accepted_idx: Some(new_idx),
        reset: false,
    }
}

/// SYN_SENT state: expecting SYN+ACK (or simultaneous open SYN).
fn process_syn_sent(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
    options: &[u8],
) -> TcpInputResult {
    let conn = &table.connections[idx];
    let iss = conn.iss;

    // Step 1: Check ACK.
    if hdr.is_ack() {
        // ACK must acknowledge our SYN.
        if seq_le(hdr.ack_num, iss) || seq_gt(hdr.ack_num, conn.snd_nxt) {
            // Bad ACK — send RST unless incoming is RST.
            if hdr.is_rst() {
                return TcpInputResult::empty();
            }
            return TcpInputResult {
                response: Some(TcpOutSegment {
                    tuple: conn.tuple,
                    seq_num: hdr.ack_num,
                    ack_num: 0,
                    flags: TCP_FLAG_RST,
                    window_size: 0,
                    mss: 0,
                    wscale: 255,
                }),
                conn_idx: Some(idx),
                ..TcpInputResult::empty()
            };
        }
    }

    // Step 2: Check RST.
    if hdr.is_rst() {
        if hdr.is_ack() {
            // Valid RST — connection refused.
            klog_debug!(
                "tcp: SYN_SENT idx={} — RST received, connection refused",
                idx
            );
            table.release(idx);
            return TcpInputResult {
                conn_idx: Some(idx),
                new_state: Some(TcpState::Closed),
                reset: true,
                ..TcpInputResult::empty()
            };
        }
        return TcpInputResult::empty();
    }

    // Step 3: Check SYN.
    if !hdr.is_syn() {
        return TcpInputResult::empty();
    }

    let opts = parse_tcp_options(options);
    let peer_mss = opts.mss.unwrap_or(DEFAULT_MSS);
    let conn = &mut table.connections[idx];
    conn.irs = hdr.seq_num;
    conn.rcv_nxt = hdr.seq_num.wrapping_add(1);
    conn.peer_mss = peer_mss;

    // RFC 7323: window scaling is enabled only if both sides offered it.
    if let Some(peer_shift) = opts.window_scale {
        conn.snd_wscale = peer_shift;
        conn.wscale_enabled = true;
        conn.snd_wnd = (hdr.window_size as u32) << peer_shift;
    } else {
        conn.snd_wnd = hdr.window_size as u32;
        conn.wscale_enabled = false;
        conn.rcv_wscale = 0;
    }

    if hdr.is_ack() {
        // SYN+ACK — our SYN was acknowledged.
        conn.snd_una = hdr.ack_num;
    }

    if seq_gt(conn.snd_una, conn.iss) {
        // Our SYN has been ACKed → ESTABLISHED.
        conn.state = TcpState::Established;
        conn.retransmits = 0;

        klog_debug!("tcp: SYN_SENT -> ESTABLISHED idx={} IRS={}", idx, conn.irs);

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
            wscale: 255,
        };

        TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::Established),
            accepted_idx: None,
            reset: false,
        }
    } else {
        // Simultaneous open: SYN without ACK → SYN_RECEIVED.
        conn.state = TcpState::SynReceived;

        klog_debug!(
            "tcp: SYN_SENT -> SYN_RECEIVED idx={} (simultaneous open)",
            idx
        );

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.iss,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: DEFAULT_MSS,
            wscale: 255,
        };

        TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::SynReceived),
            accepted_idx: None,
            reset: false,
        }
    }
}

/// SYN_RECEIVED state: expecting ACK to complete handshake.
fn process_syn_received(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
) -> TcpInputResult {
    let conn = &table.connections[idx];

    // RST — abort.
    if hdr.is_rst() {
        klog_debug!("tcp: SYN_RECEIVED idx={} — RST, closing", idx);
        table.release(idx);
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Must have ACK.
    if !hdr.is_ack() {
        return TcpInputResult::empty();
    }

    // Validate ACK range.
    if seq_lt(hdr.ack_num, conn.snd_una) || seq_gt(hdr.ack_num, conn.snd_nxt) {
        // Bad ACK — send RST.
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple: conn.tuple,
                seq_num: hdr.ack_num,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
                wscale: 255,
            }),
            conn_idx: Some(idx),
            ..TcpInputResult::empty()
        };
    }

    // Valid ACK → ESTABLISHED.
    let conn = &mut table.connections[idx];
    conn.snd_una = hdr.ack_num;
    conn.snd_wnd = hdr.window_size as u32;
    conn.state = TcpState::Established;
    conn.retransmits = 0;

    klog_debug!("tcp: SYN_RECEIVED -> ESTABLISHED idx={}", idx);

    TcpInputResult {
        response: None,
        conn_idx: Some(idx),
        new_state: Some(TcpState::Established),
        accepted_idx: None,
        reset: false,
    }
}

/// ESTABLISHED and closing states: main segment processing.
fn process_established_and_closing(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
    payload: &[u8],
    now_ms: u64,
) -> TcpInputResult {
    let current_state = table.connections[idx].state;

    // Step 1: Check RST.
    if hdr.is_rst() {
        klog_debug!("tcp: {} idx={} — RST received", current_state.name(), idx);
        if let Some(token) = table.connections[idx].retransmit_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        table.connections[idx].state = TcpState::Closed;
        table.connections[idx].reset_received = true;
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Step 2: Check SYN (unexpected in established+ states → RST).
    if hdr.is_syn() {
        let tuple = table.connections[idx].tuple;
        let snd_nxt = table.connections[idx].snd_nxt;
        klog_debug!(
            "tcp: {} idx={} — unexpected SYN, sending RST",
            current_state.name(),
            idx
        );
        table.release(idx);
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple,
                seq_num: snd_nxt,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
                wscale: 255,
            }),
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            accepted_idx: None,
            reset: true,
        };
    }

    // Step 3: Check ACK.
    if !hdr.is_ack() {
        return TcpInputResult::empty();
    }

    // Update snd_una / snd_wnd from the ACK.
    let old_snd_una = table.connections[idx].snd_una;
    let mut ack_advanced = false;
    {
        let conn = &mut table.connections[idx];
        if seq_gt(hdr.ack_num, conn.snd_una) && seq_le(hdr.ack_num, conn.snd_nxt) {
            conn.snd_una = hdr.ack_num;
            conn.snd_wnd = if conn.wscale_enabled {
                (hdr.window_size as u32) << conn.snd_wscale
            } else {
                hdr.window_size as u32
            };
            ack_advanced = true;
        }
    }

    if ack_advanced && seq_gt(hdr.ack_num, old_snd_una) {
        let acked = hdr.ack_num.wrapping_sub(old_snd_una) as usize;
        table.buffers[idx].send.process_ack(acked);
        if table.buffers[idx].send.inflight == 0 {
            if let Some(token) = table.connections[idx].retransmit_timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
            table.connections[idx].retransmits = 0;
        } else {
            if let Some(token) = table.connections[idx].retransmit_timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
            let rto_ms = table.connections[idx].rto_ms;
            let delay_ticks = ((rto_ms as u64) / 10).max(1);
            let new_token =
                NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, idx as u32);
            table.connections[idx].retransmit_timer_token = Some(new_token);
            // Note: do NOT update rto_deadline_ms here — the polling-based
            // tcp_retransmit_check() also reads it and needs the original deadline.
            // The timer wheel tracks its own delay independently.
        }
    }

    let mut accepted_payload_len = 0usize;
    if !payload.is_empty()
        && matches!(
            current_state,
            TcpState::Established | TcpState::CloseWait | TcpState::FinWait1 | TcpState::FinWait2
        )
    {
        let expected_seq = table.connections[idx].rcv_nxt;
        if hdr.seq_num != expected_seq {
            // Out-of-order segment: buffer it if it's ahead of rcv_nxt.
            if seq_gt(hdr.seq_num, expected_seq) {
                klog_debug!(
                    "tcp: OOO segment idx={} seq={} expected={} len={}",
                    idx,
                    hdr.seq_num,
                    expected_seq,
                    payload.len()
                );
                table.buffers[idx].ooo.insert(hdr.seq_num, payload);
            }
            let conn = &table.connections[idx];
            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: conn.snd_nxt,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_ACK,
                window_size: table.buffers[idx].recv.window(),
                mss: 0,
                wscale: 255,
            };
            return TcpInputResult {
                response: Some(seg),
                conn_idx: Some(idx),
                new_state: Some(conn.state),
                accepted_idx: None,
                reset: false,
            };
        }

        let wrote = table.buffers[idx].recv.enqueue(payload, now_ms);
        accepted_payload_len = wrote;
        {
            let conn = &mut table.connections[idx];
            conn.rcv_nxt = conn.rcv_nxt.wrapping_add(wrote as u32);
        }

        // Drain any OOO segments that are now contiguous with rcv_nxt.
        if !table.buffers[idx].ooo.is_empty() {
            let rcv_nxt = table.connections[idx].rcv_nxt;
            let drained = table.buffers[idx].ooo.drain_contiguous(
                rcv_nxt,
                &mut table.buffers[idx].recv,
                now_ms,
            );
            if drained > 0 {
                klog_debug!("tcp: OOO drain idx={} bytes={}", idx, drained);
                accepted_payload_len += drained;
                table.connections[idx].rcv_nxt =
                    table.connections[idx].rcv_nxt.wrapping_add(drained as u32);
            }
        }

        let recv_window = table.buffers[idx].recv.window();
        table.connections[idx].rcv_wnd = recv_window;

        if table.buffers[idx].recv.should_ack_now(now_ms) {
            let conn = &table.connections[idx];
            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: conn.snd_nxt,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };
            table.buffers[idx].recv.ack_sent();
            return TcpInputResult {
                response: Some(seg),
                conn_idx: Some(idx),
                new_state: Some(conn.state),
                accepted_idx: None,
                reset: false,
            };
        }

        if !hdr.is_fin() {
            let state = table.connections[idx].state;
            return TcpInputResult {
                response: None,
                conn_idx: Some(idx),
                new_state: Some(state),
                accepted_idx: None,
                reset: false,
            };
        }
    }

    // State-specific ACK processing.
    match current_state {
        TcpState::FinWait1 => {
            // If our FIN is acknowledged.
            if hdr.ack_num == table.connections[idx].snd_nxt {
                if hdr.is_fin() {
                    // Simultaneous close: FIN+ACK acks our FIN and carries theirs.
                    let conn = &mut table.connections[idx];
                    conn.rcv_nxt = hdr.seq_num.wrapping_add(1);
                    conn.state = TcpState::TimeWait;
                    conn.time_wait_start_ms = now_ms;
                    if let Some(token) = conn.retransmit_timer_token.take() {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                    if let Some(token) = conn.time_wait_timer_token.take() {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                    let tw_delay_ticks = ((TIME_WAIT_MS as u64) / 10).max(1);
                    let tw_token = NET_TIMER_WHEEL.schedule(
                        tw_delay_ticks,
                        TimerKind::TcpTimeWait,
                        idx as u32,
                    );
                    conn.time_wait_timer_token = Some(tw_token);
                    klog_debug!(
                        "tcp: FIN_WAIT_1 -> TIME_WAIT idx={} (simultaneous close)",
                        idx
                    );

                    let seg = TcpOutSegment {
                        tuple: conn.tuple,
                        seq_num: conn.snd_nxt,
                        ack_num: conn.rcv_nxt,
                        flags: TCP_FLAG_ACK,
                        window_size: conn.rcv_wnd,
                        mss: 0,
                        wscale: 255,
                    };
                    return TcpInputResult {
                        response: Some(seg),
                        conn_idx: Some(idx),
                        new_state: Some(TcpState::TimeWait),
                        accepted_idx: None,
                        reset: false,
                    };
                }
                let conn = &mut table.connections[idx];
                conn.state = TcpState::FinWait2;
                klog_debug!("tcp: FIN_WAIT_1 -> FIN_WAIT_2 idx={}", idx);
            }
        }
        TcpState::Closing => {
            if hdr.ack_num == table.connections[idx].snd_nxt {
                let conn = &mut table.connections[idx];
                conn.state = TcpState::TimeWait;
                conn.time_wait_start_ms = now_ms;
                if let Some(token) = conn.retransmit_timer_token.take() {
                    NET_TIMER_WHEEL.cancel(token);
                }
                if let Some(token) = conn.time_wait_timer_token.take() {
                    NET_TIMER_WHEEL.cancel(token);
                }
                let tw_delay_ticks = ((TIME_WAIT_MS as u64) / 10).max(1);
                let tw_token =
                    NET_TIMER_WHEEL.schedule(tw_delay_ticks, TimerKind::TcpTimeWait, idx as u32);
                conn.time_wait_timer_token = Some(tw_token);
                klog_debug!("tcp: CLOSING -> TIME_WAIT idx={}", idx);
                return TcpInputResult {
                    response: None,
                    conn_idx: Some(idx),
                    new_state: Some(TcpState::TimeWait),
                    accepted_idx: None,
                    reset: false,
                };
            }
        }
        TcpState::LastAck => {
            if hdr.ack_num == table.connections[idx].snd_nxt {
                klog_debug!("tcp: LAST_ACK -> CLOSED idx={}", idx);
                table.release(idx);
                return TcpInputResult {
                    conn_idx: Some(idx),
                    new_state: Some(TcpState::Closed),
                    accepted_idx: None,
                    reset: false,
                    response: None,
                };
            }
        }
        _ => {}
    }

    // Step 4: Check FIN.
    if hdr.is_fin() {
        let conn = &mut table.connections[idx];
        let fin_seq = hdr.seq_num.wrapping_add(accepted_payload_len as u32);
        if fin_seq != conn.rcv_nxt {
            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: conn.snd_nxt,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
                wscale: 255,
            };
            return TcpInputResult {
                response: Some(seg),
                conn_idx: Some(idx),
                new_state: Some(conn.state),
                accepted_idx: None,
                reset: false,
            };
        }
        conn.rcv_nxt = conn.rcv_nxt.wrapping_add(1);

        let new_state = match current_state {
            TcpState::Established => {
                conn.state = TcpState::CloseWait;
                klog_debug!("tcp: ESTABLISHED -> CLOSE_WAIT idx={}", idx);
                TcpState::CloseWait
            }
            TcpState::FinWait1 => {
                // Our FIN not yet acked + peer FIN → CLOSING.
                conn.state = TcpState::Closing;
                klog_debug!("tcp: FIN_WAIT_1 -> CLOSING idx={}", idx);
                TcpState::Closing
            }
            TcpState::FinWait2 => {
                conn.state = TcpState::TimeWait;
                conn.time_wait_start_ms = now_ms;
                if let Some(token) = conn.retransmit_timer_token.take() {
                    NET_TIMER_WHEEL.cancel(token);
                }
                if let Some(token) = conn.time_wait_timer_token.take() {
                    NET_TIMER_WHEEL.cancel(token);
                }
                let tw_delay_ticks = ((TIME_WAIT_MS as u64) / 10).max(1);
                let tw_token =
                    NET_TIMER_WHEEL.schedule(tw_delay_ticks, TimerKind::TcpTimeWait, idx as u32);
                conn.time_wait_timer_token = Some(tw_token);
                klog_debug!("tcp: FIN_WAIT_2 -> TIME_WAIT idx={}", idx);
                TcpState::TimeWait
            }
            other => other, // FIN in other states — just ACK.
        };

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
            wscale: 255,
        };

        return TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(new_state),
            accepted_idx: None,
            reset: false,
        };
    }

    TcpInputResult {
        response: None,
        conn_idx: Some(idx),
        new_state: Some(table.connections[idx].state),
        accepted_idx: None,
        reset: false,
    }
}

/// TIME_WAIT state: handle retransmitted FIN.
fn process_time_wait(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
    now_ms: u64,
) -> TcpInputResult {
    let conn = &table.connections[idx];

    if hdr.is_rst() {
        table.release(idx);
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Retransmitted FIN — re-ACK and restart timer.
    if hdr.is_fin() {
        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
            wscale: 255,
        };
        let conn = &mut table.connections[idx];
        conn.time_wait_start_ms = now_ms;
        if let Some(token) = conn.time_wait_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        let tw_delay_ticks = ((TIME_WAIT_MS as u64) / 10).max(1);
        let tw_token = NET_TIMER_WHEEL.schedule(tw_delay_ticks, TimerKind::TcpTimeWait, idx as u32);
        conn.time_wait_timer_token = Some(tw_token);

        return TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::TimeWait),
            accepted_idx: None,
            reset: false,
        };
    }

    TcpInputResult::empty()
}

// =============================================================================
// Timer-driven maintenance
// =============================================================================

/// Expire TIME_WAIT connections whose 2×MSL has elapsed.
///
/// Call periodically from a timer context.  Returns the number of connections
/// reaped.
pub fn tcp_timer_tick(now_ms: u64) -> usize {
    let mut table = TCP_TABLE.lock();
    let mut reaped = 0usize;
    for i in 0..MAX_CONNECTIONS {
        let conn = &table.connections[i];
        if conn.active
            && conn.state == TcpState::TimeWait
            && now_ms.saturating_sub(conn.time_wait_start_ms) >= TIME_WAIT_MS
        {
            klog_debug!("tcp: TIME_WAIT expired idx={}", i);
            table.release(i);
            reaped += 1;
        }
    }
    reaped
}

/// Handle a retransmit timer firing for connection `conn_id`.
///
/// Validates the connection still exists and has unacknowledged in-flight data.
/// If valid, updates retransmit state and schedules the next retransmit timer.
/// Returns the connection index so the caller can drive retransmission send.
pub fn tcp_on_retransmit(conn_id: u32) -> Option<usize> {
    let mut table = TCP_TABLE.lock();
    let idx = conn_id as usize;
    if idx >= MAX_CONNECTIONS {
        return None;
    }

    let conn = table.connections.get(idx)?;
    let send_state = matches!(
        conn.state,
        TcpState::Established
            | TcpState::CloseWait
            | TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::LastAck
    );
    if !conn.active || !send_state || table.buffers[idx].send.inflight == 0 {
        return None;
    }

    table.connections[idx].retransmits = table.connections[idx].retransmits.saturating_add(1);
    if table.connections[idx].retransmits > MAX_RETRANSMITS {
        klog_debug!(
            "tcp: retransmit timeout idx={} conn_id={} retransmits={} -> releasing",
            idx,
            conn_id,
            table.connections[idx].retransmits
        );
        table.release(idx);
        return None;
    }

    table.buffers[idx].send.retransmit_timeout();
    table.connections[idx].snd_nxt = table.connections[idx].snd_una;
    table.connections[idx].rto_ms =
        core::cmp::min(table.connections[idx].rto_ms.saturating_mul(2), MAX_RTO_MS);

    if let Some(token) = table.connections[idx].retransmit_timer_token.take() {
        NET_TIMER_WHEEL.cancel(token);
    }

    let delay_ticks = ((table.connections[idx].rto_ms as u64) / 10).max(1);
    let token = NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, conn_id);
    table.connections[idx].retransmit_timer_token = Some(token);

    let now_ms = slopos_lib::clock::uptime_ms();
    table.buffers[idx].send.rto_deadline_ms =
        now_ms.saturating_add(table.connections[idx].rto_ms as u64);

    klog_debug!(
        "tcp: retransmit fired idx={} conn_id={} rto_ms={} retransmits={}",
        idx,
        conn_id,
        table.connections[idx].rto_ms,
        table.connections[idx].retransmits
    );

    Some(idx)
}

/// Handle a TIME_WAIT timer expiry for connection `conn_id`.
///
/// Validates the connection is still active and in TIME_WAIT before release.
pub fn tcp_on_time_wait_expire(conn_id: u32) {
    let mut table = TCP_TABLE.lock();
    let idx = conn_id as usize;
    if idx >= MAX_CONNECTIONS {
        return;
    }

    let Some(conn) = table.connections.get(idx) else {
        return;
    };

    if conn.active && conn.state == TcpState::TimeWait {
        klog_debug!(
            "tcp: TIME_WAIT timer expired idx={} conn_id={}",
            idx,
            conn_id
        );
        table.release(idx);
    }
}

// =============================================================================
// Query helpers (for tests and upper layers)
// =============================================================================

/// Get a snapshot of a connection's state.
pub fn tcp_get_state(idx: usize) -> Option<TcpState> {
    TCP_TABLE.lock().get(idx).map(|c| c.state)
}

/// Get a snapshot of a connection.
pub fn tcp_get_connection(idx: usize) -> Option<TcpConnection> {
    TCP_TABLE.lock().get(idx).copied()
}

/// Get the number of active connections.
pub fn tcp_active_count() -> usize {
    TCP_TABLE.lock().active_count()
}

/// Find a connection index by tuple.
pub fn tcp_find(tuple: &TcpTuple) -> Option<usize> {
    TCP_TABLE.lock().find(tuple)
}

/// Set or clear the socket_idx on a connection.
pub fn tcp_set_socket_idx(idx: usize, socket_idx: Option<usize>) {
    let mut table = TCP_TABLE.lock();
    if let Some(conn) = table.get_mut(idx) {
        conn.socket_idx = socket_idx;
    }
}

/// Write data into a connection's send buffer.
/// Returns the number of bytes written (may be less than data.len() if buffer is full).
pub fn tcp_send(idx: usize, data: &[u8]) -> Result<usize, TcpError> {
    let mut table = TCP_TABLE.lock();
    let state = table.get(idx).ok_or(TcpError::NotFound)?.state;
    if !matches!(state, TcpState::Established | TcpState::CloseWait) {
        return Err(TcpError::InvalidState);
    }
    Ok(table.buffers[idx].send.enqueue(data))
}

/// Read data from a connection's receive buffer.
/// Returns the number of bytes read.
pub fn tcp_recv(idx: usize, out: &mut [u8]) -> Result<usize, TcpError> {
    let mut table = TCP_TABLE.lock();
    if table.get(idx).is_none() {
        return Err(TcpError::NotFound);
    }

    let read = table.buffers[idx].recv.dequeue(out);
    if read == 0
        && table.buffers[idx].recv.available() == 0
        && table.connections[idx].reset_received
    {
        return Err(TcpError::ConnectionReset);
    }

    let recv_window = table.buffers[idx].recv.window();
    if let Some(conn) = table.get_mut(idx) {
        conn.rcv_wnd = recv_window;
    }
    Ok(read)
}

/// Generate the next outgoing data segment for a connection.
/// Fills `payload_buf` with payload data. Returns (header_info, payload_len) or None.
/// Caller should call repeatedly until None.
pub fn tcp_poll_transmit(
    idx: usize,
    payload_buf: &mut [u8],
    now_ms: u64,
) -> Option<(TcpOutSegment, usize)> {
    let mut table = TCP_TABLE.lock();
    let (state, tuple, seq, ack_num, rto_ms, peer_mss, snd_wnd) = {
        let conn = table.get(idx)?;
        (
            conn.state,
            conn.tuple,
            conn.snd_nxt,
            conn.rcv_nxt,
            conn.rto_ms as u64,
            conn.peer_mss as usize,
            conn.snd_wnd as usize,
        )
    };

    if !matches!(
        state,
        TcpState::Established | TcpState::CloseWait | TcpState::FinWait1
    ) {
        return None;
    }

    let inflight = table.buffers[idx].send.inflight;
    let wnd_avail = snd_wnd.saturating_sub(inflight);
    let unsent = table.buffers[idx].send.unsent_len();
    let mut max_send = core::cmp::min(unsent, peer_mss);
    max_send = core::cmp::min(max_send, wnd_avail);
    max_send = core::cmp::min(max_send, payload_buf.len());

    if max_send == 0 {
        return None;
    }

    let payload_len = table.buffers[idx]
        .send
        .peek_unsent(&mut payload_buf[..max_send]);
    if payload_len == 0 {
        return None;
    }

    table.buffers[idx].send.mark_sent(payload_len);
    table.connections[idx].snd_nxt = table.connections[idx]
        .snd_nxt
        .wrapping_add(payload_len as u32);
    if table.buffers[idx].send.rto_deadline_ms == 0 {
        table.buffers[idx].send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
        if table.connections[idx].retransmit_timer_token.is_none() {
            let delay_ticks = (rto_ms / 10).max(1);
            let token = NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, idx as u32);
            table.connections[idx].retransmit_timer_token = Some(token);
        }
    }

    let seg = TcpOutSegment {
        tuple,
        seq_num: seq,
        ack_num,
        flags: TCP_FLAG_ACK | TCP_FLAG_PSH,
        window_size: table.buffers[idx].recv.window(),
        mss: 0,
        wscale: 255,
    };

    Some((seg, payload_len))
}

/// Check all connections for retransmission timeouts.
/// Returns connection index for first expired connection, or None.
/// After this returns Some, caller should call tcp_poll_transmit to get retransmit segments.
pub fn tcp_retransmit_check(now_ms: u64) -> Option<usize> {
    let mut table = TCP_TABLE.lock();

    for idx in 0..MAX_CONNECTIONS {
        if !table.connections[idx].active {
            continue;
        }

        let send = &table.buffers[idx].send;
        if send.inflight == 0 || send.rto_deadline_ms == 0 || now_ms < send.rto_deadline_ms {
            continue;
        }

        {
            let conn = &mut table.connections[idx];
            conn.retransmits = conn.retransmits.saturating_add(1);
            if conn.retransmits > MAX_RETRANSMITS {
                conn.active = false;
            }

            conn.rto_ms = core::cmp::min(conn.rto_ms.saturating_mul(2), MAX_RTO_MS);
        }

        if !table.connections[idx].active {
            table.release(idx);
            continue;
        }

        table.buffers[idx].send.retransmit_timeout();
        {
            let conn = &mut table.connections[idx];
            conn.snd_nxt = conn.snd_una;
            table.buffers[idx].send.rto_deadline_ms = now_ms.saturating_add(conn.rto_ms as u64);
        }
        return Some(idx);
    }

    None
}

/// Check all connections for pending delayed ACKs.
/// Returns (conn_idx, ack_segment) for first pending, or None.
pub fn tcp_delayed_ack_check(now_ms: u64) -> Option<(usize, TcpOutSegment)> {
    let mut table = TCP_TABLE.lock();

    for i in 0..MAX_CONNECTIONS {
        if !table.connections[i].active {
            continue;
        }

        if table.buffers[i].recv.should_ack_now(now_ms) {
            let conn = &table.connections[i];
            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: conn.snd_nxt,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_ACK,
                window_size: table.buffers[i].recv.window(),
                mss: 0,
                wscale: 255,
            };
            table.buffers[i].recv.ack_sent();
            return Some((i, seg));
        }
    }

    None
}

/// Generate a zero-window probe for a connection with snd_wnd == 0.
/// Returns probe segment or None if window is not zero or no data to send.
pub fn tcp_zero_window_probe(idx: usize, _now_ms: u64) -> Option<TcpOutSegment> {
    let table = TCP_TABLE.lock();
    let conn = table.get(idx)?;
    let send = &table.buffers[idx].send;

    if conn.snd_wnd != 0 || send.buffered_len() == 0 {
        return None;
    }

    let mut byte = [0u8; 1];
    let peeked = send.peek_unsent(&mut byte);
    if peeked == 0 {
        return None;
    }

    Some(TcpOutSegment {
        tuple: conn.tuple,
        seq_num: conn.snd_nxt,
        ack_num: conn.rcv_nxt,
        flags: TCP_FLAG_ACK | TCP_FLAG_PSH,
        window_size: table.buffers[idx].recv.window(),
        mss: 0,
        wscale: 255,
    })
}

/// Available send buffer space for a connection.
pub fn tcp_send_buffer_space(idx: usize) -> usize {
    let table = TCP_TABLE.lock();
    if table.get(idx).is_some() {
        table.buffers[idx].send.free_space()
    } else {
        0
    }
}

/// Bytes available to read from a connection's receive buffer.
pub fn tcp_recv_available(idx: usize) -> usize {
    let table = TCP_TABLE.lock();
    if table.get(idx).is_some() {
        table.buffers[idx].recv.available()
    } else {
        0
    }
}

/// Whether a connection has data pending transmission.
pub fn tcp_has_pending_data(idx: usize) -> bool {
    let table = TCP_TABLE.lock();
    if table.get(idx).is_some() {
        table.buffers[idx].send.unsent_len() > 0
    } else {
        false
    }
}

/// Release all connections (for testing).
pub fn tcp_reset_all() {
    let mut table = TCP_TABLE.lock();
    for i in 0..MAX_CONNECTIONS {
        // Cancel any outstanding timer tokens before overwriting the connection.
        // Without this, timers scheduled (retransmit, TIME_WAIT)
        // remain in the wheel and fire during later test suites.
        if let Some(token) = table.connections[i].retransmit_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        if let Some(token) = table.connections[i].time_wait_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        table.connections[i] = TcpConnection::empty();
        table.buffers[i].clear();
    }
    // Reset ISN counter and ephemeral port for deterministic tests.
    ISN_COUNTER.store(0x4F50_534C, Ordering::Relaxed);
    EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}
