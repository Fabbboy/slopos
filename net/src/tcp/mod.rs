//! TCP (Transmission Control Protocol) implementation — RFC 793 + RFC 7413.
//!
//! Provides TCP header parsing/construction, one's-complement checksum with
//! IPv4 pseudo-header, a full TCP state machine, connection table, three-way
//! handshake (active and passive open), and connection teardown.
//!
//! This module is purely protocol logic — it does **not** drive the NIC
//! directly.  Higher layers wire it into the VirtIO net driver
//! for actual packet I/O.

pub mod buffer;
pub mod checksum;
pub mod clock;
pub mod cong;
pub mod header;
pub mod isn;
pub mod listener;
pub mod reasm;
pub mod rtt;
pub mod segment;
pub mod seq;

pub use buffer::{
    DELAYED_ACK_MS, DELAYED_ACK_SEGMENTS, TCP_BUFFER_SIZE, TcpBuffer, TcpBufferPair, TcpRecvState,
    TcpSendState, ZWP_INTERVAL_MS,
};
pub use checksum::{tcp_checksum, verify_checksum};
pub use header::{
    DEFAULT_MSS, DEFAULT_WINDOW_SIZE, ParsedTcpOptions, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH,
    TCP_FLAG_RST, TCP_FLAG_SYN, TCP_FLAG_URG, TCP_HEADER_LEN, TCP_HEADER_MAX_LEN, TCP_OPT_END,
    TCP_OPT_MSS, TCP_OPT_MSS_LEN, TCP_OPT_NOP, TCP_OPT_WINDOW_SCALE, TCP_OPT_WINDOW_SCALE_LEN,
    TcpHeader, build_header, our_window_scale, parse_header, parse_tcp_options, write_header,
    write_mss_option, write_window_scale_option,
};
pub use reasm::TcpOooQueue;
pub use segment::{TcpOutSegment, write_tcp_segment};
pub use seq::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};

use core::sync::atomic::{AtomicU16, Ordering};

use slopos_sync::{IrqMutex, LOCK_LEVEL_RESOURCE};
use slopos_utils::klog_debug;

use self::segment::SegmentBuilder;
use crate::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};

// =============================================================================
// Non-header constants (remain here until their owning module lands)
// =============================================================================

/// Maximum number of simultaneous TCP connections.
pub const MAX_CONNECTIONS: usize = 64;

/// Initial retransmission timeout in milliseconds (RFC 6298 recommends 1s).
pub const INITIAL_RTO_MS: u32 = 1000;

/// Maximum retransmission timeout in milliseconds.
pub const MAX_RTO_MS: u32 = 60_000;

/// TIME_WAIT duration in milliseconds (2 × MSL, MSL = 30s).
pub const TIME_WAIT_MS: u64 = 60_000;

/// Maximum retransmission attempts before giving up.
pub const MAX_RETRANSMITS: u8 = 8;

const TICKS_PER_SEC: u64 = 100;
const TCP_KEEPALIVE_IDLE_TICKS: u64 = 7_200 * TICKS_PER_SEC;
const TCP_KEEPALIVE_INTERVAL_TICKS: u64 = 75 * TICKS_PER_SEC;
const TCP_KEEPALIVE_PROBES_MAX: u8 = 9;

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

    pub keepalive_timer_token: Option<TimerToken>,
    pub keepalive_probes_sent: u8,
    pub last_data_activity_tick: u64,

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
            keepalive_timer_token: None,
            keepalive_probes_sent: 0,
            last_data_activity_tick: 0,
            time_wait_start_ms: 0,
            time_wait_timer_token: None,
            active: false,
            socket_idx: None,
            reset_received: false,
        }
    }
}

// =============================================================================
// ISN (Initial Sequence Number) generator
// =============================================================================
//
// Delegates to [`isn::generate_isn`] which mixes the 4-tuple with a per-boot
// secret and a 4µs clock drift (RFC 6528).  The historical
// `ISN_COUNTER.fetch_add(64000)` scheme tracked as SLOPOS-2026-0007 is gone.
pub(crate) use isn::generate_isn;

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
static TCP_TABLE: IrqMutex<TcpConnectionTable> =
    IrqMutex::new(TcpConnectionTable::new(), LOCK_LEVEL_RESOURCE);

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
            if let Some(token) = conn.keepalive_timer_token.take() {
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

fn tcp_cancel_keepalive(conn: &mut TcpConnection) {
    if let Some(token) = conn.keepalive_timer_token.take() {
        NET_TIMER_WHEEL.cancel(token);
    }
    conn.keepalive_probes_sent = 0;
}

fn tcp_maybe_schedule_keepalive_established(table: &mut TcpConnectionTable, idx: usize) {
    let socket_idx = table.connections[idx].socket_idx;
    let Some(socket_idx) = socket_idx else {
        return;
    };
    if !super::socket::socket_keepalive_enabled_by_index(socket_idx) {
        return;
    }

    let current_tick = slopos_kernel_services::platform::timer_ticks();
    let conn = &mut table.connections[idx];
    tcp_cancel_keepalive(conn);
    let token = NET_TIMER_WHEEL.schedule(
        TCP_KEEPALIVE_IDLE_TICKS,
        TimerKind::TcpKeepalive,
        idx as u32,
    );
    conn.keepalive_timer_token = Some(token);
    conn.keepalive_probes_sent = 0;
    conn.last_data_activity_tick = current_tick;
}

fn tcp_reset_keepalive_on_inbound_data(table: &mut TcpConnectionTable, idx: usize) {
    if table.connections[idx].state != TcpState::Established {
        return;
    }
    if table.connections[idx].keepalive_timer_token.is_none() {
        return;
    }

    let current_tick = slopos_kernel_services::platform::timer_ticks();
    let conn = &mut table.connections[idx];
    tcp_cancel_keepalive(conn);
    let token = NET_TIMER_WHEEL.schedule(
        TCP_KEEPALIVE_IDLE_TICKS,
        TimerKind::TcpKeepalive,
        idx as u32,
    );
    conn.keepalive_timer_token = Some(token);
    conn.last_data_activity_tick = current_tick;
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
    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip,
        remote_port,
    };
    let iss = generate_isn(&tuple);

    let mut table = TCP_TABLE.lock();

    let idx = table.alloc_slot().ok_or(TcpError::TableFull)?;

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

    let seg = SegmentBuilder::active_syn(tuple, iss, wscale);

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
            tcp_cancel_keepalive(conn);

            let seg = SegmentBuilder::fin_ack(conn, seq);

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
            tcp_cancel_keepalive(conn);

            let seg = SegmentBuilder::fin_ack(conn, seq);

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
        Some(SegmentBuilder::rst_of(conn))
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
            tcp_cancel_keepalive(conn);

            let seg = SegmentBuilder::fin_ack(conn, seq);

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
            tcp_cancel_keepalive(conn);

            let seg = SegmentBuilder::fin_ack(conn, seq);

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
///
/// Thin adapter around [`SegmentBuilder::rst_for`] so the `tcp_input`
/// dispatcher can call it by its historical name until P3 renames the whole
/// input path.
fn build_rst_for(hdr: &TcpHeader, local_ip: [u8; 4], remote_ip: [u8; 4]) -> TcpOutSegment {
    SegmentBuilder::rst_for(hdr, local_ip, remote_ip)
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
            response: Some(SegmentBuilder::bare_rst(*incoming_tuple, hdr.ack_num)),
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

    let iss = generate_isn(incoming_tuple);
    let peer_mss = parse_tcp_options(options).mss.unwrap_or(DEFAULT_MSS);
    let listener_socket_idx = table.connections[listen_idx].socket_idx;

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
    child.socket_idx = listener_socket_idx;

    klog_debug!(
        "tcp: LISTEN -> SYN_RECEIVED idx={} ISS={} IRS={}",
        new_idx,
        iss,
        hdr.seq_num
    );

    let seg = SegmentBuilder::passive_syn_ack(child);

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
                response: Some(SegmentBuilder::bare_rst(conn.tuple, hdr.ack_num)),
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
    }

    if table.connections[idx].state == TcpState::Established {
        tcp_maybe_schedule_keepalive_established(table, idx);

        klog_debug!(
            "tcp: SYN_SENT -> ESTABLISHED idx={} IRS={}",
            idx,
            table.connections[idx].irs
        );

        let seg = SegmentBuilder::ack_of(&table.connections[idx]);

        TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::Established),
            accepted_idx: None,
            reset: false,
        }
    } else {
        // Simultaneous open: SYN without ACK → SYN_RECEIVED.
        table.connections[idx].state = TcpState::SynReceived;

        klog_debug!(
            "tcp: SYN_SENT -> SYN_RECEIVED idx={} (simultaneous open)",
            idx
        );

        // Simultaneous-open SYN+ACK: emit with the conn's own rcv_wnd
        // instead of DEFAULT_WINDOW_SIZE, preserving pre-refactor bytes.
        let seg = TcpOutSegment {
            window_size: table.connections[idx].rcv_wnd,
            ..SegmentBuilder::passive_syn_ack(&table.connections[idx])
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
            response: Some(SegmentBuilder::bare_rst(conn.tuple, hdr.ack_num)),
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
    tcp_maybe_schedule_keepalive_established(table, idx);

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
        tcp_cancel_keepalive(&mut table.connections[idx]);

        if table.connections[idx].socket_idx.is_some() {
            table.connections[idx].state = TcpState::Closed;
            table.connections[idx].reset_received = true;
            return TcpInputResult {
                conn_idx: Some(idx),
                new_state: Some(TcpState::Closed),
                reset: true,
                ..TcpInputResult::empty()
            };
        } else {
            table.release(idx);
            return TcpInputResult {
                conn_idx: None,
                new_state: Some(TcpState::Closed),
                reset: true,
                ..TcpInputResult::empty()
            };
        }
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
            response: Some(SegmentBuilder::bare_rst(tuple, snd_nxt)),
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
            let seg = SegmentBuilder::ack_with_window(conn, table.buffers[idx].recv.window());
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

        if accepted_payload_len > 0 {
            tcp_reset_keepalive_on_inbound_data(table, idx);
        }

        if table.buffers[idx].recv.should_ack_now(now_ms) {
            let conn = &table.connections[idx];
            let seg = SegmentBuilder::ack_of(conn);
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

                    let seg = SegmentBuilder::ack_of(conn);
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
            let seg = SegmentBuilder::ack_of(conn);
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
                tcp_cancel_keepalive(conn);
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

        let seg = SegmentBuilder::ack_of(conn);

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
        let seg = SegmentBuilder::ack_of(conn);
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

pub fn tcp_on_keepalive(conn_id: u32) -> Option<TcpOutSegment> {
    let mut table = TCP_TABLE.lock();
    let idx = conn_id as usize;
    if idx >= MAX_CONNECTIONS {
        return None;
    }

    let conn = table.connections.get(idx)?;
    if !conn.active || conn.state != TcpState::Established {
        return None;
    }

    table.connections[idx].keepalive_timer_token = None;

    if table.connections[idx].keepalive_probes_sent >= TCP_KEEPALIVE_PROBES_MAX {
        klog_debug!(
            "tcp: keepalive max probes reached idx={} conn_id={} -> closing",
            idx,
            conn_id
        );
        table.release(idx);
        return None;
    }

    let probe_seg = SegmentBuilder::keepalive_probe(&table.connections[idx]);

    table.connections[idx].keepalive_probes_sent = table.connections[idx]
        .keepalive_probes_sent
        .saturating_add(1);
    let token = NET_TIMER_WHEEL.schedule(
        TCP_KEEPALIVE_INTERVAL_TICKS,
        TimerKind::TcpKeepalive,
        conn_id,
    );
    table.connections[idx].keepalive_timer_token = Some(token);

    Some(probe_seg)
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

    let now_ms = slopos_utils::clock::uptime_ms();
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
    let (state, seq, rto_ms, peer_mss, snd_wnd) = {
        let conn = table.get(idx)?;
        (
            conn.state,
            conn.snd_nxt,
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

    let window = table.buffers[idx].recv.window();
    let seg = SegmentBuilder::data_push(&table.connections[idx], seq, window);
    // `ack_num` on the builder comes from `conn.rcv_nxt`, which we never
    // mutated in this function — parity with the old inline construction.

    Some((seg, payload_len))
}

/// Deterministic retransmit probe — test-only.
///
/// Walks the connection table and triggers retransmit for the first
/// connection whose RTO deadline has expired at `now_ms`.  Production code
/// no longer polls this function (retransmits fire via
/// [`NET_TIMER_WHEEL`] → [`tcp_on_retransmit`]); it is retained here so
/// mock-clock tests in `tcp_data_tests` can drive retransmit behavior
/// without waiting for the wall-clock timer wheel.
#[cfg(feature = "itests")]
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
            let window = table.buffers[i].recv.window();
            let seg = SegmentBuilder::ack_with_window(&table.connections[i], window);
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

    let window = table.buffers[idx].recv.window();
    Some(SegmentBuilder::data_push(conn, conn.snd_nxt, window))
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
        if let Some(token) = table.connections[i].keepalive_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        if let Some(token) = table.connections[i].time_wait_timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        table.connections[i] = TcpConnection::empty();
        table.buffers[i].clear();
    }
    // Reset ISN secret and ephemeral port for deterministic tests.
    isn::reset_for_tests();
    EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}
