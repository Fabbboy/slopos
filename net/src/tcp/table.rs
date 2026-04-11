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

use super::pcb::Pcb;
use super::tuple::{TcpError, TcpTuple};

/// Maximum number of simultaneous TCP connections.
///
/// Bumped from the legacy `TcpConnectionTable`'s 64 to 256 post-
/// migration.  Per-connection memory footprint is dominated by the
/// OOO reassembly buffers (~11.7 KB) + send/recv ring buffers
/// (32 KB each by default) ≈ 80 KB per slot.  256 × 80 KB ≈ 20 MB
/// static — still comfortable for SlopOS's ~512 MB QEMU baseline.
/// Halve to 128 if memory budget tightens.
pub const MAX_CONNECTIONS: usize = 256;

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
pub struct PcbTable {
    slots: [Option<Pcb>; MAX_CONNECTIONS],
    next_ephemeral_port: AtomicU16,
}

impl PcbTable {
    /// Create an empty table.  Usable from `const` context.
    pub const fn new() -> Self {
        // `[None; N]` does not work for `Option<Pcb>` because `Pcb`
        // isn't `Copy`; use the const initializer pattern instead.
        const NONE: Option<Pcb> = None;
        Self {
            slots: [NONE; MAX_CONNECTIONS],
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

    /// Install a newly-created PCB into the first free slot.  Returns
    /// the assigned [`ConnId`] or `Err(TableFull)` if every slot is
    /// occupied.
    pub fn install(&mut self, pcb: Pcb) -> Result<ConnId, TcpError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(pcb);
                return Ok(ConnId(i as u32));
            }
        }
        Err(TcpError::TableFull)
    }

    /// Free the slot at `id`, dropping its PCB.  Returns the old
    /// `Pcb` so callers can inspect it (e.g. to cancel outstanding
    /// timers that reference slot-local data) before it is dropped.
    pub fn release(&mut self, id: ConnId) -> Option<Pcb> {
        self.slots.get_mut(id.slot())?.take()
    }

    /// Linear-scan lookup by 4-tuple.  Exact match first, then
    /// LISTEN-socket wildcard match on local port.
    ///
    /// Walks at most `MAX_CONNECTIONS` slots — acceptable for the
    /// single-mutex pre-F.1 table.  F.1 replaces this with a sharded
    /// hash lookup keyed by `tuple`.
    pub fn find(&self, tuple: &TcpTuple) -> Option<ConnId> {
        use super::pcb::PcbState;

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

    /// Reset the table to its empty state — used by `tcp_reset_all`
    /// in tests.  Drops every `Pcb` via `Option::take`.
    pub fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        self.next_ephemeral_port.store(49152, Ordering::Relaxed);
    }
}
