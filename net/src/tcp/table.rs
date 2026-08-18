//! Lock-free demux + per-slot mutation for the TCP connection table.
//!
//! Reads (`find`) enter [`NET_EPOCH`] and load the RCU-published indices; no
//! `SpinLock` is touched on the dispatch path. Mutating a PCB takes exactly
//! one [`TCP_PCB_SLOTS`] entry; an install or release also takes the matching
//! [`TCP_SHARDS_WRITE`] lock while the new index is published.
//!
//! Lock order: all locks are [`LOCK_LEVEL_RESOURCE`], and the only co-acquire
//! is `TCP_*_WRITE → TCP_*_SLOTS[i]`. Taking a tracked `SpinLock` while a
//! [`NET_EPOCH`] guard is live panics via [`slopos_ostd::sync::lock_graph`],
//! so epoch read-side regions must stay pure-RCU-read.

use core::fmt;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use slopos_ostd::klog_debug;
use slopos_ostd::lock_class;

use slopos_ostd::KBox;
use slopos_ostd::sync::{Epoch, LOCK_LEVEL_RESOURCE, RcuCell, SpinLock};

use super::buffer::TcpBufferPair;
use super::pcb::{Pcb, PcbState};
use super::tuple::{TcpError, TcpTuple};
use crate::timer::NET_TIMER_WHEEL;

pub const NUM_SHARDS: usize = 16;

/// 16 × 4 = 64 total established-connection capacity.
pub const SLOTS_PER_SHARD: usize = 4;

pub const MAX_LISTENERS: usize = 16;

pub const TOTAL_PCB_SLOTS: usize = NUM_SHARDS * SLOTS_PER_SHARD;

/// Type-safe handle to a connection slot.
///
/// ```text
/// bit 31     1 = listener, 0 = shard
/// bits 30:16 generation, never 0
/// bits 15:8  shard index      (shard ids only)
/// bits  7:0  slot index within the shard or the listener table
/// ```
///
/// The generation is what makes an id name a *connection* rather than a slot:
/// slots are recycled, and without it a timer that outlived its connection
/// fires on the connection that replaced it, and a `close` on a stale socket
/// tears down someone else's. 15 bits gives 32767 generations per slot against
/// id holders bounded by the longest TCP timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ConnId(u32);

impl ConnId {
    const LISTENER_BIT: u32 = 1 << 31;
    const GENERATION_SHIFT: u32 = 16;
    const GENERATION_MASK: u32 = 0x7FFF;

    /// Sentinel "no connection" value. Not well-formed: the listener bit is
    /// set and the slot index is past the listener table.
    pub const SENTINEL: Self = Self(u32::MAX);

    #[inline]
    pub fn new_shard(shard: usize, slot: usize, generation: u16) -> Self {
        Self(Self::encode_generation(generation) | ((shard as u32) << 8) | (slot as u32))
    }

    #[inline]
    pub fn new_listener(slot: usize, generation: u16) -> Self {
        Self(Self::LISTENER_BIT | Self::encode_generation(generation) | (slot as u32))
    }

    /// Rebuild an id from a value carried through an interface that only
    /// speaks `u32` — a timer-wheel payload.
    #[inline]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn generation(self) -> u16 {
        ((self.0 >> Self::GENERATION_SHIFT) & Self::GENERATION_MASK) as u16
    }

    #[inline]
    const fn encode_generation(generation: u16) -> u32 {
        ((generation as u32) & Self::GENERATION_MASK) << Self::GENERATION_SHIFT
    }

    #[inline]
    pub fn is_listener(self) -> bool {
        self.0 & Self::LISTENER_BIT != 0
    }

    /// Shard index (only valid when `!is_listener()`).
    #[inline]
    pub fn shard(self) -> usize {
        ((self.0 >> 8) & 0xFF) as usize
    }

    #[inline]
    pub fn slot(self) -> usize {
        (self.0 & 0xFF) as usize
    }

    /// Linear index into [`TCP_PCB_SLOTS`] (only valid when `!is_listener()`).
    #[inline]
    pub fn linear_slot(self) -> usize {
        self.shard() * SLOTS_PER_SHARD + self.slot()
    }

    /// True if the encoded shard/slot indices fall inside the static table
    /// dimensions. The `with_pcb*` accessors check it up front so an
    /// out-of-range id cannot panic on an array index.
    #[inline]
    pub fn is_well_formed(self) -> bool {
        if self.is_listener() {
            self.slot() < MAX_LISTENERS
        } else {
            self.shard() < NUM_SHARDS && self.slot() < SLOTS_PER_SHARD
        }
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_listener() {
            write!(f, "L:{}@{}", self.slot(), self.generation())
        } else {
            write!(f, "{}:{}@{}", self.shard(), self.slot(), self.generation())
        }
    }
}

/// Generations start at 1 and skip 0 on wrap, so a zeroed slot record cannot
/// be mistaken for a live generation.
#[inline]
const fn next_generation(current: u16) -> u16 {
    match current.wrapping_add(1) & (ConnId::GENERATION_MASK as u16) {
        0 => 1,
        next => next,
    }
}

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

/// Immutable per-shard tuple table, cloned + mutated + RCU-published on every
/// install/release.
#[derive(Clone, Default)]
pub struct TcpShardIndex {
    pub tuples: [Option<TcpTuple>; SLOTS_PER_SHARD],
    /// Published in the same RCU snapshot as `tuples` so `find` reads a tuple
    /// and its owner's generation in one coherent step; a separate array would
    /// let it mint an id for the wrong occupant.
    pub generations: [u16; SLOTS_PER_SHARD],
}

impl TcpShardIndex {
    pub const fn empty() -> Self {
        Self {
            tuples: [const { None }; SLOTS_PER_SHARD],
            generations: [1; SLOTS_PER_SHARD],
        }
    }

    #[inline]
    fn find_exact(&self, t: &TcpTuple) -> Option<usize> {
        for (i, slot) in self.tuples.iter().enumerate() {
            if slot.as_ref() == Some(t) {
                return Some(i);
            }
        }
        None
    }

    #[inline]
    fn first_free(&self) -> Option<usize> {
        self.tuples.iter().position(|s| s.is_none())
    }

    /// True if any tuple binds `(local_ip, local_port)`; the `0.0.0.0`
    /// wildcard is honoured on the caller's side and the stored side alike.
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
    /// As [`TcpShardIndex::generations`].
    pub generations: [u16; MAX_LISTENERS],
}

impl ListenerIndex {
    pub const fn empty() -> Self {
        Self {
            entries: [const { None }; MAX_LISTENERS],
            generations: [1; MAX_LISTENERS],
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

/// PCB + its lazy receive/send buffer. Only Data-phase connections
/// carry a `Some(buffer)`; all other states keep it `None`.
pub struct PcbSlot {
    pub pcb: Pcb,
    pub buffer: Option<TcpBufferPair>,
    /// Copy of the index's generation for this slot, so a lookup holding the
    /// slot lock can reject a stale id without reading the RCU index.
    pub generation: u16,
}

/// A listening PCB and its generation. Mirrors [`PcbSlot`]; listeners carry no
/// buffer.
pub struct ListenerSlot {
    pub pcb: Pcb,
    pub generation: u16,
}

/// Net-stack epoch: RCU grace periods are scoped to the net stack rather than
/// to a kernel-wide implicit read-side.
pub static NET_EPOCH: Epoch = Epoch::new(slopos_ostd::epoch_class!("NET_EPOCH"));

/// RCU-published per-shard tuple indices. Empty after boot; populated
/// by the first install into each shard.
pub static TCP_SHARDS_INDEX: [RcuCell<TcpShardIndex>; NUM_SHARDS] =
    [const { RcuCell::empty() }; NUM_SHARDS];

/// RCU-published listener-key index.
pub static TCP_LISTENERS_INDEX: RcuCell<ListenerIndex> = RcuCell::empty();

/// Per-slot PCB locks. Index = `shard * SLOTS_PER_SHARD + slot`.
pub static TCP_PCB_SLOTS: [SpinLock<Option<PcbSlot>>; TOTAL_PCB_SLOTS] = {
    const SLOT: SpinLock<Option<PcbSlot>> =
        SpinLock::new(None, lock_class!("TCP_PCB_SLOTS", LOCK_LEVEL_RESOURCE));
    [SLOT; TOTAL_PCB_SLOTS]
};

pub static TCP_LISTENER_SLOTS: [SpinLock<Option<ListenerSlot>>; MAX_LISTENERS] = {
    const SLOT: SpinLock<Option<ListenerSlot>> =
        SpinLock::new(None, lock_class!("TCP_LISTENER_SLOTS", LOCK_LEVEL_RESOURCE));
    [SLOT; MAX_LISTENERS]
};

/// Lookups rejected because the id named an occupant the slot no longer holds.
/// Non-zero means slot reuse raced a holder of a stale id.
static STALE_LOOKUPS: AtomicU32 = AtomicU32::new(0);

pub fn stale_lookup_count() -> u32 {
    STALE_LOOKUPS.load(Ordering::Relaxed)
}

/// Whether the occupant under a slot lock is the one `id` names.
///
/// Deliberately not reached for a malformed id or an empty slot: the first is
/// a caller error and the second is the ordinary "connection gone" answer.
#[inline]
fn generation_matches(id: ConnId, slot_generation: u16) -> bool {
    if id.generation() == slot_generation {
        return true;
    }
    note_stale_lookup(id, slot_generation);
    false
}

#[cold]
#[inline(never)]
fn note_stale_lookup(id: ConnId, live_generation: u16) {
    STALE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    klog_debug!(
        "tcp: rejected stale id {} against slot generation {}",
        id,
        live_generation
    );
}

/// Per-shard write-serialisation locks, held only across "pick-slot →
/// install/clear PCB → publish new index". Mutation-only paths do not take it.
static TCP_SHARDS_WRITE: [SpinLock<()>; NUM_SHARDS] = {
    const WL: SpinLock<()> =
        SpinLock::new((), lock_class!("TCP_SHARDS_WRITE", LOCK_LEVEL_RESOURCE));
    [WL; NUM_SHARDS]
};

/// Listener-side write-serialisation lock.
static TCP_LISTENERS_WRITE: SpinLock<()> =
    SpinLock::new((), lock_class!("TCP_LISTENERS_WRITE", LOCK_LEVEL_RESOURCE));

/// Global ephemeral port counter (RFC 6335 range 49152–65535).
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

/// Find a connection by 4-tuple: exact shard match first, listener fallback
/// second.
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    let _g = NET_EPOCH.enter();

    let shard_idx = tcp_hash(tuple);
    if let Some(idx) = TCP_SHARDS_INDEX[shard_idx].load() {
        if let Some(slot) = idx.find_exact(tuple) {
            return Some(ConnId::new_shard(shard_idx, slot, idx.generations[slot]));
        }
    }

    if let Some(idx) = TCP_LISTENERS_INDEX.load() {
        if let Some(slot) = idx.find_by_port(tuple.local_ip, tuple.local_port) {
            return Some(ConnId::new_listener(slot, idx.generations[slot]));
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
/// currently bound by any PCB.
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
    // The generation was advanced by whichever `release` vacated this slot,
    // so the installing side only copies it.
    let generation = idx.generations[free_slot];

    // The outer write lock is the only other lock held while this one is live.
    {
        let mut slot = TCP_PCB_SLOTS[shard_idx * SLOTS_PER_SHARD + free_slot].lock();
        debug_assert!(
            slot.is_none(),
            "tcp::table: free slot in index but per-slot lock occupied"
        );
        let mut pcb = Pcb::new(tuple, state);
        init(&mut pcb);
        *slot = Some(PcbSlot {
            pcb,
            buffer: None,
            generation,
        });
    }

    idx.tuples[free_slot] = Some(tuple);
    let new_box = KBox::try_new(idx)?;
    TCP_SHARDS_INDEX[shard_idx].replace(new_box);

    Ok(ConnId::new_shard(shard_idx, free_slot, generation))
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
    let generation = idx.generations[free_slot];

    {
        let mut slot = TCP_LISTENER_SLOTS[free_slot].lock();
        debug_assert!(
            slot.is_none(),
            "tcp::table: free listener slot in index but per-slot lock occupied"
        );
        let mut pcb = Pcb::new(tuple, state);
        init(&mut pcb);
        *slot = Some(ListenerSlot { pcb, generation });
    }

    idx.entries[free_slot] = Some(ListenerKey {
        local_ip: tuple.local_ip,
        local_port: tuple.local_port,
    });
    let new_box = KBox::try_new(idx)?;
    TCP_LISTENERS_INDEX.replace(new_box);

    Ok(ConnId::new_listener(free_slot, generation))
}

/// Release the connection `id` names. A no-op on a malformed id, on an
/// already-empty slot, and on an id whose connection is gone — the last is
/// what stops a `close` on a stale socket from tearing down whichever
/// connection took over the slot.
///
/// Advancing the generation is what makes every id issued for this slot dead
/// from here on, whether or not the slot is ever refilled.
pub fn release(id: ConnId) {
    if !id.is_well_formed() {
        return;
    }
    if id.is_listener() {
        let _w = TCP_LISTENERS_WRITE.lock();
        let mut idx = load_listener_index();
        if !generation_matches(id, idx.generations[id.slot()]) {
            return;
        }
        idx.entries[id.slot()] = None;
        idx.generations[id.slot()] = next_generation(idx.generations[id.slot()]);
        if let Ok(new_box) = KBox::try_new(idx) {
            TCP_LISTENERS_INDEX.replace(new_box);
        }
        let mut slot = TCP_LISTENER_SLOTS[id.slot()].lock();
        if let Some(s) = slot.as_ref() {
            cancel_pcb_timers(&s.pcb);
        }
        *slot = None;
        return;
    }

    let shard_idx = id.shard();
    let _w = TCP_SHARDS_WRITE[shard_idx].lock();
    let mut idx = load_shard_index(shard_idx);
    if !generation_matches(id, idx.generations[id.slot()]) {
        return;
    }
    idx.tuples[id.slot()] = None;
    idx.generations[id.slot()] = next_generation(idx.generations[id.slot()]);
    if let Ok(new_box) = KBox::try_new(idx) {
        TCP_SHARDS_INDEX[shard_idx].replace(new_box);
    }
    let mut slot = TCP_PCB_SLOTS[id.linear_slot()].lock();
    if let Some(s) = slot.as_ref() {
        cancel_pcb_timers(&s.pcb);
    }
    *slot = None;
}

pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard
            .as_ref()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| f(&s.pcb))
    } else {
        let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard
            .as_ref()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| f(&s.pcb))
    }
}

pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let mut guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard
            .as_mut()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| f(&mut s.pcb))
    } else {
        let mut guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard
            .as_mut()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| f(&mut s.pcb))
    }
}

/// Closure access to a PCB *and* its lazy buffer slot. The slot is given by
/// mutable reference so the closure can allocate or free it without
/// re-acquiring the per-slot lock. Listeners receive a transient `&mut None`;
/// writing through it has no observable effect.
pub fn with_pcb_and_bufs<T>(
    id: ConnId,
    f: impl FnOnce(&mut Pcb, &mut Option<TcpBufferPair>) -> T,
) -> Option<T> {
    if !id.is_well_formed() {
        return None;
    }
    if id.is_listener() {
        let mut guard = TCP_LISTENER_SLOTS[id.slot()].lock();
        guard
            .as_mut()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| {
                let mut none_buf: Option<TcpBufferPair> = None;
                f(&mut s.pcb, &mut none_buf)
            })
    } else {
        let mut guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
        guard
            .as_mut()
            .filter(|s| generation_matches(id, s.generation))
            .map(|s| f(&mut s.pcb, &mut s.buffer))
    }
}

pub fn with_bufs<T>(id: ConnId, f: impl FnOnce(&TcpBufferPair) -> T) -> Option<T> {
    if id.is_listener() || !id.is_well_formed() {
        return None;
    }
    let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
    guard
        .as_ref()
        .filter(|s| generation_matches(id, s.generation))
        .and_then(|s| s.buffer.as_ref().map(f))
}

pub fn has_buffer(id: ConnId) -> bool {
    if id.is_listener() || !id.is_well_formed() {
        return false;
    }
    let guard = TCP_PCB_SLOTS[id.linear_slot()].lock();
    guard
        .as_ref()
        .filter(|s| generation_matches(id, s.generation))
        .is_some_and(|s| s.buffer.is_some())
}

/// Run `f` against listener slot `slot`, if it holds one.
///
/// By slot rather than by `ConnId`: SYN-queue timer keys carry no connection
/// id, so the owning listener has to be found by looking.
pub fn with_listener_slot_mut<T>(slot: usize, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    if slot >= MAX_LISTENERS {
        return None;
    }
    let mut guard = TCP_LISTENER_SLOTS[slot].lock();
    guard.as_mut().map(|s| f(&mut s.pcb))
}

/// Snapshot the currently-live shard ConnIds from the published indices.
/// They may go stale before the caller acts on them, so the caller's loop
/// body must tolerate a vacant slot.
pub fn snapshot_shard_conn_ids(out: &mut [Option<ConnId>; TOTAL_PCB_SLOTS]) -> usize {
    let _g = NET_EPOCH.enter();
    let mut n = 0;
    for (shard_idx, cell) in TCP_SHARDS_INDEX.iter().enumerate() {
        if let Some(idx) = cell.load() {
            for (slot, entry) in idx.tuples.iter().enumerate() {
                if entry.is_some() && n < out.len() {
                    out[n] = Some(ConnId::new_shard(shard_idx, slot, idx.generations[slot]));
                    n += 1;
                }
            }
        }
    }
    n
}

/// Clear every PCB slot and reset the index cells. Test-only.
///
/// Generations advance rather than reset: republishing an empty index would
/// hand every slot back its starting generation and revalidate ids the
/// cleared connections had already issued.
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
        let mut idx = load_shard_index(shard_idx);
        for s in 0..SLOTS_PER_SHARD {
            idx.tuples[s] = None;
            idx.generations[s] = next_generation(idx.generations[s]);
        }
        if let Ok(new_box) = KBox::try_new(idx) {
            TCP_SHARDS_INDEX[shard_idx].replace(new_box);
        }
    }
    {
        let _w = TCP_LISTENERS_WRITE.lock();
        for s in 0..MAX_LISTENERS {
            let mut guard = TCP_LISTENER_SLOTS[s].lock();
            if let Some(slot) = guard.as_ref() {
                cancel_pcb_timers(&slot.pcb);
            }
            *guard = None;
        }
        let mut idx = load_listener_index();
        for s in 0..MAX_LISTENERS {
            idx.entries[s] = None;
            idx.generations[s] = next_generation(idx.generations[s]);
        }
        if let Ok(new_box) = KBox::try_new(idx) {
            TCP_LISTENERS_INDEX.replace(new_box);
        }
    }
    NEXT_EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}

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
