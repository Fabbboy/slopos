//! Network-state monitors: the rings behind a `net_monitor` fd.
//!
//! [`netmon_post`] publishes a configuration change; the consumer side is a
//! pollable fd whose `read` drains whole [`NetEvent`] records (see
//! [`crate::netmon_file_ops`]). Each monitor owns a fixed
//! [`NETMON_RING_CAP`]-entry ring in `.bss`; nothing here allocates.
//!
//! Records are stamped from [`crate::netseq`], which advances whether or not
//! anybody is subscribed: a client joins the stream by discarding drained
//! records with `seq <= hdr.seq` from its `net_query` snapshot, and a hole in
//! the numbering would be indistinguishable from a lost record.

use slopos_abi::Errno;
use slopos_abi::event::{KernelEvent, MAX_NETMON, NetMonSlot};
use slopos_abi::net::{
    NET_EV_ADDR_ADDED, NET_EV_ADDR_REMOVED, NET_EV_CONNECTIVITY, NET_EV_DHCP, NET_EV_GLOBAL_ENABLE,
    NET_EV_IFACE_ADDED, NET_EV_IFACE_CHANGED, NET_EV_IFACE_REMOVED, NET_EV_NEIGH_CHANGED,
    NET_EV_OVERFLOW, NET_EV_RESOLVER, NET_EV_ROUTE_ADDED, NET_EV_ROUTE_REMOVED, NET_IFINDEX_GLOBAL,
    NET_MON_ADDR, NET_MON_CONN, NET_MON_DHCP, NET_MON_GLOBAL, NET_MON_IFACE, NET_MON_NEIGH,
    NET_MON_RESOLV, NET_MON_ROUTE, NetEvent,
};
use slopos_fs::fileio::FdTable;
use slopos_ostd::handle::Handle;
use slopos_ostd::lock_class;
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::netseq::next_seq;

/// Records one monitor buffers before it starts dropping. Sized for the burst
/// a single DHCP transaction produces, with room to spare.
pub const NETMON_RING_CAP: usize = 64;

/// Monitors one process may hold open at once. Without a per-process quota the
/// fixed [`MAX_NETMON`]-slot registry is an exhaustion denial of service from
/// any unprivileged process; two covers the shapes that exist — an indicator
/// and a configuration tool.
pub const NETMON_MAX_PER_PROCESS: usize = 2;

/// Slot-index bit width in the packed fd handle; the rest holds the generation.
const SLOT_BITS: u32 = 4;

const _: () = assert!(MAX_NETMON <= (1usize << SLOT_BITS));

/// Which subscription bit selects `kind`.
///
/// [`NET_EV_OVERFLOW`] deliberately maps to no bit: it is synthesised by the
/// ring that dropped a record and reaches its subscriber whatever that
/// subscriber asked for.
#[inline]
pub const fn mask_bit_for_kind(kind: u16) -> u32 {
    match kind {
        NET_EV_IFACE_ADDED | NET_EV_IFACE_REMOVED | NET_EV_IFACE_CHANGED => NET_MON_IFACE,
        NET_EV_ADDR_ADDED | NET_EV_ADDR_REMOVED => NET_MON_ADDR,
        NET_EV_ROUTE_ADDED | NET_EV_ROUTE_REMOVED => NET_MON_ROUTE,
        NET_EV_RESOLVER => NET_MON_RESOLV,
        NET_EV_CONNECTIVITY => NET_MON_CONN,
        NET_EV_DHCP => NET_MON_DHCP,
        NET_EV_GLOBAL_ENABLE => NET_MON_GLOBAL,
        NET_EV_NEIGH_CHANGED => NET_MON_NEIGH,
        _ => 0,
    }
}

/// One subscriber's ring plus the state that describes it.
///
/// A dead monitor keeps its slot and its generation; `live` distinguishes the
/// two, and the generation makes a handle minted before the slot was recycled
/// resolve to a typed miss. Deliberately neither `Copy` nor `Clone`: at two
/// kilobytes, an accidental by-value copy must not be spellable under the
/// stack-frame gate.
struct Monitor {
    live: bool,
    generation: u64,
    /// The key the per-process cap counts against. An [`FdTable`] rather than
    /// a raw pid because the cap must not be inherited: a recycled id would
    /// charge the next process with its predecessor's outstanding monitors.
    owner: Option<FdTable>,
    mask: u32,
    ring: [NetEvent; NETMON_RING_CAP],
    head: usize,
    len: usize,
    /// Records dropped and not yet reported. Non-zero **is** the open overflow
    /// episode; there is no separate latch.
    dropped: u32,
    /// Sequence of the first record dropped in the open episode.
    overflow_seq: u64,
}

/// Generations start at 1, so a packed handle of 0 can never name a live
/// monitor whatever slot it claims.
const FREE_MONITOR: Monitor = Monitor {
    live: false,
    generation: 1,
    owner: None,
    mask: 0,
    ring: [NetEvent::new(0, 0, 0, [0u8; 16]); NETMON_RING_CAP],
    head: 0,
    len: 0,
    dropped: 0,
    overflow_seq: 0,
};

impl Monitor {
    /// Whether a `read` would return anything — the poll condition.
    #[inline]
    const fn readable(&self) -> bool {
        self.len != 0 || self.dropped != 0
    }

    /// Append `ev`, dropping it and opening (or extending) an overflow episode
    /// when the ring is full.
    ///
    /// Drop-newest rather than overwrite-oldest: a queued record's loss is
    /// unrecoverable to a reader that has not yet seen it, while the newest
    /// record's information is still in the live state a re-snapshot reads.
    fn push(&mut self, ev: &NetEvent) {
        if self.len == NETMON_RING_CAP {
            if self.dropped == 0 {
                self.overflow_seq = ev.seq;
            }
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.ring[(self.head + self.len) % NETMON_RING_CAP] = *ev;
        self.len += 1;
    }

    /// The record a `read` would deliver next, without consuming it.
    ///
    /// The overflow marker is synthesised here rather than stored, so it can be
    /// reported ahead of records the ring queued before the drop without
    /// occupying an entry of its own.
    fn front(&self) -> Option<NetEvent> {
        if self.dropped != 0 {
            return Some(NetEvent::new(
                self.overflow_seq,
                NET_EV_OVERFLOW,
                NET_IFINDEX_GLOBAL,
                NetEvent::u32_payload(self.dropped),
            ));
        }
        if self.len == 0 {
            return None;
        }
        Some(self.ring[self.head])
    }

    /// Consume what [`front`](Self::front) handed out, once it has been
    /// delivered.
    ///
    /// Peek-copy-commit is what lets a `read` copy to a user buffer without
    /// holding the table lock. A record retires only if it is still at the
    /// head, and the marker retires only the drops it actually reported, so a
    /// drop landing between the peek and the retire leaves a smaller episode
    /// open rather than vanishing from the count.
    fn commit(&mut self, ev: &NetEvent) {
        if ev.kind == NET_EV_OVERFLOW {
            if self.dropped != 0 && self.overflow_seq == ev.seq {
                self.dropped = self.dropped.saturating_sub(ev.as_u32());
                if self.dropped == 0 {
                    self.overflow_seq = 0;
                }
            }
            return;
        }
        if self.len == 0 || self.ring[self.head].seq != ev.seq {
            return;
        }
        self.head = (self.head + 1) % NETMON_RING_CAP;
        self.len -= 1;
    }
}

struct NetMonTableInner {
    slots: [Monitor; MAX_NETMON],
}

/// What a registry is currently holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetMonStats {
    /// Monitors currently open.
    pub live: usize,
    /// Records queued across all of them.
    pub queued: usize,
    /// Records dropped and not yet reported, across all of them.
    pub dropped: u32,
}

/// A registry of network-state monitors.
///
/// One instance is the kernel's ([`NETMON_TABLE`]); the type is public so a
/// test can drive a scratch registry without touching the live one.
pub struct NetMonTable {
    inner: SpinLock<NetMonTableInner>,
}

/// The kernel's monitor registry.
pub static NETMON_TABLE: NetMonTable = NetMonTable::new(lock_class!("NETMON", LOCK_LEVEL_RESOURCE));

impl NetMonTable {
    /// An empty registry. The lock class comes from the caller so a scratch
    /// registry is a genuinely different lock to the validator.
    pub const fn new(class: &'static slopos_ostd::sync::lock_tracking::LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                NetMonTableInner {
                    slots: [const { FREE_MONITOR }; MAX_NETMON],
                },
                class,
            ),
        }
    }

    /// Open a monitor for `owner` subscribed to `mask`, returning the
    /// packed handle an fd stores.
    ///
    /// * `EINVAL` — an empty mask, which would produce an fd that can never
    ///   become ready.
    /// * `EMFILE` — this process already holds [`NETMON_MAX_PER_PROCESS`].
    /// * `ENOMEM` — every registry slot is taken.
    pub fn open(&self, owner: FdTable, mask: u32) -> Result<usize, Errno> {
        if mask == 0 {
            return Err(Errno::EINVAL);
        }
        let mut table = self.inner.lock();

        let held = table
            .slots
            .iter()
            .filter(|m| m.live && m.owner == Some(owner))
            .count();
        if held >= NETMON_MAX_PER_PROCESS {
            return Err(Errno::EMFILE);
        }

        let (slot, monitor) = table
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, m)| !m.live)
            .ok_or(Errno::ENOMEM)?;

        monitor.live = true;
        monitor.owner = Some(owner);
        monitor.mask = mask;
        monitor.head = 0;
        monitor.len = 0;
        monitor.overflow_seq = 0;
        monitor.dropped = 0;

        Ok(Handle::<Monitor>::from_parts(slot as u32, monitor.generation).pack(SLOT_BITS))
    }

    /// Release a monitor. Called from the owning fd's backing `Drop`, so the
    /// last fd close **is** the teardown. A stale handle is a no-op, which
    /// makes a double release unrepresentable.
    pub fn close(&self, raw_handle: usize) {
        let handle = Handle::<Monitor>::unpack(raw_handle, SLOT_BITS);
        let mut table = self.inner.lock();
        let Some(monitor) = table.slots.get_mut(handle.slot() as usize) else {
            return;
        };
        if !monitor.live || monitor.generation != handle.generation() {
            return;
        }
        monitor.live = false;
        monitor.generation = monitor.generation.wrapping_add(1);
        monitor.mask = 0;
        monitor.owner = None;
        monitor.head = 0;
        monitor.len = 0;
        monitor.overflow_seq = 0;
        monitor.dropped = 0;
    }

    fn with_monitor<R>(
        &self,
        raw_handle: usize,
        f: impl FnOnce(&mut Monitor) -> R,
    ) -> Result<R, Errno> {
        let handle = Handle::<Monitor>::unpack(raw_handle, SLOT_BITS);
        let mut table = self.inner.lock();
        let monitor = table
            .slots
            .get_mut(handle.slot() as usize)
            .ok_or(Errno::EBADF)?;
        if !monitor.live || monitor.generation != handle.generation() {
            return Err(Errno::EBADF);
        }
        Ok(f(monitor))
    }

    /// Publish a state change to every monitor subscribed to its kind,
    /// returning the sequence it was given.
    ///
    /// Woken slots are collected into a stack array and published after the
    /// table lock is released, so this has no outgoing lock edge and stays
    /// callable from a hard IRQ and from under a caller's cli-spinlock.
    pub fn post(&self, kind: u16, ifindex: u32, payload: [u8; 16]) -> u64 {
        let bit = mask_bit_for_kind(kind);
        let seq = next_seq();
        let event = NetEvent::new(seq, kind, ifindex, payload);

        let mut woken = [0u32; MAX_NETMON];
        let mut n_woken = 0usize;
        {
            let mut table = self.inner.lock();
            for (slot, monitor) in table.slots.iter_mut().enumerate() {
                if !monitor.live || (monitor.mask & bit) == 0 {
                    continue;
                }
                let was_readable = monitor.readable();
                monitor.push(&event);
                if !was_readable && monitor.readable() {
                    woken[n_woken] = slot as u32;
                    n_woken += 1;
                }
            }
        }
        for &slot in &woken[..n_woken] {
            BUS.publish(KernelEvent::NetMonitor {
                mon: NetMonSlot(slot),
            });
        }
        seq
    }

    pub fn peek(&self, raw_handle: usize) -> Result<Option<NetEvent>, Errno> {
        self.with_monitor(raw_handle, |m| m.front())
    }

    /// Consume the record [`peek`](Self::peek) returned. Ignored if the
    /// monitor has moved on.
    pub fn commit(&self, raw_handle: usize, event: &NetEvent) -> Result<(), Errno> {
        self.with_monitor(raw_handle, |m| m.commit(event))
    }

    pub fn drain(&self, raw_handle: usize, out: &mut [NetEvent]) -> Result<usize, Errno> {
        let mut written = 0usize;
        while written < out.len() {
            let Some(event) = self.peek(raw_handle)? else {
                break;
            };
            out[written] = event;
            self.commit(raw_handle, &event)?;
            written += 1;
        }
        Ok(written)
    }

    pub fn is_readable(&self, raw_handle: usize) -> bool {
        self.with_monitor(raw_handle, |m| m.readable())
            .unwrap_or(false)
    }

    /// The registry slot a live handle names — the wait-queue key its pollers
    /// register on. `None` once the monitor is gone.
    pub fn slot_of(&self, raw_handle: usize) -> Option<NetMonSlot> {
        let handle = Handle::<Monitor>::unpack(raw_handle, SLOT_BITS);
        self.with_monitor(raw_handle, |_| NetMonSlot(handle.slot()))
            .ok()
    }

    pub fn count(&self) -> usize {
        let table = self.inner.lock();
        table.slots.iter().filter(|m| m.live).count()
    }

    /// A summary of what the registry is holding, for the diagnostic console.
    /// A non-zero `dropped` names a subscriber that is not keeping up, which is
    /// invisible from anywhere else.
    pub fn stats(&self) -> NetMonStats {
        let table = self.inner.lock();
        let mut stats = NetMonStats::default();
        for monitor in table.slots.iter().filter(|m| m.live) {
            stats.live += 1;
            stats.queued += monitor.len;
            stats.dropped = stats.dropped.saturating_add(monitor.dropped);
        }
        stats
    }

    /// Release every monitor. Test-only: production tears a monitor down
    /// through its fd, and closing one out from under its owner would leave
    /// that owner polling an fd that can never be ready again.
    #[cfg(feature = "test-hooks")]
    pub fn clear(&self) {
        let mut table = self.inner.lock();
        for monitor in table.slots.iter_mut() {
            if monitor.live {
                monitor.generation = monitor.generation.wrapping_add(1);
            }
            monitor.live = false;
            monitor.mask = 0;
            monitor.owner = None;
            monitor.head = 0;
            monitor.len = 0;
            monitor.overflow_seq = 0;
            monitor.dropped = 0;
        }
    }
}

/// Publish a state change to the kernel's monitors. See [`NetMonTable::post`].
///
/// Build `payload` with the [`NetEvent`] encoder for the kind being posted so
/// kernel and userland share one decoder.
#[inline]
pub fn netmon_post(kind: u16, ifindex: u32, payload: [u8; 16]) -> u64 {
    NETMON_TABLE.post(kind, ifindex, payload)
}

/// Open a monitor in the kernel registry. See [`NetMonTable::open`].
#[inline]
pub fn netmon_open(owner: FdTable, mask: u32) -> Result<usize, Errno> {
    NETMON_TABLE.open(owner, mask)
}

/// Release a monitor from the kernel registry. See [`NetMonTable::close`].
#[inline]
pub fn netmon_close(raw_handle: usize) {
    NETMON_TABLE.close(raw_handle)
}
