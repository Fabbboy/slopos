//! `PcbTable` — the fixed-size storage for active TCP connections.
//!
//! Owns a `[Option<Pcb>; MAX_CONNECTIONS]` slot array, a parallel
//! `[Option<TcpBufferPair>; MAX_CONNECTIONS]` buffer array, a per-boot
//! ephemeral port allocator, and the slot-allocation / lookup / release
//! machinery the glue layer drives under a single `IrqMutex`.
//!
//! ## Lazy buffer lifecycle
//!
//! Buffers are stored as `Option<TcpBufferPair>` in a parallel array
//! indexed by the same slot as the PCB.  Only Data-phase connections
//! allocate a buffer (`Some`); Listen, SynSent, SynRecv, and TimeWait
//! keep `None`.  The glue layer in `tcp::input` calls
//! [`PcbTable::alloc_buffer_for`] on →Data and
//! [`PcbTable::free_buffer_for`] on →TimeWait / release.
//!
//! This design uses **zero `unsafe`**: Rust's struct-level borrow
//! splitting proves `slots` and `buffers` are disjoint, so
//! `get_with_bufs` and `iter_mut_with_bufs` compile without raw pointers.
//!
//! ## Identifier scheme
//!
//! External callers never see a raw `usize` slot index — they hold a
//! [`ConnId`] newtype instead.  Today the id is just the slot index
//! wrapped in `#[repr(transparent)]`; in F.1 (sharded table) the
//! encoding becomes `(shard: u8, slot: u24)` packed into the same
//! 32-bit space, invisibly to callers.

use core::sync::atomic::{AtomicU16, Ordering};

use slopos_sync::{IrqMutex, LOCK_LEVEL_RESOURCE};

use super::buffer::TcpBufferPair;
use super::pcb::{Pcb, PcbState};
use super::tuple::{TcpError, TcpTuple};
use crate::timer::NET_TIMER_WHEEL;

/// Maximum number of simultaneous TCP connections.
pub const MAX_CONNECTIONS: usize = 64;

/// Type-safe handle to a `PcbTable` slot.
///
/// `#[repr(transparent)]` so runtime cost matches a bare `u32`.  F.1
/// will re-encode the bits as `(shard: u8, slot: u24)` once the table
/// is sharded, but the type alias stays the same for callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ConnId(pub u32);

impl ConnId {
    /// Sentinel "no connection" value used in debug formatting.  Not
    /// valid for use as a real handle — `PcbTable::get(SENTINEL)`
    /// returns `None`.
    pub const SENTINEL: Self = Self(u32::MAX);

    /// Raw slot index (pre-F.1).  Becomes a decode helper once the
    /// sharded layout lands.
    #[inline]
    pub fn slot(self) -> usize {
        self.0 as usize
    }
}

/// Global PCB storage.
///
/// Single-mutex form for Phase A–D.  F.1 shards this into
/// `[IrqMutex<PcbShard>; 16]`; callers continue using [`PCB_TABLE`]
/// as the entry point through a thin shard-router.
pub static PCB_TABLE: IrqMutex<PcbTable> = IrqMutex::new(PcbTable::new(), LOCK_LEVEL_RESOURCE);

/// The table itself.  PCB metadata and buffers are stored in parallel
/// `Option` arrays — PCBs are small (~200 bytes), while buffers
/// (~65 KB each) are lazily allocated: `None` until the connection
/// reaches Data state, `None` again on TimeWait / release.
///
/// The parallel-array layout lets Rust's borrow checker prove
/// `slots` and `buffers` are disjoint, so `get_with_bufs` and
/// `iter_mut_with_bufs` need zero `unsafe`.
pub struct PcbTable {
    slots: [Option<Pcb>; MAX_CONNECTIONS],
    buffers: [Option<TcpBufferPair>; MAX_CONNECTIONS],
    next_ephemeral_port: AtomicU16,
}

impl PcbTable {
    pub const fn new() -> Self {
        const NONE_PCB: Option<Pcb> = None;
        const NONE_BUF: Option<TcpBufferPair> = None;
        Self {
            slots: [NONE_PCB; MAX_CONNECTIONS],
            buffers: [NONE_BUF; MAX_CONNECTIONS],
            next_ephemeral_port: AtomicU16::new(49152),
        }
    }

    /// Count the number of active (non-empty) slots.
    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Borrow the PCB at a given id, if alive.
    pub fn get(&self, id: ConnId) -> Option<&Pcb> {
        self.slots.get(id.slot()).and_then(|s| s.as_ref())
    }

    /// Mutably borrow the PCB at a given id, if alive.
    pub fn get_mut(&mut self, id: ConnId) -> Option<&mut Pcb> {
        self.slots.get_mut(id.slot()).and_then(|s| s.as_mut())
    }

    /// Borrow a PCB and its lazily-allocated buffer together.
    ///
    /// Returns `None` if the slot is empty **or** the buffer has not
    /// been allocated yet (non-Data state).  Callers that need the PCB
    /// without a buffer should use [`get_mut`] instead.
    pub fn get_with_bufs(&mut self, id: ConnId) -> Option<(&mut Pcb, &mut TcpBufferPair)> {
        let Self { slots, buffers, .. } = self;
        let slot = id.slot();
        let pcb = slots.get_mut(slot)?.as_mut()?;
        let bufs = buffers.get_mut(slot)?.as_mut()?;
        Some((pcb, bufs))
    }

    /// Borrow a PCB (always) and its buffer (if allocated).
    ///
    /// Returns `None` only if the slot is empty.  The buffer is `Some`
    /// for Data-phase connections, `None` for others.
    pub fn get_pcb_and_opt_bufs(
        &mut self,
        id: ConnId,
    ) -> Option<(&mut Pcb, Option<&mut TcpBufferPair>)> {
        let Self { slots, buffers, .. } = self;
        let slot = id.slot();
        let pcb = slots.get_mut(slot)?.as_mut()?;
        let bufs = buffers.get_mut(slot).and_then(|b| b.as_mut());
        Some((pcb, bufs))
    }

    /// Buffer for a slot (immutable).  Returns `None` when unallocated.
    pub fn bufs(&self, id: ConnId) -> Option<&TcpBufferPair> {
        self.buffers.get(id.slot())?.as_ref()
    }

    /// Buffer for a slot (mutable).  Returns `None` when unallocated.
    pub fn bufs_mut(&mut self, id: ConnId) -> Option<&mut TcpBufferPair> {
        self.buffers.get_mut(id.slot())?.as_mut()
    }

    // -------------------------------------------------------------------------
    // Lazy buffer lifecycle
    // -------------------------------------------------------------------------

    /// Allocate a fresh buffer for the given slot.
    ///
    /// Called by the glue layer when a connection transitions to Data.
    /// Panics (debug) if a buffer is already allocated for this slot.
    pub fn alloc_buffer_for(&mut self, slot: usize) {
        debug_assert!(
            self.buffers[slot].is_none(),
            "alloc_buffer_for: slot {} already has a buffer",
            slot
        );
        self.buffers[slot] = Some(TcpBufferPair::new());
    }

    /// Free the buffer for the given slot (if any).
    ///
    /// Called by the glue layer on →TimeWait or release.
    pub fn free_buffer_for(&mut self, slot: usize) {
        self.buffers[slot] = None;
    }

    /// Check whether a buffer is allocated for a slot.
    pub fn has_buffer(&self, slot: usize) -> bool {
        self.buffers.get(slot).is_some_and(|b| b.is_some())
    }

    // -------------------------------------------------------------------------
    // Slot management
    // -------------------------------------------------------------------------

    /// Allocate a free slot and install a PCB.
    ///
    /// Buffers are **not** allocated here — they are lazily created via
    /// [`alloc_buffer_for`] when the connection transitions to Data state.
    pub fn install_with(
        &mut self,
        tuple: TcpTuple,
        state: PcbState,
        init: impl FnOnce(&mut Pcb),
    ) -> Result<ConnId, TcpError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                let pcb = slot.insert(Pcb::new(tuple, state));
                init(pcb);
                return Ok(ConnId(i as u32));
            }
        }
        Err(TcpError::TableFull)
    }

    /// Free the slot at `id`.  Cancels outstanding timers, drops the
    /// buffer (if allocated), and drops the PCB.
    pub fn release(&mut self, id: ConnId) {
        let slot = id.slot();
        if let Some(s) = self.slots.get_mut(slot) {
            if let Some(pcb) = s.as_ref() {
                Self::cancel_pcb_timers(pcb);
            }
            *s = None;
            self.buffers[slot] = None;
        }
    }

    /// Linear-scan lookup by 4-tuple.  Exact match first, then
    /// LISTEN-socket wildcard match on local port.
    ///
    /// Walks at most `MAX_CONNECTIONS` slots — acceptable for the
    /// single-mutex pre-F.1 table.  F.1 replaces this with a sharded
    /// hash lookup keyed by `tuple`.
    pub fn find(&self, tuple: &TcpTuple) -> Option<ConnId> {
        // Exact match pass.
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(pcb) = slot {
                let t = &pcb.tuple;
                if t.local_ip == tuple.local_ip
                    && t.local_port == tuple.local_port
                    && t.remote_ip == tuple.remote_ip
                    && t.remote_port == tuple.remote_port
                {
                    return Some(ConnId(i as u32));
                }
            }
        }
        // LISTEN wildcard pass.
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(pcb) = slot {
                if matches!(pcb.state, PcbState::Listen(_))
                    && pcb.tuple.local_port == tuple.local_port
                    && (pcb.tuple.local_ip == [0; 4] || pcb.tuple.local_ip == tuple.local_ip)
                {
                    return Some(ConnId(i as u32));
                }
            }
        }
        None
    }

    /// Check whether a `(local_ip, local_port)` is already bound by a
    /// PCB that would shadow a new passive open.  Used by
    /// `tcp::listen` to reject duplicate binds.
    pub fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.slots.iter().any(|slot| {
            let Some(pcb) = slot else {
                return false;
            };
            pcb.tuple.local_port == local_port
                && (pcb.tuple.local_ip == [0; 4]
                    || local_ip == [0; 4]
                    || pcb.tuple.local_ip == local_ip)
        })
    }

    /// Allocate the next ephemeral port from the 49152–65535 range
    /// (RFC 6335).  Skips ports already in use and wraps on overflow.
    /// Returns `None` if the entire range is exhausted.
    pub fn alloc_ephemeral_port(&self) -> Option<u16> {
        for _ in 0..16384u32 {
            let port = self.next_ephemeral_port.fetch_add(1, Ordering::Relaxed);
            if port < 49152 {
                // Wrapped past u16::MAX or below range — reset.
                self.next_ephemeral_port.store(49152, Ordering::Relaxed);
                continue;
            }
            if !self.port_in_use([0; 4], port) {
                return Some(port);
            }
        }
        None
    }

    /// Closure-based read access to a PCB.  Acquires no locks itself —
    /// callers that use the module-level `with_pcb` free function get
    /// the `PCB_TABLE` lock implicitly.
    pub fn with_pcb<T>(&self, id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
        self.get(id).map(f)
    }

    /// Closure-based mutable access to a PCB.
    pub fn with_pcb_mut<T>(&mut self, id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
        self.get_mut(id).map(f)
    }

    /// Iterate over all live PCBs with their ids.  Used by scan
    /// operations (`tcp_delayed_ack_check`, `tcp_retransmit_check`).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (ConnId, &mut Pcb)> {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_mut().map(|pcb| (ConnId(i as u32), pcb)))
    }

    /// Iterate over all live PCBs that have an allocated buffer.
    ///
    /// Splits the borrow between `slots` and `buffers` so both can be
    /// mutated in the loop body.  Only yields PCBs where
    /// `buffers[i].is_some()` (i.e. Data-phase connections).
    pub fn iter_mut_with_bufs(
        &mut self,
    ) -> impl Iterator<Item = (ConnId, &mut Pcb, &mut TcpBufferPair)> {
        let Self { slots, buffers, .. } = self;
        slots
            .iter_mut()
            .zip(buffers.iter_mut())
            .enumerate()
            .filter_map(|(i, (slot, buf))| {
                let pcb = slot.as_mut()?;
                let bufs = buf.as_mut()?;
                Some((ConnId(i as u32), pcb, bufs))
            })
    }

    /// Raw mutable access to the slot array — only for `tcp_reset_all`
    /// and similar admin paths.
    pub fn slots_mut(&mut self) -> &mut [Option<Pcb>] {
        &mut self.slots
    }

    /// Reset the table to its empty state — used by `tcp_reset_all`
    /// in tests.  Cancels outstanding timer tokens before dropping PCBs
    /// so stale timers don't fire into freed slots.
    pub fn clear(&mut self) {
        for (slot, buf) in self.slots.iter_mut().zip(self.buffers.iter_mut()) {
            if let Some(pcb) = slot.as_ref() {
                Self::cancel_pcb_timers(pcb);
            }
            *slot = None;
            *buf = None;
        }
        self.next_ephemeral_port.store(49152, Ordering::Relaxed);
    }

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
}
