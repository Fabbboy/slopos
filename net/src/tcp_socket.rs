//! TCP socket layer — Two-Queue Listen Model.
//!
//! Implements the SYN queue / accept queue split for TCP listening sockets,
//! following the Linux two-queue model that prevents SYN floods from blocking
//! legitimate connections.
//!
//! # Architecture
//!
//! When a SYN arrives on a listening socket:
//! 1. A [`SynRecvEntry`] is created in the SYN queue (half-open connections)
//! 2. A SYN-ACK is sent and a retransmit timer is scheduled
//! 3. When the final ACK arrives, the entry moves to the accept queue
//! 4. `accept()` dequeues completed connections
//!
//! If the SYN queue is full, new SYNs are silently dropped (no RST — that
//! would help attackers confirm their flood is working). If the accept queue
//! is full, completed connections stay in the SYN queue until space opens.
//!
//! # Timer Integration
//!
//! SYN-ACK retransmission uses [`TimerKind::TcpRetransmit`] with a per-entry
//! key. Exponential backoff: 1s, 2s, 4s, 8s, 16s. After [`SYN_RETRIES_MAX`]
//! (5) failed attempts, the entry is silently removed.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_lib::{IrqMutex, klog_debug};

use crate::tcp::{
    self, DEFAULT_MSS, DEFAULT_WINDOW_SIZE, MAX_CONNECTIONS, TCP_FLAG_ACK, TCP_FLAG_SYN,
    TcpOutSegment, TcpTuple,
};
use crate::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};
use crate::types::{Ipv4Addr, Port, SockAddr};

// =============================================================================
// Constants
// =============================================================================

/// Maximum number of half-open connections per listening socket.
///
/// Separate from the accept backlog — this bounds the SYN queue to prevent
/// memory exhaustion during SYN floods.
pub const SYN_QUEUE_MAX: usize = 128;

/// Maximum SYN-ACK retransmission attempts before silent drop.
///
/// 5 retries with exponential backoff (1s, 2s, 4s, 8s, 16s) = 31s total
/// before giving up.
pub const SYN_RETRIES_MAX: u8 = 5;

/// Base SYN-ACK retransmit delay in timer ticks (~1 second at 100Hz).
///
/// Each subsequent retry doubles this value (exponential backoff).
pub const SYN_ACK_BASE_DELAY_TICKS: u64 = 100;

/// Minimum listen backlog.
pub const BACKLOG_MIN: usize = 1;

/// Maximum listen backlog.
pub const BACKLOG_MAX: usize = 128;

// =============================================================================
// Key generator for timer dispatch
// =============================================================================

/// Monotonically increasing key generator for SYN queue timer entries.
///
/// Each [`SynRecvEntry`] gets a unique key so that timer dispatch can find
/// the correct entry.  Keys are never reused — the generator wraps at u32::MAX
/// which is acceptable for a hobby OS.
static NEXT_SYN_ENTRY_KEY: AtomicU32 = AtomicU32::new(1);

fn alloc_syn_entry_key() -> u32 {
    NEXT_SYN_ENTRY_KEY.fetch_add(1, Ordering::Relaxed)
}

/// Reset the key generator (for deterministic tests).
#[cfg(feature = "itests")]
pub fn reset_syn_entry_keys() {
    NEXT_SYN_ENTRY_KEY.store(1, Ordering::Relaxed);
}

// =============================================================================
// TcpFourTuple — SYN queue lookup key
// =============================================================================

/// A four-tuple identifying a half-open connection in the SYN queue.
///
/// Uses the type-safe [`Ipv4Addr`] and [`Port`] newtypes from the types module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpFourTuple {
    pub local_ip: Ipv4Addr,
    pub local_port: Port,
    pub remote_ip: Ipv4Addr,
    pub remote_port: Port,
}

impl TcpFourTuple {
    /// Convert from a raw [`TcpTuple`].
    pub fn from_tcp_tuple(t: &TcpTuple) -> Self {
        Self {
            local_ip: Ipv4Addr(t.local_ip),
            local_port: Port(t.local_port),
            remote_ip: Ipv4Addr(t.remote_ip),
            remote_port: Port(t.remote_port),
        }
    }

    /// Convert to a raw [`TcpTuple`].
    pub fn to_tcp_tuple(&self) -> TcpTuple {
        TcpTuple {
            local_ip: self.local_ip.0,
            local_port: self.local_port.0,
            remote_ip: self.remote_ip.0,
            remote_port: self.remote_port.0,
        }
    }
}

// =============================================================================
// 5A.1 — SynRecvEntry
// =============================================================================

/// A connection in `SYN_RECEIVED` state, not yet fully established.
///
/// Lives in the SYN queue of a [`TcpListenState`].  When the final ACK of
/// the three-way handshake arrives, this entry is consumed and an
/// [`AcceptedConn`] is placed in the accept queue.
///
/// Bounded at [`SYN_QUEUE_MAX`] entries per listener — separate from the
/// accept backlog.
pub struct SynRecvEntry {
    /// Remote endpoint (client).
    pub remote: SockAddr,
    /// Local endpoint (server).
    pub local: SockAddr,
    /// Initial Send Sequence number (our ISS, sent in SYN-ACK).
    pub iss: u32,
    /// Initial Receive Sequence number (client's ISS from their SYN).
    pub irs: u32,
    /// Number of SYN-ACK retransmissions so far.
    pub retries: u8,
    /// Timer token for the pending SYN-ACK retransmit timer.
    pub timer_token: TimerToken,
    /// Timestamp (timer ticks) when this entry was created.
    pub timestamp: u64,
    /// Peer's advertised MSS (or [`DEFAULT_MSS`] if not specified).
    pub peer_mss: u16,
    /// Unique key for timer dispatch — identifies this entry when the timer fires.
    pub key: u32,
}

impl core::fmt::Debug for SynRecvEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SynRecvEntry")
            .field("remote", &self.remote)
            .field("iss", &self.iss)
            .field("irs", &self.irs)
            .field("retries", &self.retries)
            .field("key", &self.key)
            .finish()
    }
}

// =============================================================================
// AcceptedConn — completed 3WHS output
// =============================================================================

/// Information about a completed TCP connection (three-way handshake done).
///
/// This is what [`TcpListenState::accept`] returns.  The socket layer will use this
/// to create a full [`TcpConnection`] in the connection table and bind it to
/// a socket.
#[derive(Clone, Copy, Debug)]
pub struct AcceptedConn {
    /// The four-tuple of the established connection.
    pub tuple: TcpTuple,
    /// Our Initial Send Sequence number.
    pub iss: u32,
    /// Peer's Initial Receive Sequence number.
    pub irs: u32,
    /// Peer's advertised MSS.
    pub peer_mss: u16,
}

// =============================================================================
// 5A.2 — TcpListenState
// =============================================================================

/// Two-queue listen model for TCP listening sockets.
///
/// Separates half-open connections (SYN queue) from fully established
/// connections (accept queue), following the Linux `inet_csk_reqsk_queue` model.
///
/// # SYN Flood Protection
///
/// - SYN queue has a separate, larger bound ([`SYN_QUEUE_MAX`] = 128)
/// - When full, new SYNs are silently dropped (no RST)
/// - Accept queue overflow keeps connections in SYN queue until space opens
/// - SYN-ACK retransmit uses exponential backoff with bounded retries
pub struct TcpListenState {
    /// Half-open connections: SYN received, SYN-ACK sent, waiting for ACK.
    ///
    /// Keyed by four-tuple for O(n) lookup (n ≤ 128, linear scan is fine).
    syn_queue: Vec<(TcpFourTuple, SynRecvEntry)>,

    /// Completed connections waiting for `accept()`.
    ///
    /// Capacity is bounded by the listen `backlog`.
    accept_queue: VecDeque<AcceptedConn>,

    /// Maximum accept queue size (from `listen(fd, backlog)`).
    backlog: usize,

    /// Local address this listener is bound to.
    local: SockAddr,
}

impl TcpListenState {
    /// Create a new listen state with the given backlog and local address.
    ///
    /// Backlog is clamped to [`BACKLOG_MIN`]..=[`BACKLOG_MAX`].
    pub fn new(backlog: usize, local: SockAddr) -> Self {
        let backlog = backlog.clamp(BACKLOG_MIN, BACKLOG_MAX);
        Self {
            syn_queue: Vec::with_capacity(core::cmp::min(SYN_QUEUE_MAX, 32)),
            accept_queue: VecDeque::with_capacity(backlog),
            backlog,
            local,
        }
    }

    // =========================================================================
    // SYN handling
    // =========================================================================

    /// Handle an incoming SYN segment.
    ///
    /// Creates a [`SynRecvEntry`] in the SYN queue, generates an ISS, and
    /// returns the SYN-ACK segment to send.  Schedules a retransmit timer.
    ///
    /// Returns `None` if the SYN queue is full (silently dropped — no RST,
    /// per anti-SYN-flood design).
    ///
    /// If a duplicate SYN arrives for an existing four-tuple, the existing
    /// SYN-ACK is retransmitted.
    pub fn on_syn(
        &mut self,
        remote: SockAddr,
        irs: u32,
        peer_mss: u16,
        timestamp: u64,
    ) -> Option<TcpOutSegment> {
        let four_tuple = TcpFourTuple {
            local_ip: self.local.ip,
            local_port: self.local.port,
            remote_ip: remote.ip,
            remote_port: remote.port,
        };

        // Duplicate SYN — retransmit existing SYN-ACK.
        if let Some((_, entry)) = self.syn_queue.iter().find(|(ft, _)| *ft == four_tuple) {
            return Some(self.build_syn_ack(entry, &four_tuple));
        }

        // SYN queue full — drop silently (no RST).
        if self.syn_queue.len() >= SYN_QUEUE_MAX {
            klog_debug!(
                "tcp_listen: SYN queue full ({}), dropping SYN from {}:{}",
                SYN_QUEUE_MAX,
                remote.ip,
                remote.port.0
            );
            return None;
        }

        let iss = tcp::generate_isn();
        let key = alloc_syn_entry_key();

        // Schedule initial SYN-ACK retransmit timer.
        let timer_token =
            NET_TIMER_WHEEL.schedule(SYN_ACK_BASE_DELAY_TICKS, TimerKind::TcpRetransmit, key);

        let effective_mss = if peer_mss == 0 { DEFAULT_MSS } else { peer_mss };

        let entry = SynRecvEntry {
            remote,
            local: self.local,
            iss,
            irs,
            retries: 0,
            timer_token,
            timestamp,
            peer_mss: effective_mss,
            key,
        };

        let syn_ack = self.build_syn_ack(&entry, &four_tuple);
        self.syn_queue.push((four_tuple, entry));

        klog_debug!(
            "tcp_listen: SYN from {}:{} -> SYN_RECEIVED (key={}, iss={}, irs={})",
            remote.ip,
            remote.port.0,
            key,
            iss,
            irs
        );

        Some(syn_ack)
    }

    // =========================================================================
    // ACK handling (3WHS completion)
    // =========================================================================

    /// Handle an incoming ACK that may complete a three-way handshake.
    ///
    /// Looks up the matching SYN queue entry by four-tuple and validates that
    /// the ACK number acknowledges our SYN-ACK (ack_num == iss + 1).
    ///
    /// If the accept queue has room, the entry is moved from SYN queue to
    /// accept queue and the completed connection info is returned.
    ///
    /// If the accept queue is full, the entry **stays in the SYN queue** — it
    /// is not dropped or RST'd.  The next `accept()` call will free space.
    ///
    /// Returns `None` if no matching entry is found or the accept queue is full.
    pub fn on_ack(&mut self, remote: SockAddr, ack_num: u32) -> Option<AcceptedConn> {
        let four_tuple = TcpFourTuple {
            local_ip: self.local.ip,
            local_port: self.local.port,
            remote_ip: remote.ip,
            remote_port: remote.port,
        };

        // Find matching SYN queue entry.
        let idx = self
            .syn_queue
            .iter()
            .position(|(ft, entry)| *ft == four_tuple && ack_num == entry.iss.wrapping_add(1))?;

        // Accept queue full — keep in SYN queue (don't RST, don't drop).
        if self.accept_queue.len() >= self.backlog {
            klog_debug!(
                "tcp_listen: accept queue full (backlog={}), keeping in SYN queue",
                self.backlog
            );
            return None;
        }

        // Remove from SYN queue (swap_remove is O(1)).
        let (_, entry) = self.syn_queue.swap_remove(idx);

        // Cancel the retransmit timer.
        NET_TIMER_WHEEL.cancel(entry.timer_token);

        let accepted = AcceptedConn {
            tuple: four_tuple.to_tcp_tuple(),
            iss: entry.iss,
            irs: entry.irs,
            peer_mss: entry.peer_mss,
        };

        klog_debug!(
            "tcp_listen: 3WHS complete for {}:{} (iss={}, irs={})",
            remote.ip,
            remote.port.0,
            entry.iss,
            entry.irs
        );

        self.accept_queue.push_back(accepted);
        Some(accepted)
    }

    // =========================================================================
    // 5A.3 — SYN-ACK retransmission
    // =========================================================================

    /// Handle a SYN-ACK retransmission timer firing.
    ///
    /// Looks up the SYN queue entry by its unique `key`.  If found:
    /// - If retries < [`SYN_RETRIES_MAX`]: retransmit SYN-ACK with exponential
    ///   backoff (1s, 2s, 4s, 8s, 16s) and schedule the next timer.
    /// - If retries >= [`SYN_RETRIES_MAX`]: remove the entry silently (no RST).
    ///
    /// Returns the SYN-ACK segment to retransmit, or `None` if the entry was
    /// removed or not found.
    pub fn on_retransmit(&mut self, key: u32) -> Option<TcpOutSegment> {
        let idx = self.syn_queue.iter().position(|(_, e)| e.key == key)?;

        let (four_tuple, entry) = &mut self.syn_queue[idx];
        entry.retries += 1;

        if entry.retries > SYN_RETRIES_MAX {
            // Max retries exceeded — remove silently (no RST to avoid aiding attackers).
            let four_tuple_copy = *four_tuple;
            let (_, removed) = self.syn_queue.swap_remove(idx);

            klog_debug!(
                "tcp_listen: SYN-ACK retransmit exhausted for {}:{} (key={}, retries={})",
                four_tuple_copy.remote_ip,
                four_tuple_copy.remote_port.0,
                removed.key,
                removed.retries
            );

            // Timer already fired, no need to cancel.
            return None;
        }

        // Build retransmit SYN-ACK.
        let syn_ack = build_syn_ack_from(entry, four_tuple);

        // Schedule next retransmit with exponential backoff.
        // retries=1 → 1s, retries=2 → 2s, retries=3 → 4s, retries=4 → 8s, retries=5 → 16s
        let delay = SYN_ACK_BASE_DELAY_TICKS * (1u64 << (entry.retries as u64 - 1));
        entry.timer_token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpRetransmit, key);

        klog_debug!(
            "tcp_listen: SYN-ACK retransmit #{} for {}:{} (key={}, next_delay={})",
            entry.retries,
            four_tuple.remote_ip,
            four_tuple.remote_port.0,
            key,
            delay
        );

        Some(syn_ack)
    }

    // =========================================================================
    // Accept queue operations
    // =========================================================================

    /// Dequeue a completed connection from the accept queue.
    ///
    /// Returns `None` if no connections are ready.
    pub fn accept(&mut self) -> Option<AcceptedConn> {
        self.accept_queue.pop_front()
    }

    /// Push a completed connection directly into the accept queue.
    ///
    /// Used by `socket_notify_tcp_activity()` when a server-side child connection
    /// completes the 3WHS (SYN_RECEIVED → Established) in the TCP state machine.
    /// Returns `true` if the connection was enqueued, `false` if the accept queue is full.
    pub fn push_accepted(&mut self, conn: AcceptedConn) -> bool {
        if self.accept_queue.len() >= self.backlog {
            return false;
        }
        self.accept_queue.push_back(conn);
        true
    }

    // =========================================================================
    // Diagnostics
    // =========================================================================

    /// Number of half-open connections in the SYN queue.
    pub fn syn_queue_len(&self) -> usize {
        self.syn_queue.len()
    }

    /// Number of completed connections waiting in the accept queue.
    pub fn accept_queue_len(&self) -> usize {
        self.accept_queue.len()
    }

    /// Whether the accept queue has room for another completed connection.
    pub fn accept_queue_has_room(&self) -> bool {
        self.accept_queue.len() < self.backlog
    }

    /// Maximum accept queue capacity (the listen backlog).
    pub fn backlog(&self) -> usize {
        self.backlog
    }

    /// The local address this listener is bound to.
    pub fn local_addr(&self) -> SockAddr {
        self.local
    }

    /// Find a SYN queue entry by its timer key (for timer dispatch).
    pub fn has_syn_entry_for_key(&self, key: u32) -> bool {
        self.syn_queue.iter().any(|(_, e)| e.key == key)
    }

    /// Clear all SYN queue entries, cancelling their timers.
    pub fn clear_syn_queue(&mut self) {
        for (_, entry) in self.syn_queue.drain(..) {
            NET_TIMER_WHEEL.cancel(entry.timer_token);
        }
    }

    /// Clear both queues.
    pub fn clear(&mut self) {
        self.clear_syn_queue();
        self.accept_queue.clear();
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    fn build_syn_ack(&self, entry: &SynRecvEntry, ft: &TcpFourTuple) -> TcpOutSegment {
        build_syn_ack_from(entry, ft)
    }
}

impl Drop for TcpListenState {
    fn drop(&mut self) {
        // Cancel all pending retransmit timers.
        self.clear_syn_queue();
    }
}

impl core::fmt::Debug for TcpListenState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TcpListenState")
            .field("local", &self.local)
            .field("backlog", &self.backlog)
            .field("syn_queue_len", &self.syn_queue.len())
            .field("accept_queue_len", &self.accept_queue.len())
            .finish()
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Build a SYN-ACK segment from a SYN queue entry.
fn build_syn_ack_from(entry: &SynRecvEntry, ft: &TcpFourTuple) -> TcpOutSegment {
    TcpOutSegment {
        tuple: ft.to_tcp_tuple(),
        seq_num: entry.iss,
        ack_num: entry.irs.wrapping_add(1),
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: DEFAULT_MSS,
        wscale: 255,
    }
}

// =============================================================================
// TCP Demux Table
// =============================================================================

/// Entry in the established-connection demux table.
///
/// Maps a full four-tuple (local_ip, local_port, remote_ip, remote_port) to a
/// connection index in [`TcpConnectionTable`].
#[derive(Clone, Copy)]
struct TcpEstablishedEntry {
    local_ip: Ipv4Addr,
    local_port: Port,
    remote_ip: Ipv4Addr,
    remote_port: Port,
    conn_id: u32,
}

/// Entry in the listener demux table.
///
/// Maps a two-tuple (local_ip, local_port) to a socket index in the socket table.
#[derive(Clone, Copy)]
struct TcpListenerEntry {
    local_ip: Ipv4Addr,
    local_port: Port,
    sock_idx: u32,
}

/// TCP demux table for fast connection and listener lookup.
///
/// Two separate tables:
/// - **Established**: 4-tuple → connection index (used for data segments)
/// - **Listener**: 2-tuple → socket index (used for SYN on listening sockets)
///
/// Modeled after [`UdpDemuxTable`](super::udp::UdpDemuxTable) but with separate
/// established and listener lookup paths per TCP semantics.
pub struct TcpDemuxTable {
    established: [Option<TcpEstablishedEntry>; MAX_CONNECTIONS],
    listeners: [Option<TcpListenerEntry>; MAX_CONNECTIONS],
}

impl TcpDemuxTable {
    pub const fn new() -> Self {
        Self {
            established: [None; MAX_CONNECTIONS],
            listeners: [None; MAX_CONNECTIONS],
        }
    }

    /// Register an established connection (4-tuple → conn_id).
    pub fn register_established(
        &mut self,
        local_ip: Ipv4Addr,
        local_port: Port,
        remote_ip: Ipv4Addr,
        remote_port: Port,
        conn_id: u32,
    ) -> Result<(), super::types::NetError> {
        // Check for duplicate.
        for slot in &self.established {
            if let Some(entry) = slot
                && entry.local_ip == local_ip
                && entry.local_port == local_port
                && entry.remote_ip == remote_ip
                && entry.remote_port == remote_port
            {
                return Err(super::types::NetError::AddressInUse);
            }
        }

        for slot in &mut self.established {
            if slot.is_none() {
                *slot = Some(TcpEstablishedEntry {
                    local_ip,
                    local_port,
                    remote_ip,
                    remote_port,
                    conn_id,
                });
                return Ok(());
            }
        }

        Err(super::types::NetError::NoBufferSpace)
    }

    /// Unregister an established connection by conn_id.
    pub fn unregister_established(&mut self, conn_id: u32) {
        for slot in &mut self.established {
            if let Some(entry) = slot
                && entry.conn_id == conn_id
            {
                *slot = None;
                return;
            }
        }
    }

    /// Register a listener (2-tuple → sock_idx).
    pub fn register_listener(
        &mut self,
        local_ip: Ipv4Addr,
        local_port: Port,
        sock_idx: u32,
    ) -> Result<(), super::types::NetError> {
        // Check for duplicate.
        for slot in &self.listeners {
            if let Some(entry) = slot
                && entry.local_ip == local_ip
                && entry.local_port == local_port
            {
                return Err(super::types::NetError::AddressInUse);
            }
        }

        for slot in &mut self.listeners {
            if slot.is_none() {
                *slot = Some(TcpListenerEntry {
                    local_ip,
                    local_port,
                    sock_idx,
                });
                return Ok(());
            }
        }

        Err(super::types::NetError::NoBufferSpace)
    }

    /// Unregister a listener by sock_idx.
    pub fn unregister_listener(&mut self, sock_idx: u32) {
        for slot in &mut self.listeners {
            if let Some(entry) = slot
                && entry.sock_idx == sock_idx
            {
                *slot = None;
                return;
            }
        }
    }

    /// Look up an established connection by exact 4-tuple match.
    pub fn lookup_established(
        &self,
        local_ip: Ipv4Addr,
        local_port: Port,
        remote_ip: Ipv4Addr,
        remote_port: Port,
    ) -> Option<u32> {
        for entry in self.established.iter().flatten() {
            if entry.local_ip == local_ip
                && entry.local_port == local_port
                && entry.remote_ip == remote_ip
                && entry.remote_port == remote_port
            {
                return Some(entry.conn_id);
            }
        }
        None
    }

    /// Look up a listener by 2-tuple (local_ip, local_port).
    ///
    /// Exact IP match first, then wildcard (UNSPECIFIED) fallback.
    pub fn lookup_listener(&self, local_ip: Ipv4Addr, local_port: Port) -> Option<u32> {
        // Exact match first.
        for entry in self.listeners.iter().flatten() {
            if entry.local_ip == local_ip && entry.local_port == local_port {
                return Some(entry.sock_idx);
            }
        }

        // Wildcard (0.0.0.0) fallback.
        for entry in self.listeners.iter().flatten() {
            if entry.local_ip == Ipv4Addr::UNSPECIFIED && entry.local_port == local_port {
                return Some(entry.sock_idx);
            }
        }

        None
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.established = [None; MAX_CONNECTIONS];
        self.listeners = [None; MAX_CONNECTIONS];
    }
}

/// Global TCP demux table.
pub static TCP_DEMUX: IrqMutex<TcpDemuxTable> = IrqMutex::new(TcpDemuxTable::new());
