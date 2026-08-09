//! TCP listen model — a SYN queue and an accept queue, per Linux.
//!
//! The split is what keeps a flood of unanswered SYNs from denying service to
//! connections that complete. Half-open state lives in [`SynQueue`], bounded
//! at [`SYN_QUEUE_MAX`] *per listener* and held inside the listener's own PCB;
//! completed connections live in [`TcpListenState`]'s accept queue in the
//! socket layer, bounded by the `listen(fd, backlog)` backlog.
//!
//! A SYN arriving on a listening socket therefore costs one queue entry, not a
//! slot in the machine-wide connection table. Only the final ACK promotes a
//! connection into that table.
//!
//! 1. A SYN admits a [`SynRecvEntry`] to the SYN queue and emits a SYN-ACK.
//! 2. The final ACK removes the entry and yields an [`AcceptedConn`].
//! 3. `accept()` dequeues from the accept queue.
//!
//! A full SYN queue drops new SYNs silently. No RST: it would confirm to the
//! sender that the flood is working.
//!
//! # Timers
//!
//! SYN-ACK retransmission uses [`TimerKind::TcpSynAck`] with a per-entry key,
//! backing off 1s, 2s, 4s, 8s, 16s. After [`SYN_RETRIES_MAX`] attempts the
//! entry is dropped, again silently. The kind is distinct from
//! `TcpRetransmit` because these keys come from a counter of their own and
//! would otherwise collide with the `ConnId`s an established connection uses.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::klog_debug;
use slopos_ostd::{AllocError, KVec, KVecDeque};

use crate::tcp::{
    self, DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpOutSegment, TcpTuple,
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

/// Base SYN-ACK retransmit delay in milliseconds (1 second).
///
/// Each subsequent retry doubles this value (exponential backoff).
pub const SYN_ACK_BASE_DELAY_MS: u64 = 1_000;

/// Minimum listen backlog.
pub const BACKLOG_MIN: usize = 1;

/// Maximum listen backlog.
///
/// Every entry in the accept queue is a connection already installed in the
/// machine-wide shard table, so a backlog larger than that table describes a
/// depth no listener can reach — and one listener promising it would be
/// promising every other listener's slots.
pub const BACKLOG_MAX: usize = crate::tcp::table::TOTAL_PCB_SLOTS / 2;

// =============================================================================
// Key generator for timer dispatch
// =============================================================================

/// Monotonically increasing key generator for SYN queue timer entries.
///
/// Each [`SynRecvEntry`] gets a unique key so timer dispatch can find it. This
/// space is disjoint from `ConnId`, which is why the entries carry
/// [`TimerKind::TcpSynAck`] rather than sharing `TcpRetransmit`.
static NEXT_SYN_ENTRY_KEY: AtomicU32 = AtomicU32::new(1);

fn alloc_syn_entry_key() -> u32 {
    NEXT_SYN_ENTRY_KEY.fetch_add(1, Ordering::Relaxed)
}

/// Reset the key generator (for deterministic tests).
#[cfg(feature = "test-hooks")]
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
    /// Peer offered SACK-Permitted in their SYN.
    pub sack_permitted: bool,
    /// Unique key for timer dispatch — identifies this entry when the timer fires.
    pub key: u32,
    /// Peer's TSval from their SYN (if they offered timestamps).
    pub peer_tsval: Option<u32>,
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
    /// Peer offered SACK-Permitted in their SYN.
    pub sack_permitted: bool,
    /// Peer's TSval from their SYN (if they offered timestamps).
    pub peer_tsval: Option<u32>,
}

// =============================================================================
// SynQueue — half-open connections, bounded per listener
// =============================================================================

/// The half-open connections of one listening socket.
///
/// Lives inside the listener's own PCB, so the LISTEN state machine can admit
/// a SYN under the lock it already holds and nothing half-open ever reaches
/// the shared connection table. That table has [`crate::tcp::table`]'s
/// `TOTAL_PCB_SLOTS` entries for the whole machine; a per-listener bound is
/// what keeps one unanswered flood from spending them all.
///
/// Entries leave on the final ACK, on RST, or when the retransmit budget runs
/// out. A full queue drops new SYNs silently — a RST would tell the sender its
/// flood is working.
pub struct SynQueue {
    /// Keyed by four-tuple for an O(n) scan; `n <= SYN_QUEUE_MAX`, and the
    /// alternative is a hash table whose insert allocates under a cli-lock.
    entries: KVec<(TcpFourTuple, SynRecvEntry)>,
    local: SockAddr,
}

impl SynQueue {
    /// An empty queue with no capacity, for a PCB that is not listening.
    pub const fn new() -> Self {
        Self {
            entries: KVec::new(),
            local: SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0)),
        }
    }

    /// A queue with room for [`SYN_QUEUE_MAX`] entries reserved up front.
    ///
    /// Built before the listener PCB is installed. `on_syn` runs under the
    /// listener's cli-spinlock, and a `push` that grows the backing buffer
    /// there would put the allocator beneath a lock a remote peer drives.
    pub fn with_capacity(local: SockAddr) -> Result<Self, AllocError> {
        Ok(Self {
            entries: KVec::with_capacity(SYN_QUEUE_MAX)?,
            local,
        })
    }

    fn four_tuple(&self, remote: SockAddr) -> TcpFourTuple {
        TcpFourTuple {
            local_ip: self.local.ip,
            local_port: self.local.port,
            remote_ip: remote.ip,
            remote_port: remote.port,
        }
    }

    /// Admit a SYN, returning the SYN-ACK to send.
    ///
    /// `None` means the SYN was dropped: the queue is full. A duplicate SYN
    /// for a tuple already queued retransmits the original SYN-ACK rather than
    /// taking a second slot.
    pub fn on_syn(
        &mut self,
        remote: SockAddr,
        irs: u32,
        peer_mss: u16,
        sack_permitted: bool,
        timestamp: u64,
        peer_tsval: Option<u32>,
    ) -> Option<TcpOutSegment> {
        let four_tuple = self.four_tuple(remote);

        if let Some((_, entry)) = self.entries.iter().find(|(ft, _)| *ft == four_tuple) {
            return Some(build_syn_ack_from(entry, &four_tuple));
        }

        if self.entries.len() >= SYN_QUEUE_MAX {
            klog_debug!(
                "tcp_listen: SYN queue full ({}), dropping SYN from {}:{}",
                SYN_QUEUE_MAX,
                remote.ip,
                remote.port.0
            );
            return None;
        }

        let child_tuple = four_tuple.to_tcp_tuple();
        let iss = tcp::isn::generate_isn(&child_tuple);
        let key = alloc_syn_entry_key();
        let timer_token =
            NET_TIMER_WHEEL.schedule(SYN_ACK_BASE_DELAY_MS, TimerKind::TcpSynAck, key);
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
            sack_permitted,
            key,
            peer_tsval,
        };

        let syn_ack = build_syn_ack_from(&entry, &four_tuple);
        // Reserved at construction, so this cannot grow the buffer. A failure
        // here would mean the reservation was skipped.
        if self.entries.push((four_tuple, entry)).is_err() {
            NET_TIMER_WHEEL.cancel(timer_token);
            return None;
        }

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

    /// Complete a handshake: match the final ACK against a queued entry and
    /// hand back everything the connection needs.
    ///
    /// `None` means no entry matched — the caller answers that with a RST, as
    /// an ACK naming no connection always has been.
    pub fn on_ack(&mut self, remote: SockAddr, ack_num: u32) -> Option<AcceptedConn> {
        let four_tuple = self.four_tuple(remote);
        let idx = self
            .entries
            .iter()
            .position(|(ft, entry)| *ft == four_tuple && ack_num == entry.iss.wrapping_add(1))?;

        let (_, entry) = self.entries.swap_remove(idx);
        NET_TIMER_WHEEL.cancel(entry.timer_token);

        klog_debug!(
            "tcp_listen: 3WHS complete for {}:{} (iss={}, irs={})",
            remote.ip,
            remote.port.0,
            entry.iss,
            entry.irs
        );

        Some(AcceptedConn {
            tuple: four_tuple.to_tcp_tuple(),
            iss: entry.iss,
            irs: entry.irs,
            peer_mss: entry.peer_mss,
            sack_permitted: entry.sack_permitted,
            peer_tsval: entry.peer_tsval,
        })
    }

    /// A SYN-ACK retransmit timer fired for `key`.
    ///
    /// Retransmits with exponential backoff until [`SYN_RETRIES_MAX`], then
    /// drops the entry silently — again, no RST.
    pub fn on_retransmit(&mut self, key: u32) -> Option<TcpOutSegment> {
        let idx = self.entries.iter().position(|(_, e)| e.key == key)?;

        let (four_tuple, entry) = &mut self.entries[idx];
        entry.retries += 1;

        if entry.retries > SYN_RETRIES_MAX {
            let four_tuple_copy = *four_tuple;
            let (_, removed) = self.entries.swap_remove(idx);
            klog_debug!(
                "tcp_listen: SYN-ACK retransmit exhausted for {}:{} (key={}, retries={})",
                four_tuple_copy.remote_ip,
                four_tuple_copy.remote_port.0,
                removed.key,
                removed.retries
            );
            return None;
        }

        let syn_ack = build_syn_ack_from(entry, four_tuple);
        let delay = SYN_ACK_BASE_DELAY_MS * (1u64 << (entry.retries as u64 - 1));
        entry.timer_token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpSynAck, key);

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

    /// Forget a half-open connection the peer reset.
    pub fn remove(&mut self, remote: SockAddr) -> bool {
        let four_tuple = self.four_tuple(remote);
        let Some(idx) = self.entries.iter().position(|(ft, _)| *ft == four_tuple) else {
            return false;
        };
        let (_, entry) = self.entries.swap_remove(idx);
        NET_TIMER_WHEEL.cancel(entry.timer_token);
        true
    }

    /// Half-open connections currently queued.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `key` names an entry here — the timer wheel's routing question.
    pub fn has_key(&self, key: u32) -> bool {
        self.entries.iter().any(|(_, e)| e.key == key)
    }

    /// Drop every entry, cancelling its retransmit timer.
    pub fn clear(&mut self) {
        for (_, entry) in self.entries.drain(..) {
            NET_TIMER_WHEEL.cancel(entry.timer_token);
        }
    }
}

impl Default for SynQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SynQueue {
    fn drop(&mut self) {
        self.clear();
    }
}

impl core::fmt::Debug for SynQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SynQueue")
            .field("local", &self.local)
            .field("len", &self.entries.len())
            .finish()
    }
}

// =============================================================================
// TcpListenState — completed connections waiting for accept()
// =============================================================================

/// The accept queue of one listening socket.
///
/// The other half of the Linux two-queue model; the half-open side is
/// [`SynQueue`], in the listener's PCB. This side lives in the socket layer
/// because `accept()` is a socket call, and an entry only arrives here once
/// the handshake is complete.
pub struct TcpListenState {
    /// Completed connections waiting for `accept()`.
    accept_queue: KVecDeque<AcceptedConn>,

    /// Maximum accept queue size (from `listen(fd, backlog)`).
    backlog: usize,

    /// Local address this listener is bound to.
    local: SockAddr,
}

impl TcpListenState {
    /// Create a new listen state with the given backlog and local address.
    ///
    /// Backlog is clamped to [`BACKLOG_MIN`]..=[`BACKLOG_MAX`]. `None` on
    /// allocation failure — `listen` reports it rather than panicking.
    pub fn new(backlog: usize, local: SockAddr) -> Option<Self> {
        let backlog = backlog.clamp(BACKLOG_MIN, BACKLOG_MAX);
        Some(Self {
            accept_queue: KVecDeque::with_capacity(backlog).ok()?,
            backlog,
            local,
        })
    }

    /// Dequeue a completed connection.
    pub fn accept(&mut self) -> Option<AcceptedConn> {
        self.accept_queue.pop_front()
    }

    /// Enqueue a completed connection. `false` if the accept queue is full.
    pub fn push_accepted(&mut self, conn: AcceptedConn) -> bool {
        if self.accept_queue.len() >= self.backlog {
            return false;
        }
        let _ = self.accept_queue.push_back(conn);
        true
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

    /// Drop every queued connection.
    pub fn clear(&mut self) {
        self.accept_queue.clear();
    }
}

impl core::fmt::Debug for TcpListenState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TcpListenState")
            .field("local", &self.local)
            .field("backlog", &self.backlog)
            .field("accept_queue_len", &self.accept_queue.len())
            .finish()
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Build a SYN-ACK segment from a SYN queue entry.
fn build_syn_ack_from(entry: &SynRecvEntry, ft: &TcpFourTuple) -> TcpOutSegment {
    let mut seg = TcpOutSegment {
        tuple: ft.to_tcp_tuple(),
        seq_num: entry.iss,
        ack_num: entry.irs.wrapping_add(1),
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: Some(DEFAULT_MSS),
        wscale: None,
        sack_permitted: entry.sack_permitted,
        sack_blocks: [(0, 0); 4],
        sack_block_count: 0,
        timestamp: None,
    };
    if let Some(tsval) = entry.peer_tsval {
        seg.timestamp = Some((super::clock::now_ms() as u32, tsval));
    }
    seg
}

// TCP_DEMUX subsystem removed in Phase 6b — all demux now goes through
// the sharded table in tcp::table (TCP_SHARDS + TCP_LISTENERS).
