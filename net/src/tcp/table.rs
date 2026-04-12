//! `PcbTable` — the fixed-size storage for active TCP connections.
//!
//! Replaces the legacy `TcpConnectionTable` in the C.1 atomic cutover.
//! Owns a `[Option<Pcb>; MAX_CONNECTIONS]`, a per-boot ephemeral port
//! allocator, and the slot-allocation / lookup / release machinery the
//! glue layer drives under a single `IrqMutex`.
//!
//! In Phase A the table is **inert**: no production code acquires
//! [`PCB_TABLE`] yet; all real traffic still flows through the legacy
//! `TCP_TABLE: IrqMutex<TcpConnectionTable>` in `tcp/mod.rs`.  The
//! switchover happens in C.1 when `tcp_input` is rewritten to lock
//! `PCB_TABLE` and dispatch through `Pcb::on_segment`.
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
///
/// Each `Pcb` slot is ~75 KB (32 KB send + 32 KB recv + 11.7 KB OOO),
/// so this directly controls static memory usage.  64 × 75 KB ≈ 4.8 MB.
/// Bump to 128/256 once the table moves to lazy/heap allocation (F.1).
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
/// `[IrqMutex<PcbShard>; 32]`; callers continue using [`PCB_TABLE`]
/// as the entry point through a thin shard-router.
pub static PCB_TABLE: IrqMutex<PcbTable> = IrqMutex::new(PcbTable::new(), LOCK_LEVEL_RESOURCE);

/// The table itself.  Holds the slot array + the ephemeral-port
/// counter (moved here from `tcp/mod.rs` in A.4 so the legacy and the
/// new allocator don't race).
/// The table itself.  PCB metadata and buffers are stored in parallel
/// arrays — PCBs are small (~200 bytes, safe to move/drop on stack),
/// while buffers (~75 KB each) live in a separate static pool indexed
/// by slot, matching the architecture of Linux's sk_buff separation
/// and the old `TcpConnectionTable`'s `connections[]` + `buffers[]`.
pub struct PcbTable {
    slots: [Option<Pcb>; MAX_CONNECTIONS],
    pub buffers: [TcpBufferPair; MAX_CONNECTIONS],
    next_ephemeral_port: AtomicU16,
}

impl PcbTable {
    pub const fn new() -> Self {
        const NONE: Option<Pcb> = None;
        const BUF: TcpBufferPair = TcpBufferPair::new();
        Self {
            slots: [NONE; MAX_CONNECTIONS],
            buffers: [BUF; MAX_CONNECTIONS],
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

    /// Borrow a PCB and its associated buffer pool entry together.
    pub fn get_with_bufs(&mut self, id: ConnId) -> Option<(&mut Pcb, &mut TcpBufferPair)> {
        let slot = id.slot();
        let pcb = self.slots.get_mut(slot)?.as_mut()?;
        let bufs = &mut self.buffers[slot];
        Some((pcb, bufs))
    }

    /// Buffer pool entry for a slot (immutable).
    pub fn bufs(&self, id: ConnId) -> &TcpBufferPair {
        &self.buffers[id.slot()]
    }

    /// Buffer pool entry for a slot (mutable).
    pub fn bufs_mut(&mut self, id: ConnId) -> &mut TcpBufferPair {
        &mut self.buffers[id.slot()]
    }

    /// Allocate a free slot, install a PCB, and clear its buffer pool entry.
    /// `Pcb` is small (~200 bytes) so constructing it on the stack is fine.
    /// The ~75 KB buffers live in the parallel `buffers[]` array and are
    /// cleared in-place without touching the stack.
    pub fn install_with(
        &mut self,
        tuple: TcpTuple,
        state: PcbState,
        init: impl FnOnce(&mut Pcb),
    ) -> Result<ConnId, TcpError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                let pcb = slot.insert(Pcb::new(tuple, state));
                self.buffers[i].clear();
                init(pcb);
                return Ok(ConnId(i as u32));
            }
        }
        Err(TcpError::TableFull)
    }

    /// Free the slot at `id`. Cancels outstanding timers, clears
    /// buffers, and drops the small PCB metadata.
    pub fn release(&mut self, id: ConnId) {
        let slot = id.slot();
        if let Some(s) = self.slots.get_mut(slot) {
            if let Some(pcb) = s.as_ref() {
                Self::cancel_pcb_timers(pcb);
            }
            *s = None;
            self.buffers[slot].clear();
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
    /// (RFC 6335).  Wraps on overflow back to 49152.
    pub fn alloc_ephemeral_port(&self) -> u16 {
        loop {
            let port = self.next_ephemeral_port.fetch_add(1, Ordering::Relaxed);
            if port >= 49152 {
                return port;
            }
            self.next_ephemeral_port.store(49152, Ordering::Relaxed);
        }
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

    /// Iterate over all live PCBs together with their buffer pool
    /// entries.  Splits the borrow between `slots` and `buffers` so
    /// both can be mutated in the loop body.
    pub fn iter_mut_with_bufs(
        &mut self,
    ) -> impl Iterator<Item = (ConnId, &mut Pcb, &mut TcpBufferPair)> {
        self.slots
            .iter_mut()
            .zip(self.buffers.iter_mut())
            .enumerate()
            .filter_map(|(i, (slot, bufs))| slot.as_mut().map(|pcb| (ConnId(i as u32), pcb, bufs)))
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
        for slot in self.slots.iter_mut() {
            if let Some(pcb) = slot.as_ref() {
                Self::cancel_pcb_timers(pcb);
            }
            *slot = None;
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
            }
            PcbState::TimeWait(tw) => {
                if let Some(token) = tw.expire_token {
                    NET_TIMER_WHEEL.cancel(token);
                }
            }
        }
    }
}
