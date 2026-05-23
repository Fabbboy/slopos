//! Lock-free demux + per-slot mutation for the TCP connection table.
//!
//! ## Read side — wait-free under [`NET_EPOCH`]
//!
//! `find` enters [`NET_EPOCH`] and loads two [`RcuCell`]s — the
//! per-shard tuple index and the listener-key index. No `SpinLock` is
//! touched on the dispatch path; multiple readers and a concurrent
//! writer never serialise.
//!
//! ## Write side — per-slot locks + write-serialised index publish
//!
//! Established connections live in 16 shards × 4 slots = 64
//! independently-locked [`PcbSlot`]s. Mutation of a PCB / buffer takes
//! exactly one [`TCP_PCB_SLOTS`] entry; an install or release also
//! takes the matching [`TCP_SHARDS_WRITE`] lock for the brief window
//! during which the published index is updated via
//! [`RcuCell::replace`]. The displaced index `KBox` is deferred via
//! the cell's built-in `rcu_call_typed` reclamation.
//!
//! Listeners follow the same pattern: a single
//! [`TCP_LISTENERS_INDEX`] cell + 16 per-listener slots
//! ([`TCP_LISTENER_SLOTS`]).
//!
//! ## Lock-order rules
//!
//! All real locks are [`LOCK_LEVEL_RESOURCE`]. The only co-acquire
//! pattern is `TCP_*_WRITE → TCP_*_SLOTS[i]`, taken in this order
//! during install/release. Mutation-only call sites
//! (`tcp::input_process_*`, `tcp::send`, `tcp::recv`, timers) take a
//! single per-slot lock; no nested acquire.
//!
//! Calling any tracked `SpinLock::lock` while a [`NET_EPOCH`] guard
//! is live is detected by [`slopos_ostd::sync::lock_graph`] and
//! panics — keep epoch read-side regions structurally short and pure-
//! RCU-read.
//!
//! ## ConnId encoding
//!
//! ```text
//! Bit 31:      1 = listener, 0 = shard
//! Bits [11:8]: shard index (0..15) — only when bit 31 = 0
//! Bits  [7:0]: slot index within shard (0..3) or listener table (0..15)
//! ```

use core::sync::atomic::{AtomicU16, Ordering};

use slopos_ostd::KBox;
use slopos_ostd::sync::{Epoch, LOCK_LEVEL_RESOURCE, RcuCell, SpinLock};

use super::buffer::TcpBufferPair;
use super::pcb::{Pcb, PcbState};
use super::tuple::{TcpError, TcpTuple};
use crate::timer::NET_TIMER_WHEEL;

// =============================================================================
// Constants
// =============================================================================

/// Number of independently-hashed shards for established connections.
pub const NUM_SHARDS: usize = 16;

/// Slots per shard. 16 × 4 = 64 total established-connection capacity.
pub const SLOTS_PER_SHARD: usize = 4;

/// Maximum number of LISTEN sockets.
pub const MAX_LISTENERS: usize = 16;

/// Total per-slot PCB lock count across all shards.
pub const TOTAL_PCB_SLOTS: usize = NUM_SHARDS * SLOTS_PER_SHARD;

// =============================================================================
// ConnId — type-safe connection handle
// =============================================================================

/// Type-safe handle to a connection slot. Encodes whether the
/// connection is in a shard or in the listener table, plus the
/// shard/slot indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ConnId(pub u32);

impl ConnId {
    const LISTENER_BIT: u32 = 1 << 31;

    /// Sentinel "no connection" value.
    pub const SENTINEL: Self = Self(u32::MAX);

    /// Create a ConnId for an established connection in a shard.
    #[inline]
    pub fn new_shard(shard: usize, slot: usize) -> Self {
        Self(((shard as u32) << 8) | (slot as u32))
    }

    /// Create a ConnId for a listener.
    #[inline]
    pub fn new_listener(slot: usize) -> Self {
        Self(Self::LISTENER_BIT | (slot as u32))
    }

    /// Whether this id refers to a listener-table entry.
    #[inline]
    pub fn is_listener(self) -> bool {
        self.0 & Self::LISTENER_BIT != 0
    }

    /// Shard index (only valid when `!is_listener()`).
    #[inline]
    pub fn shard(self) -> usize {
        ((self.0 >> 8) & 0xFF) as usize
    }

    /// Slot index within the shard or listener table.
    #[inline]
    pub fn slot(self) -> usize {
        (self.0 & 0xFF) as usize
    }

    /// Linear index into [`TCP_PCB_SLOTS`] (only valid when `!is_listener()`).
    #[inline]
    pub fn linear_slot(self) -> usize {
        self.shard() * SLOTS_PER_SHARD + self.slot()
    }

    /// True if the encoded shard/slot indices fall inside the static
    /// table dimensions. Out-of-range ids (e.g. user-supplied integers
    /// for negative-path tests) are rejected up-front by the
    /// `with_pcb*` accessors rather than panicking on an array index.
    #[inline]
    pub fn is_well_formed(self) -> bool {
        if self.is_listener() {
            self.slot() < MAX_LISTENERS
        } else {
            self.shard() < NUM_SHARDS && self.slot() < SLOTS_PER_SHARD
        }
    }
}

// =============================================================================
// Hash function — FNV-1a → masked to NUM_SHARDS
// =============================================================================

pub(super) fn tcp_hash(tuple: &TcpTuple) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    let fnv_prime: u64 = 0x100000001b3;
    for &b in tuple
        .local_ip
        .iter()
        .chain(&tuple.local_port.to_ne_bytes())
        .chain(tuple.remote_ip.iter())
        .chain(&tuple.remote_port.to_ne_bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(fnv_prime);
    }
    ((h >> 48) as usize) & (NUM_SHARDS - 1)
}

// =============================================================================
// RCU-published indices
// =============================================================================

/// Immutable per-shard tuple table. Cloned + mutated + RCU-published on
/// every install/release. Small (≈48 bytes) — cheap to deep-copy.
#[derive(Clone, Default)]
pub struct TcpShardIndex {
    pub tuples: [Option<TcpTuple>; SLOTS_PER_SHARD],
}

impl TcpShardIndex {
    pub const fn empty() -> Self {
        Self {
            tuples: [const { None }; SLOTS_PER_SHARD],
        }
    }

    /// Find the slot whose stored tuple equals `t`.
    #[inline]
    fn find_exact(&self, t: &TcpTuple) -> Option<usize> {
        for (i, slot) in self.tuples.iter().enumerate() {
            if slot.as_ref() == Some(t) {
                return Some(i);
            }
        }
        None
    }

    /// First empty slot index, or `None` if full.
    #[inline]
    fn first_free(&self) -> Option<usize> {
        self.tuples.iter().position(|s| s.is_none())
    }

    /// True if any tuple binds `(local_ip, local_port)`. Wildcards
    /// honoured both ways (caller-supplied `local_ip == [0;4]` and
    /// stored `tuple.local_ip == [0;4]`).
    #[inline]
    fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.tuples.iter().any(|slot| {
            let Some(t) = slot else { return false };
            t.local_port == local_port
                && (t.local_ip == [0; 4] || local_ip == [0; 4] || t.local_ip == local_ip)
        })
    }

    #[inline]
    fn active_count(&self) -> usize {
        self.tuples.iter().filter(|s| s.is_some()).count()
    }
}

/// Listener key — local address binding only (LISTEN sockets do not
/// hash by 4-tuple because their remote is wildcard).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerKey {
    pub local_ip: [u8; 4],
    pub local_port: u16,
}

/// Immutable listener-key table. Cloned + mutated + RCU-published on
/// every listener install/release.
#[derive(Clone, Default)]
pub struct ListenerIndex {
    pub entries: [Option<ListenerKey>; MAX_LISTENERS],
}

impl ListenerIndex {
    pub const fn empty() -> Self {
        Self {
            entries: [const { None }; MAX_LISTENERS],
        }
    }

    /// Match `(local_ip, local_port)` against the index. Exact-IP first,
    /// then wildcard (0.0.0.0).
    #[inline]
    fn find_by_port(&self, local_ip: [u8; 4], local_port: u16) -> Option<usize> {
        for (i, slot) in self.entries.iter().enumerate() {
            if let Some(k) = slot {
                if k.local_port == local_port && k.local_ip == local_ip {
                    return Some(i);
                }
            }
        }
        for (i, slot) in self.entries.iter().enumerate() {
            if let Some(k) = slot {
                if k.local_port == local_port && k.local_ip == [0; 4] {
                    return Some(i);
                }
            }
        }
        None
    }

    #[inline]
    fn first_free(&self) -> Option<usize> {
        self.entries.iter().position(|s| s.is_none())
    }

    #[inline]
    fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.entries.iter().any(|slot| {
            let Some(k) = slot else { return false };
            k.local_port == local_port
                && (k.local_ip == [0; 4] || local_ip == [0; 4] || k.local_ip == local_ip)
        })
    }

    #[inline]
    fn active_count(&self) -> usize {
        self.entries.iter().filter(|s| s.is_some()).count()
    }
}

// =============================================================================
// Per-slot mutation state
// =============================================================================

/// PCB + its lazy receive/send buffer. Only Data-phase connections
/// carry a `Some(buffer)`; all other states keep it `None`.
pub struct PcbSlot {
    pub pcb: Pcb,
    pub buffer: Option<TcpBufferPair>,
}

// =============================================================================
// Statics
// =============================================================================

/// Net-stack epoch. Held across `find` / `port_in_use` /
/// `active_count` lookups so RCU grace periods are scoped to the net
/// stack rather than a kernel-wide implicit read-side.
pub static NET_EPOCH: Epoch = Epoch::new();

/// RCU-published per-shard tuple indices. Empty after boot; populated
/// by the first install into each shard.
pub static TCP_SHARDS_INDEX: [RcuCell<TcpShardIndex>; NUM_SHARDS] =
    [const { RcuCell::empty() }; NUM_SHARDS];

/// RCU-published listener-key index.
pub static TCP_LISTENERS_INDEX: RcuCell<ListenerIndex> = RcuCell::empty();

/// Per-slot PCB locks. Index = `shard * SLOTS_PER_SHARD + slot`.
pub static TCP_PCB_SLOTS: [SpinLock<Option<PcbSlot>>; TOTAL_PCB_SLOTS] = {
    const SLOT: SpinLock<Option<PcbSlot>> = SpinLock::new(None, LOCK_LEVEL_RESOURCE);
    [SLOT; TOTAL_PCB_SLOTS]
};

/// Per-listener-slot locks.
pub static TCP_LISTENER_SLOTS: [SpinLock<Option<Pcb>>; MAX_LISTENERS] = {
    const SLOT: SpinLock<Option<Pcb>> = SpinLock::new(None, LOCK_LEVEL_RESOURCE);
    [SLOT; MAX_LISTENERS]
};

/// Per-shard write-serialisation locks. Held only during the
/// "pick-slot → install/clear PCB → publish new index" critical
/// section. Mutation-only paths (input/send/recv/timers) do not
/// acquire this.
static TCP_SHARDS_WRITE: [SpinLock<()>; NUM_SHARDS] = {
    const WL: SpinLock<()> = SpinLock::new((), LOCK_LEVEL_RESOURCE);
    [WL; NUM_SHARDS]
};

/// Listener-side write-serialisation lock.
static TCP_LISTENERS_WRITE: SpinLock<()> = SpinLock::new((), LOCK_LEVEL_RESOURCE);

/// Global ephemeral port counter (RFC 6335 range 49152–65535).
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

// =============================================================================
// Read path — wait-free under NET_EPOCH
// =============================================================================

/// Find a connection by 4-tuple. Exact shard match first; listener
/// fallback second. No `SpinLock` is acquired on this path — the read
/// is one `Epoch::enter` + two `RcuCell::load`s.
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    let _g = NET_EPOCH.enter();

    let shard_idx = tcp_hash(tuple);
    if let Some(idx) = TCP_SHARDS_INDEX[shard_idx].load() {
        if let Some(slot) = idx.find_exact(tuple) {
            return Some(ConnId::new_shard(shard_idx, slot));
        }
    }

    if let Some(idx) = TCP_LISTENERS_INDEX.load() {
        if let Some(slot) = idx.find_by_port(tuple.local_ip, tuple.local_port) {
            return Some(ConnId::new_listener(slot));
        }
    }

    None
}

/// True if any PCB binds `(local_ip, local_port)`. Lock-free.
pub fn port_in_use(local_ip: [u8; 4], local_port: u16) -> bool {
    let _g = NET_EPOCH.enter();
    for cell in TCP_SHARDS_INDEX.iter() {
        if let Some(idx) = cell.load() {
            if idx.port_in_use(local_ip, local_port) {
                return true;
            }
        }
    }
    if let Some(idx) = TCP_LISTENERS_INDEX.load() {
        return idx.port_in_use(local_ip, local_port);
    }
    false
}

/// Number of active connections across all shards + listeners. Lock-free.
pub fn active_count() -> usize {
    let _g = NET_EPOCH.enter();
    let mut count = 0;
    for cell in TCP_SHARDS_INDEX.iter() {
        if let Some(idx) = cell.load() {
            count += idx.active_count();
        }
    }
    if let Some(idx) = TCP_LISTENERS_INDEX.load() {
        count += idx.active_count();
    }
    count
}

/// Allocate an ephemeral port (RFC 6335 49152–65535) that is not
/// currently bound by any PCB. Uses the lock-free `port_in_use`.
pub fn alloc_ephemeral_port() -> Option<u16> {
    for _ in 0..16384u32 {
        let p = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        if p < 49152 {
            NEXT_EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
            continue;
        }
        if !port_in_use([0; 4], p) {
            return Some(p);
        }
    }
    None
}

// =============================================================================
// Write path — install / release
// =============================================================================

fn load_shard_index(shard_idx: usize) -> TcpShardIndex {
    TCP_SHARDS_INDEX[shard_idx]
        .load()
        .map(|g| (*g).clone())
        .unwrap_or_default()
}

fn load_listener_index() -> ListenerIndex {
    TCP_LISTENERS_INDEX
        .load()
        .map(|g| (*g).clone())
        .unwrap_or_default()
}

/// Install an established/transient connection (SYN_SENT / SYN_RECV /
/// Data / TimeWait) into its shard.
pub fn install_established(
    tuple: TcpTuple,
    state: PcbState,
    init: impl FnOnce(&mut Pcb),
) -> Result<ConnId, TcpError> {
    let shard_idx = tcp_hash(&tuple);
    let _w = TCP_SHARDS_WRITE[shard_idx].lock();

    let mut idx = load_shard_index(shard_idx);
    let free_slot = idx.first_free().ok_or(TcpError::TableFull)?;

    // Install PCB in the per-slot lock. Held briefly; no nested
    // acquire while it's live other than the outer write lock.
    {
        let mut slot = TCP_PCB_SLOTS[shard_idx * SLOTS_PER_SHARD + free_slot].lock();
        debug_assert!(
            slot.is_none(),
            "tcp::table: free slot in index but per-slot lock occupied"
        );
        let mut pcb = Pcb::new(tuple, state);
        init(&mut pcb);
        *slot = Some(PcbSlot { pcb, buffer: None });
    }

    // Publish the new index. `RcuCell::replace` schedules the displaced
    // box for deferred drop via `rcu_call_typed` — readers in-flight on
    // the old version complete safely.
    idx.tuples[free_slot] = Some(tuple);
    let new_box = KBox::try_new(idx)?;
    TCP_SHARDS_INDEX[shard_idx].replace(new_box);

    Ok(ConnId::new_shard(shard_idx, free_slot))
}

/// Install a LISTEN socket.
pub fn install_listener(
    tuple: TcpTuple,
    state: PcbState,
    init: impl FnOnce(&mut Pcb),
) -> Result<ConnId, TcpError> {
    let _w = TCP_LISTENERS_WRITE.lock();

    let mut idx = load_listener_index();
    let free_slot = idx.first_free().ok_or(TcpError::TableFull)?;

    {
        let mut slot = TCP_LISTENER_SLOTS[free_slot].lock();
        debug_assert!(
            slot.is_none(),
            "tcp::table: free listener slot in index but per-slot lock occupied"
        );
        let mut pcb = Pcb::new(tuple, state);
        init(&mut pcb);
        *slot = Some(pcb);
    }

    idx.entries[free_slot] = Some(ListenerKey {
        local_ip: tuple.local_ip,
        local_port: tuple.local_port,
    });
    let new_box = KBox::try_new(idx)?;
    TCP_LISTENERS_INDEX.replace(new_box);

    Ok(ConnId::new_listener(free_slot))
}

/// Release a connection by id. Idempotent on a stale id (no-op if the
/// slot is already empty). Returns early on a malformed id (e.g. a
/// hand-crafted ConnId from a negative-path test).
pub fn release(id: ConnId) {
    if !id.is_well_formed() {
        return;
    }
    if id.is_listener() {
        let _w = TCP_LISTENERS_WRITE.lock();
        let mut idx = load_listener_index();
        idx.entries[id.slot()] = None;
        if let Ok(new_box) = KBox::try_new(idx) {
            TCP_LISTENERS_INDEX.replace(new_box);
        }
        let mut slot = TCP_LISTENER_SLOTS[id.slot()].lock();
        if let Some(pcb) = slot.as_ref() {
            cancel_pcb_timers(pcb);
        }
        *slot = None;
        return;
    }

    let shard_idx = id.shard();
    let _w = TCP_SHARDS_WRITE[shard_idx].lock();
    let mut idx = load_shard_index(shard_idx);
    idx.tuples[id.slot()] = None;
    if let Ok(new_box) = KBox::try_new(idx) {
        TCP_SHARDS_INDEX[shard_idx].replace(new_box);
    }
    let mut slot = TCP_PCB_SLOTS[id.linear_slot()].lock();
    if let Some(s) = slot.as_ref() {
        cancel_pcb_timers(&s.pcb);
    }
    *slot = None;
}

// =============================================================================
// Per-slot accessors
// =============================================================================

/// Read-only closure access to a PCB.
pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard.as_ref().map(f)
    } else {
        let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard.as_ref().map(|s| f(&s.pcb))
    }
}

/// Mutable closure access to a PCB.
pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let mut guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard.as_mut().map(f)
    } else {
        let mut guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard.as_mut().map(|s| f(&mut s.pcb))
    }
}

/// Closure access to a PCB *and* its lazy buffer slot. The buffer
/// slot is given by mutable reference so the closure can allocate
/// (`*buf = Some(TcpBufferPair::new(..)?)`) or free (`*buf = None`)
/// without re-acquiring the per-slot lock. Listeners receive a
/// transient `&mut None` — writing through it has no observable
/// effect since listeners never carry a buffer.
pub fn with_pcb_and_bufs<T>(
    id: ConnId,
    f: impl FnOnce(&mut Pcb, &mut Option<TcpBufferPair>) -> T,
) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let mut guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard.as_mut().map(|pcb| {
            let mut none_buf: Option<TcpBufferPair> = None;
            f(pcb, &mut none_buf)
        })
    } else {
        let mut guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard.as_mut().map(|s| f(&mut s.pcb, &mut s.buffer))
    }
}

/// Convenience: read-only peek at the lazy buffer's send/recv state
/// (e.g. for `recv_available`, `send_buffer_space`).
pub fn with_bufs<T>(id: ConnId, f: impl FnOnce(&TcpBufferPair) -> T) -> Option<T> {
    if id.is_listener() || !id.is_well_formed() {
        return None;
    }
    let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
    guard.as_ref().and_then(|s| s.buffer.as_ref().map(f))
}

/// True if `id` is established and currently has a buffer allocated.
pub fn has_buffer(id: ConnId) -> bool {
    if id.is_listener() || !id.is_well_formed() {
        return false;
    }
    let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
    guard.as_ref().is_some_and(|s| s.buffer.is_some())
}

// =============================================================================
// Iteration helpers (timers / housekeeping)
// =============================================================================

/// Snapshot the currently-live shard ConnIds from the published
/// indices. Lock-free (one `Epoch::enter` + `NUM_SHARDS` RCU loads).
/// Returned ConnIds may become stale before the caller acts on them —
/// per-slot lock acquires in the caller's loop body must tolerate a
/// vacant slot.
pub fn snapshot_shard_conn_ids(out: &mut [Option<ConnId>; TOTAL_PCB_SLOTS]) -> usize {
    let _g = NET_EPOCH.enter();
    let mut n = 0;
    for (shard_idx, cell) in TCP_SHARDS_INDEX.iter().enumerate() {
        if let Some(idx) = cell.load() {
            for (slot, entry) in idx.tuples.iter().enumerate() {
                if entry.is_some() && n < out.len() {
                    out[n] = Some(ConnId::new_shard(shard_idx, slot));
                    n += 1;
                }
            }
        }
    }
    n
}

// =============================================================================
// Test helpers
// =============================================================================

/// Clear every PCB slot and reset the index cells. Test-only.
pub fn clear_all() {
    for shard_idx in 0..NUM_SHARDS {
        let _w = TCP_SHARDS_WRITE[shard_idx].lock();
        for s in 0..SLOTS_PER_SHARD {
            let mut guard = TCP_PCB_SLOTS[shard_idx * SLOTS_PER_SHARD + s].lock();
            if let Some(slot) = guard.as_ref() {
                cancel_pcb_timers(&slot.pcb);
            }
            *guard = None;
        }
        if let Ok(new_box) = KBox::try_new(TcpShardIndex::empty()) {
            TCP_SHARDS_INDEX[shard_idx].replace(new_box);
        }
    }
    {
        let _w = TCP_LISTENERS_WRITE.lock();
        for s in 0..MAX_LISTENERS {
            let mut guard = TCP_LISTENER_SLOTS[s].lock();
            if let Some(pcb) = guard.as_ref() {
                cancel_pcb_timers(pcb);
            }
            *guard = None;
        }
        if let Ok(new_box) = KBox::try_new(ListenerIndex::empty()) {
            TCP_LISTENERS_INDEX.replace(new_box);
        }
    }
    NEXT_EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}

// =============================================================================
// Helpers
// =============================================================================

fn cancel_pcb_timers(pcb: &Pcb) {
    match &pcb.state {
        PcbState::Listen(_) => {}
        PcbState::SynSent(s) => {
            if let Some(token) = s.retransmit_token {
                NET_TIMER_WHEEL.cancel(token);
            }
        }
        PcbState::SynRecv(s) => {
            if let Some(token) = s.retransmit_token {
                NET_TIMER_WHEEL.cancel(token);
            }
        }
        PcbState::Data(d) => {
            if let Some(token) = d.retransmit_token {
                NET_TIMER_WHEEL.cancel(token);
            }
            if let Some(token) = d.keepalive_token {
                NET_TIMER_WHEEL.cancel(token);
            }
            if let Some(token) = d.fin_wait2_token {
                NET_TIMER_WHEEL.cancel(token);
            }
        }
        PcbState::TimeWait(tw) => {
            if let Some(token) = tw.expire_token {
                NET_TIMER_WHEEL.cancel(token);
            }
        }
    }
}
