//! Sharded TCP connection table with per-shard locking.
//!
//! Replaces both the old single-lock `PcbTable` and the shadow
//! `TcpDemuxTable` with one unified structure: 16 hash-indexed shards
//! for established/transient connections, plus a separate small table
//! for LISTEN sockets.
//!
//! ## Sharding
//!
//! Established connections (SYN_SENT, SYN_RECV, DATA, TIME_WAIT) live
//! in [`TCP_SHARDS`]: an array of 16 independently-locked [`TcpShard`]s.
//! The incoming 4-tuple is hashed (FNV-1a) to pick a shard; within that
//! shard, 8 slots are scanned linearly (cache-friendly, no probe chains).
//!
//! Listeners (LISTEN state) live in a separate [`TCP_LISTENERS`] table
//! keyed by local 2-tuple.  Linear scan of ≤16 entries is fine for the
//! handful of listening sockets a kernel serves.
//!
//! ## Lock ordering
//!
//! All shard and listener locks are `LOCK_LEVEL_RESOURCE` (1).
//! **Never hold two RESOURCE locks simultaneously** — unlock one before
//! locking another.  Timer scheduling (RESOURCE → REGISTRY) is ascending.
//!
//! ## Lazy buffers
//!
//! Each shard stores buffers as `[Option<TcpBufferPair>; SLOTS_PER_SHARD]`
//! parallel to the PCB slots.  Only Data-phase connections allocate a
//! buffer (`Some`); all other states keep `None`.  Zero `unsafe`.
//!
//! ## ConnId encoding
//!
//! ```text
//! Bit 31:      1 = listener, 0 = shard
//! Bits [11:8]: shard index (0..15) — only when bit 31 = 0
//! Bits  [7:0]: slot index within shard (0..7) or listener table (0..15)
//! ```

use core::sync::atomic::{AtomicU16, Ordering};

use slopos_sync::{IrqMutex, LOCK_LEVEL_RESOURCE};

use super::buffer::TcpBufferPair;
use super::pcb::{Pcb, PcbState};
use super::tuple::{TcpError, TcpTuple};
use crate::timer::NET_TIMER_WHEEL;

// =============================================================================
// Constants
// =============================================================================

/// Number of independently-locked shards for established connections.
pub const NUM_SHARDS: usize = 16;

/// Slots per shard.  16 × 4 = 64 total established-connection capacity.
///
/// Kept at 4 (not 8) so the static buffer memory matches the pre-sharding
/// footprint (~4.1 MB).  Doubling to 8 would push BSS past the point
/// where the scheduler's task-creation tests run out of kernel memory.
pub const SLOTS_PER_SHARD: usize = 4;

/// Maximum number of LISTEN sockets.
pub const MAX_LISTENERS: usize = 16;

// =============================================================================
// ConnId — type-safe connection handle
// =============================================================================

/// Type-safe handle to a connection slot.
///
/// Encodes whether the connection is in a shard (established) or in the
/// listener table, plus the shard/slot indices.
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

    /// Whether this id refers to a listener table entry.
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
}

// =============================================================================
// Hash function
// =============================================================================

/// FNV-1a hash of a TCP 4-tuple, masked to [`NUM_SHARDS`].
pub(super) fn tcp_hash(tuple: &TcpTuple) -> usize {
    let mut h: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
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
// TcpShard — one hash bucket for established connections
// =============================================================================

/// A shard of the established-connection table.
///
/// Contains parallel PCB and buffer arrays (same index), a la the old
/// `PcbTable` but much smaller (8 slots).
pub struct TcpShard {
    pcbs: [Option<Pcb>; SLOTS_PER_SHARD],
    buffers: [Option<TcpBufferPair>; SLOTS_PER_SHARD],
}

impl TcpShard {
    const fn new() -> Self {
        const NONE_PCB: Option<Pcb> = None;
        const NONE_BUF: Option<TcpBufferPair> = None;
        Self {
            pcbs: [NONE_PCB; SLOTS_PER_SHARD],
            buffers: [NONE_BUF; SLOTS_PER_SHARD],
        }
    }

    /// Find a PCB by exact 4-tuple match within this shard.
    pub fn find_exact(&self, tuple: &TcpTuple) -> Option<usize> {
        for (i, slot) in self.pcbs.iter().enumerate() {
            if let Some(pcb) = slot {
                if pcb.tuple.local_ip == tuple.local_ip
                    && pcb.tuple.local_port == tuple.local_port
                    && pcb.tuple.remote_ip == tuple.remote_ip
                    && pcb.tuple.remote_port == tuple.remote_port
                {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Install a PCB into the first free slot.
    pub fn install(
        &mut self,
        tuple: TcpTuple,
        state: PcbState,
        init: impl FnOnce(&mut Pcb),
    ) -> Option<usize> {
        for (i, slot) in self.pcbs.iter_mut().enumerate() {
            if slot.is_none() {
                let pcb = slot.insert(Pcb::new(tuple, state));
                init(pcb);
                return Some(i);
            }
        }
        None
    }

    /// Release the PCB at `slot`.  Cancels timers, drops buffer.
    pub fn release(&mut self, slot: usize) {
        if let Some(pcb) = self.pcbs[slot].as_ref() {
            cancel_pcb_timers(pcb);
        }
        self.pcbs[slot] = None;
        self.buffers[slot] = None;
    }

    pub fn get(&self, slot: usize) -> Option<&Pcb> {
        self.pcbs.get(slot)?.as_ref()
    }

    pub fn get_mut(&mut self, slot: usize) -> Option<&mut Pcb> {
        self.pcbs.get_mut(slot)?.as_mut()
    }

    pub fn get_with_bufs(&mut self, slot: usize) -> Option<(&mut Pcb, &mut TcpBufferPair)> {
        let Self { pcbs, buffers } = self;
        let pcb = pcbs.get_mut(slot)?.as_mut()?;
        let bufs = buffers.get_mut(slot)?.as_mut()?;
        Some((pcb, bufs))
    }

    pub fn get_pcb_and_opt_bufs(
        &mut self,
        slot: usize,
    ) -> Option<(&mut Pcb, Option<&mut TcpBufferPair>)> {
        let Self { pcbs, buffers } = self;
        let pcb = pcbs.get_mut(slot)?.as_mut()?;
        let bufs = buffers.get_mut(slot).and_then(|b| b.as_mut());
        Some((pcb, bufs))
    }

    pub fn bufs(&self, slot: usize) -> Option<&TcpBufferPair> {
        self.buffers.get(slot)?.as_ref()
    }

    pub fn bufs_mut(&mut self, slot: usize) -> Option<&mut TcpBufferPair> {
        self.buffers.get_mut(slot)?.as_mut()
    }

    pub fn alloc_buffer_for(&mut self, slot: usize) -> Result<(), slopos_alloc::AllocError> {
        debug_assert!(
            self.buffers[slot].is_none(),
            "alloc_buffer_for: slot {} already has a buffer",
            slot
        );
        self.buffers[slot] = Some(TcpBufferPair::new(super::buffer::TCP_BUFFER_SIZE)?);
        Ok(())
    }

    pub fn free_buffer_for(&mut self, slot: usize) {
        self.buffers[slot] = None;
    }

    pub fn has_buffer(&self, slot: usize) -> bool {
        self.buffers.get(slot).is_some_and(|b| b.is_some())
    }

    /// Check whether any PCB in this shard binds the given local address.
    pub fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.pcbs.iter().any(|slot| {
            let Some(pcb) = slot else { return false };
            pcb.tuple.local_port == local_port
                && (pcb.tuple.local_ip == [0; 4]
                    || local_ip == [0; 4]
                    || pcb.tuple.local_ip == local_ip)
        })
    }

    pub fn active_count(&self) -> usize {
        self.pcbs.iter().filter(|s| s.is_some()).count()
    }

    /// Iterate live PCBs with their buffers.
    pub fn iter_mut_with_bufs(
        &mut self,
    ) -> impl Iterator<Item = (usize, &mut Pcb, &mut TcpBufferPair)> {
        let Self { pcbs, buffers } = self;
        pcbs.iter_mut()
            .zip(buffers.iter_mut())
            .enumerate()
            .filter_map(|(i, (slot, buf))| {
                let pcb = slot.as_mut()?;
                let bufs = buf.as_mut()?;
                Some((i, pcb, bufs))
            })
    }

    /// Clear all slots and buffers, cancelling timers.
    pub fn clear(&mut self) {
        for (slot, buf) in self.pcbs.iter_mut().zip(self.buffers.iter_mut()) {
            if let Some(pcb) = slot.as_ref() {
                cancel_pcb_timers(pcb);
            }
            *slot = None;
            *buf = None;
        }
    }
}

// =============================================================================
// ListenerTable — small table for LISTEN sockets
// =============================================================================

/// Storage for LISTEN-state PCBs, keyed by local 2-tuple.
///
/// Listeners don't hash to shards (their remote is wildcard), so they
/// live in a separate small table with linear scan.
pub struct ListenerTable {
    slots: [Option<Pcb>; MAX_LISTENERS],
}

impl ListenerTable {
    const fn new() -> Self {
        const NONE: Option<Pcb> = None;
        Self {
            slots: [NONE; MAX_LISTENERS],
        }
    }

    /// Find a listener matching `(local_ip, local_port)`.
    /// Tries exact IP first, then wildcard (0.0.0.0).
    pub fn find_by_port(&self, local_ip: [u8; 4], local_port: u16) -> Option<usize> {
        // Exact IP match.
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(pcb) = slot {
                if pcb.tuple.local_port == local_port && pcb.tuple.local_ip == local_ip {
                    return Some(i);
                }
            }
        }
        // Wildcard match.
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(pcb) = slot {
                if pcb.tuple.local_port == local_port && pcb.tuple.local_ip == [0; 4] {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Install a listener into the first free slot.
    pub fn install(
        &mut self,
        tuple: TcpTuple,
        state: PcbState,
        init: impl FnOnce(&mut Pcb),
    ) -> Option<usize> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                let pcb = slot.insert(Pcb::new(tuple, state));
                init(pcb);
                return Some(i);
            }
        }
        None
    }

    pub fn release(&mut self, slot: usize) {
        if let Some(pcb) = self.slots[slot].as_ref() {
            cancel_pcb_timers(pcb);
        }
        self.slots[slot] = None;
    }

    pub fn get(&self, slot: usize) -> Option<&Pcb> {
        self.slots.get(slot)?.as_ref()
    }

    pub fn get_mut(&mut self, slot: usize) -> Option<&mut Pcb> {
        self.slots.get_mut(slot)?.as_mut()
    }

    pub fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.slots.iter().any(|slot| {
            let Some(pcb) = slot else { return false };
            pcb.tuple.local_port == local_port
                && (pcb.tuple.local_ip == [0; 4]
                    || local_ip == [0; 4]
                    || pcb.tuple.local_ip == local_ip)
        })
    }

    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            if let Some(pcb) = slot.as_ref() {
                cancel_pcb_timers(pcb);
            }
            *slot = None;
        }
    }

    /// Find a listener by socket index (for unregister on socket close).
    pub fn find_by_socket_idx(&self, sock_idx: u32) -> Option<usize> {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(pcb) = slot {
                if pcb.socket_id.map(|s| s.0) == Some(sock_idx) {
                    return Some(i);
                }
            }
        }
        None
    }
}

// =============================================================================
// Statics
// =============================================================================

/// 16 independently-locked shards for established connections.
pub static TCP_SHARDS: [IrqMutex<TcpShard>; NUM_SHARDS] = {
    const SHARD: IrqMutex<TcpShard> = IrqMutex::new(TcpShard::new(), LOCK_LEVEL_RESOURCE);
    [SHARD; NUM_SHARDS]
};

/// Listener table (LISTEN-state PCBs only).
pub static TCP_LISTENERS: IrqMutex<ListenerTable> =
    IrqMutex::new(ListenerTable::new(), LOCK_LEVEL_RESOURCE);

/// Global ephemeral port counter (RFC 6335 range 49152–65535).
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

// =============================================================================
// Module-level API — encapsulates shard dispatch
// =============================================================================

/// Find a connection by 4-tuple.
///
/// First checks the hash-indexed shard for an exact match, then falls
/// back to the listener table for a local-port wildcard match.
///
/// **Lock protocol:** locks one shard, releases, then locks listeners.
/// Never holds two RESOURCE locks simultaneously.
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    // Phase 1: exact match in the appropriate shard.
    let shard_idx = tcp_hash(tuple);
    {
        let shard = TCP_SHARDS[shard_idx].lock();
        if let Some(slot) = shard.find_exact(tuple) {
            return Some(ConnId::new_shard(shard_idx, slot));
        }
    }

    // Phase 2: listener wildcard match.
    {
        let listeners = TCP_LISTENERS.lock();
        if let Some(slot) = listeners.find_by_port(tuple.local_ip, tuple.local_port) {
            return Some(ConnId::new_listener(slot));
        }
    }

    None
}

/// Install an established connection (SYN_SENT, SYN_RECV, etc.) into
/// the appropriate shard.
pub fn install_established(
    tuple: TcpTuple,
    state: PcbState,
    init: impl FnOnce(&mut Pcb),
) -> Result<ConnId, TcpError> {
    let shard_idx = tcp_hash(&tuple);
    let mut shard = TCP_SHARDS[shard_idx].lock();
    match shard.install(tuple, state, init) {
        Some(slot) => Ok(ConnId::new_shard(shard_idx, slot)),
        None => Err(TcpError::TableFull),
    }
}

/// Install a LISTEN socket into the listener table.
pub fn install_listener(
    tuple: TcpTuple,
    state: PcbState,
    init: impl FnOnce(&mut Pcb),
) -> Result<ConnId, TcpError> {
    let mut listeners = TCP_LISTENERS.lock();
    match listeners.install(tuple, state, init) {
        Some(slot) => Ok(ConnId::new_listener(slot)),
        None => Err(TcpError::TableFull),
    }
}

/// Release a connection by id.
pub fn release(id: ConnId) {
    if id.is_listener() {
        TCP_LISTENERS.lock().release(id.slot());
    } else {
        TCP_SHARDS[id.shard()].lock().release(id.slot());
    }
}

/// Read-only closure access to a PCB.
pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    if id.is_listener() {
        TCP_LISTENERS.lock().get(id.slot()).map(f)
    } else {
        TCP_SHARDS[id.shard()].lock().get(id.slot()).map(f)
    }
}

/// Mutable closure access to a PCB.
pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    if id.is_listener() {
        TCP_LISTENERS.lock().get_mut(id.slot()).map(f)
    } else {
        TCP_SHARDS[id.shard()].lock().get_mut(id.slot()).map(f)
    }
}

/// Check whether a `(local_ip, local_port)` is bound by any PCB.
///
/// Scans all shards + listener table.  Used by `tcp::listen`.
pub fn port_in_use(local_ip: [u8; 4], local_port: u16) -> bool {
    for shard_lock in TCP_SHARDS.iter() {
        if shard_lock.lock().port_in_use(local_ip, local_port) {
            return true;
        }
    }
    TCP_LISTENERS.lock().port_in_use(local_ip, local_port)
}

/// Allocate an ephemeral port (49152–65535, RFC 6335).
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

/// Count all active connections across shards + listeners.
pub fn active_count() -> usize {
    let mut count = 0;
    for shard_lock in TCP_SHARDS.iter() {
        count += shard_lock.lock().active_count();
    }
    count += TCP_LISTENERS.lock().active_count();
    count
}

/// Clear all connections (test helper).
pub fn clear_all() {
    for shard_lock in TCP_SHARDS.iter() {
        shard_lock.lock().clear();
    }
    TCP_LISTENERS.lock().clear();
    NEXT_EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}

// =============================================================================
// Helpers
// =============================================================================

/// Cancel every outstanding timer token on a PCB.
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

// =============================================================================
// Backward-compat shim (removed items)
// =============================================================================

// PCB_TABLE, PcbTable, MAX_CONNECTIONS — removed in Phase 6b.
// All callers now use TCP_SHARDS / TCP_LISTENERS / module-level functions.
