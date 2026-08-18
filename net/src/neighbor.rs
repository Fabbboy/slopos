//! ARP neighbor cache with state machine and timer-driven aging.
//!
//! Public methods collect pending I/O as [`NeighborAction`]s under the lock and
//! return them for the caller to execute unlocked, so `DeviceHandle::tx_lock`
//! never nests under the cache lock.

use core::fmt;
use slopos_ostd::lock_class;

use slopos_ostd::KVec;
use slopos_ostd::klog_debug;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::packetbuf::PacketBuf;
use super::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};
use super::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

const MAX_ENTRIES: usize = 256;

/// Maximum packets queued per `Incomplete` entry before dropping.
const MAX_PENDING_PKTS: usize = 4;

/// Maximum ARP retransmissions before transitioning to `Failed`.
const MAX_RETRIES: u8 = 3;

/// Milliseconds until a `Reachable` entry ages to `Stale`.
pub const REACHABLE_TIME_MS: u64 = 30_000;

/// Milliseconds before re-probing a `Stale` entry that is used.
pub const STALE_PROBE_TIME_MS: u64 = 5_000;

/// Milliseconds between ARP retransmissions for `Incomplete` entries.
pub const RETRANSMIT_TIME_MS: u64 = 1_000;

/// Mirrors the RFC 4861 neighbor unreachability detection states, adapted for
/// ARP over IPv4.
pub enum NeighborState {
    /// ARP request sent; up to [`MAX_PENDING_PKTS`] outgoing packets queue
    /// until the reply arrives.
    Incomplete {
        retries: u8,
        pending: KVec<PacketBuf>,
    },
    Reachable {
        mac: MacAddr,
        confirmed_tick: u64,
    },
    /// Aged past [`REACHABLE_TIME_MS`]; the MAC is still usable but triggers a
    /// re-probe on next use.
    Stale {
        mac: MacAddr,
        last_used_tick: u64,
    },
    /// No ARP reply after [`MAX_RETRIES`] retransmissions; packets for this
    /// address are dropped.
    Failed,
}

impl fmt::Debug for NeighborState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { retries, pending } => write!(
                f,
                "Incomplete(retries={}, pending={})",
                retries,
                pending.len()
            ),
            Self::Reachable { mac, .. } => write!(f, "Reachable({})", mac),
            Self::Stale { mac, .. } => write!(f, "Stale({})", mac),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

pub struct NeighborEntry {
    pub dev: DevIndex,
    pub ip: Ipv4Addr,
    pub state: NeighborState,
    pub timer_token: Option<TimerToken>,
    /// Timer `key` for this entry; assigned once at creation and never reused.
    pub entry_id: u32,
}

impl fmt::Debug for NeighborEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NeighborEntry {{ dev={}, ip={}, state={:?}, id={} }}",
            self.dev, self.ip, self.state, self.entry_id
        )
    }
}

/// One neighbour-cache entry, flattened for enumeration.
#[derive(Clone, Copy)]
pub struct NeighborSnapshot {
    pub dev: DevIndex,
    pub ip: Ipv4Addr,
    /// All-zero while the entry is `Incomplete` or `Failed`.
    pub mac: MacAddr,
    /// `NET_NEIGH_*`.
    pub state: u8,
    pub queued_pkts: u32,
    /// How long ago the entry's MAC was last confirmed, in milliseconds:
    /// `Reachable` measures from the ARP reply, `Stale` from the last use, and
    /// the never-confirmed states report 0.
    pub confirmed_ms_ago: u32,
}

/// I/O for the caller to perform after releasing the cache lock.
pub enum NeighborAction {
    SendArpRequest {
        dev: DevIndex,
        target_ip: Ipv4Addr,
    },
    /// MAC already set in the Ethernet header.
    TransmitPacket {
        pkt: PacketBuf,
    },
    FlushPending {
        packets: KVec<PacketBuf>,
        dst_mac: MacAddr,
        dev: DevIndex,
    },
    None,
}

struct NeighborCacheInner {
    /// Fixed capacity of [`MAX_ENTRIES`].
    entries: KVec<NeighborEntry>,
    next_entry_id: u32,
}

pub struct NeighborCache {
    inner: SpinLock<NeighborCacheInner>,
}

pub static NEIGHBOR_CACHE: NeighborCache = NeighborCache::new();

impl NeighborCache {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(
                NeighborCacheInner {
                    entries: KVec::new(),
                    next_entry_id: 1,
                },
                lock_class!("NEIGHBOR_CACHE", LOCK_LEVEL_REGISTRY),
            ),
        }
    }

    /// Clear all entries and reset the ID generator, so neighbor state cannot
    /// leak between tests.
    pub fn reset(&self) {
        let mut inner = self.inner.lock();
        for entry in inner.entries.iter_mut() {
            if let Some(token) = entry.timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
        }
        inner.entries.clear();
        inner.next_entry_id = 1;
    }

    /// Drop every entry belonging to `dev`, returning the packets that were
    /// queued waiting for resolution.
    ///
    /// The packets come back to the caller because `PacketBuf::drop` takes the
    /// [`PACKET_POOL`] lock, which must not nest under the neighbour cache.
    ///
    /// [`PACKET_POOL`]: crate::pool::PACKET_POOL
    pub fn flush_device(&self, dev: DevIndex) -> KVec<PacketBuf> {
        let mut orphans = KVec::new();
        let mut inner = self.inner.lock();
        let mut i = 0usize;
        while i < inner.entries.len() {
            if inner.entries[i].dev != dev {
                i += 1;
                continue;
            }
            let mut entry = inner.entries.remove(i);
            if let Some(token) = entry.timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }
            if let NeighborState::Incomplete { pending, .. } = &mut entry.state {
                while let Some(pkt) = pending.pop() {
                    // A failed push drops the packet — the same outcome the caller would produce.
                    let _ = orphans.push(pkt);
                }
            }
        }
        orphans
    }

    /// Drop one entry, returning any packets it had queued.
    ///
    /// Same packet-ownership contract as [`flush_device`](Self::flush_device).
    pub fn remove(&self, dev: DevIndex, ip: Ipv4Addr) -> Option<KVec<PacketBuf>> {
        let mut inner = self.inner.lock();
        let pos = inner
            .entries
            .iter()
            .position(|e| e.dev == dev && e.ip == ip)?;
        let mut entry = inner.entries.remove(pos);
        if let Some(token) = entry.timer_token.take() {
            NET_TIMER_WHEEL.cancel(token);
        }
        let mut orphans = KVec::new();
        if let NeighborState::Incomplete { pending, .. } = &mut entry.state {
            while let Some(pkt) = pending.pop() {
                let _ = orphans.push(pkt);
            }
        }
        Some(orphans)
    }

    /// Snapshot the cache into a fresh vector, returning `(entries, total)`.
    ///
    /// Capacity is reserved before the lock is taken: the cache lock never
    /// nests the allocator.
    pub fn snapshot_owned(&self, dev: Option<DevIndex>) -> (KVec<NeighborSnapshot>, usize) {
        let mut out = match KVec::with_capacity(MAX_ENTRIES) {
            Ok(out) => out,
            Err(_) => return (KVec::new(), 0),
        };
        let blank = NeighborSnapshot {
            dev: DevIndex(0),
            ip: Ipv4Addr::UNSPECIFIED,
            mac: MacAddr::ZERO,
            state: slopos_abi::net::NET_NEIGH_INCOMPLETE,
            queued_pkts: 0,
            confirmed_ms_ago: 0,
        };
        for _ in 0..MAX_ENTRIES {
            if out.push(blank).is_err() {
                return (KVec::new(), 0);
            }
        }
        let (written, total) = self.snapshot(dev, out.as_mut_slice());
        out.truncate(written);
        (out, total)
    }

    /// Copy the cache into `out`, returning `(written, total)`.
    pub fn snapshot(&self, dev: Option<DevIndex>, out: &mut [NeighborSnapshot]) -> (usize, usize) {
        // Sampled before the lock: the ages only need to be approximate.
        let now = current_tick_approx();
        let inner = self.inner.lock();
        let mut total = 0usize;
        let mut written = 0usize;
        for entry in inner.entries.iter() {
            if let Some(want) = dev
                && entry.dev != want
            {
                continue;
            }
            total += 1;
            if written >= out.len() {
                continue;
            }
            let (mac, state, queued, since_tick) = match &entry.state {
                NeighborState::Incomplete { pending, .. } => (
                    MacAddr::ZERO,
                    slopos_abi::net::NET_NEIGH_INCOMPLETE,
                    pending.len() as u32,
                    None,
                ),
                NeighborState::Reachable {
                    mac,
                    confirmed_tick,
                } => (
                    *mac,
                    slopos_abi::net::NET_NEIGH_REACHABLE,
                    0,
                    Some(*confirmed_tick),
                ),
                NeighborState::Stale {
                    mac,
                    last_used_tick,
                } => (
                    *mac,
                    slopos_abi::net::NET_NEIGH_STALE,
                    0,
                    Some(*last_used_tick),
                ),
                NeighborState::Failed => {
                    (MacAddr::ZERO, slopos_abi::net::NET_NEIGH_FAILED, 0, None)
                }
            };
            out[written] = NeighborSnapshot {
                dev: entry.dev,
                ip: entry.ip,
                mac,
                state,
                queued_pkts: queued,
                confirmed_ms_ago: since_tick.map_or(0, |t| ticks_to_ms(now.saturating_sub(t))),
            };
            written += 1;
        }
        (written, total)
    }

    /// Look up the MAC address for a neighbor.
    ///
    /// Answers for `Stale` as well as `Reachable`: a stale MAC is still worth
    /// sending to. `Incomplete`, `Failed` and absent all answer `None`.
    pub fn lookup(&self, dev: DevIndex, ip: Ipv4Addr) -> Option<MacAddr> {
        let inner = self.inner.lock();
        inner
            .entries
            .iter()
            .find(|e| e.dev == dev && e.ip == ip)
            .and_then(|e| match &e.state {
                NeighborState::Reachable { mac, .. } => Some(*mac),
                NeighborState::Stale { mac, .. } => Some(*mac),
                _ => None,
            })
    }

    /// Whether a neighbour is currently `Reachable`.
    ///
    /// Stricter than [`lookup`](Self::lookup): a `Stale` entry means nobody has
    /// confirmed the first hop recently, which the connectivity classifier
    /// reports as `Limited`.
    pub fn is_reachable(&self, dev: DevIndex, ip: Ipv4Addr) -> bool {
        let inner = self.inner.lock();
        inner.entries.iter().any(|e| {
            e.dev == dev && e.ip == ip && matches!(e.state, NeighborState::Reachable { .. })
        })
    }

    /// Insert or update a neighbor entry with a confirmed MAC address.
    ///
    /// Called when an ARP reply (or gratuitous ARP) is received. Returns any
    /// packets queued while the entry was `Incomplete`.
    pub fn insert_or_update(
        &self,
        dev: DevIndex,
        ip: Ipv4Addr,
        mac: MacAddr,
        current_tick: u64,
    ) -> NeighborAction {
        let mut inner = self.inner.lock();

        if let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|e| e.dev == dev && e.ip == ip)
        {
            if let Some(token) = entry.timer_token.take() {
                NET_TIMER_WHEEL.cancel(token);
            }

            let pending = if let NeighborState::Incomplete { pending, .. } = &mut entry.state {
                let packets: KVec<PacketBuf> = pending.drain(..).collect();
                if !packets.is_empty() {
                    klog_debug!(
                        "neighbor: flushing {} pending packets for {} on dev {}",
                        packets.len(),
                        ip,
                        dev
                    );
                }
                packets
            } else {
                KVec::new()
            };

            entry.state = NeighborState::Reachable {
                mac,
                confirmed_tick: current_tick,
            };

            let token =
                NET_TIMER_WHEEL.schedule(REACHABLE_TIME_MS, TimerKind::ArpExpire, entry.entry_id);
            entry.timer_token = Some(token);

            if pending.is_empty() {
                NeighborAction::None
            } else {
                NeighborAction::FlushPending {
                    packets: pending,
                    dst_mac: mac,
                    dev,
                }
            }
        } else {
            let entry_id = inner.next_entry_id;
            inner.next_entry_id = inner.next_entry_id.wrapping_add(1);

            if inner.entries.len() >= MAX_ENTRIES {
                Self::evict_one(&mut inner);
            }

            let token = NET_TIMER_WHEEL.schedule(REACHABLE_TIME_MS, TimerKind::ArpExpire, entry_id);

            let _ = inner.entries.push(NeighborEntry {
                dev,
                ip,
                state: NeighborState::Reachable {
                    mac,
                    confirmed_tick: current_tick,
                },
                timer_token: Some(token),
                entry_id,
            });

            klog_debug!(
                "neighbor: new entry {} -> {} on dev {} (id={})",
                ip,
                mac,
                dev,
                entry_id
            );

            NeighborAction::None
        }
    }

    /// Resolve a neighbor's MAC address for packet transmission.
    ///
    /// A `Stale` hit also asks for a re-probe; an absent or `Incomplete` entry
    /// queues `pkt` (up to [`MAX_PENDING_PKTS`]); `Failed` drops it.
    pub fn resolve(&self, dev: DevIndex, ip: Ipv4Addr, pkt: PacketBuf) -> ResolveOutcome {
        let mut inner = self.inner.lock();

        if let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|e| e.dev == dev && e.ip == ip)
        {
            match &mut entry.state {
                NeighborState::Reachable { mac, .. } => {
                    let mac_copy = *mac;
                    ResolveOutcome::Resolved {
                        mac: mac_copy,
                        pkt,
                        action: None,
                    }
                }
                NeighborState::Stale {
                    mac,
                    last_used_tick,
                } => {
                    let mac_copy = *mac;
                    *last_used_tick = current_tick_approx();

                    if let Some(token) = entry.timer_token.take() {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                    let token = NET_TIMER_WHEEL.schedule(
                        STALE_PROBE_TIME_MS,
                        TimerKind::ArpRetransmit,
                        entry.entry_id,
                    );
                    entry.timer_token = Some(token);

                    ResolveOutcome::Resolved {
                        mac: mac_copy,
                        pkt,
                        action: Some(NeighborAction::SendArpRequest { dev, target_ip: ip }),
                    }
                }
                NeighborState::Incomplete { pending, .. } => {
                    if pending.len() < MAX_PENDING_PKTS {
                        let _ = pending.push(pkt);
                    } else {
                        klog_debug!(
                            "neighbor: pending queue full for {} on dev {}, dropping",
                            ip,
                            dev
                        );
                    }
                    ResolveOutcome::Queued
                }
                NeighborState::Failed => {
                    klog_debug!(
                        "neighbor: resolve for {} on dev {} — Failed, dropping",
                        ip,
                        dev
                    );
                    ResolveOutcome::Failed(NetError::HostUnreachable)
                }
            }
        } else {
            let entry_id = inner.next_entry_id;
            inner.next_entry_id = inner.next_entry_id.wrapping_add(1);

            if inner.entries.len() >= MAX_ENTRIES {
                Self::evict_one(&mut inner);
            }

            let token =
                NET_TIMER_WHEEL.schedule(RETRANSMIT_TIME_MS, TimerKind::ArpRetransmit, entry_id);

            let mut pending = KVec::with_capacity(MAX_PENDING_PKTS).expect("neighbor: alloc");
            let _ = pending.push(pkt);

            let _ = inner.entries.push(NeighborEntry {
                dev,
                ip,
                state: NeighborState::Incomplete {
                    retries: 0,
                    pending,
                },
                timer_token: Some(token),
                entry_id,
            });

            klog_debug!(
                "neighbor: new Incomplete entry for {} on dev {} (id={})",
                ip,
                dev,
                entry_id
            );

            ResolveOutcome::ArpNeeded(NeighborAction::SendArpRequest { dev, target_ip: ip })
        }
    }

    /// Timer callback for [`ArpExpire`](TimerKind::ArpExpire): `Reachable` →
    /// `Stale`.
    pub fn on_expire(&self, entry_id: u32) {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner.entries.iter_mut().find(|e| e.entry_id == entry_id) {
            if let NeighborState::Reachable { mac, .. } = entry.state {
                klog_debug!(
                    "neighbor: entry {} ({}) on dev {} expired, Reachable -> Stale",
                    entry_id,
                    entry.ip,
                    entry.dev
                );
                entry.state = NeighborState::Stale {
                    mac,
                    last_used_tick: current_tick_approx(),
                };
                entry.timer_token = None;
            }
            // Any other state: updated between scheduling and firing — ignore.
        }
    }

    /// Timer callback for [`ArpRetransmit`](TimerKind::ArpRetransmit): retry
    /// ARP for `Incomplete`, or transition to `Failed`.
    pub fn on_retransmit(&self, entry_id: u32) -> (Option<NeighborAction>, KVec<PacketBuf>) {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.iter_mut().find(|e| e.entry_id == entry_id) else {
            return (None, KVec::new());
        };

        match &mut entry.state {
            NeighborState::Incomplete { retries, pending } => {
                if *retries < MAX_RETRIES {
                    *retries += 1;
                    let dev = entry.dev;
                    let ip = entry.ip;
                    let retry_count = *retries;

                    let token = NET_TIMER_WHEEL.schedule(
                        RETRANSMIT_TIME_MS,
                        TimerKind::ArpRetransmit,
                        entry_id,
                    );
                    entry.timer_token = Some(token);

                    klog_debug!(
                        "neighbor: retransmit {} for {} on dev {} (retry {}/{})",
                        entry_id,
                        ip,
                        dev,
                        retry_count,
                        MAX_RETRIES
                    );

                    (
                        Some(NeighborAction::SendArpRequest { dev, target_ip: ip }),
                        KVec::new(),
                    )
                } else {
                    let dropped: KVec<PacketBuf> = pending.drain(..).collect();
                    let drop_count = dropped.len();
                    let ip = entry.ip;
                    let dev = entry.dev;

                    entry.state = NeighborState::Failed;
                    entry.timer_token = None;

                    klog_debug!(
                        "neighbor: entry {} ({}) on dev {} -> Failed, dropped {} pending packets",
                        entry_id,
                        ip,
                        dev,
                        drop_count
                    );

                    // Returned so the caller can log/count them before they drop.
                    (None, dropped)
                }
            }
            NeighborState::Stale { .. } => {
                // TODO(tech-debt): a stale re-probe drawing no reply never falls back to Incomplete — it should restart resolution.
                let dev = entry.dev;
                let ip = entry.ip;

                klog_debug!("neighbor: stale re-probe for {} on dev {}", ip, dev);

                (
                    Some(NeighborAction::SendArpRequest { dev, target_ip: ip }),
                    KVec::new(),
                )
            }
            _ => {
                // Timer-cancellation race — ignore.
                (None, KVec::new())
            }
        }
    }

    pub fn entry_count(&self) -> usize {
        self.inner.lock().entries.len()
    }

    fn evict_one(inner: &mut NeighborCacheInner) {
        let mut best_idx: Option<usize> = None;
        let mut best_priority = 0u8; // higher = more evictable
        let mut best_age = 0u64;

        for (i, entry) in inner.entries.iter().enumerate() {
            let (priority, age) = match &entry.state {
                NeighborState::Failed => (4, u64::MAX),
                NeighborState::Stale { last_used_tick, .. } => (3, *last_used_tick),
                NeighborState::Reachable { confirmed_tick, .. } => (2, *confirmed_tick),
                NeighborState::Incomplete { .. } => (1, 0),
            };

            if priority > best_priority || (priority == best_priority && age < best_age) {
                best_idx = Some(i);
                best_priority = priority;
                best_age = age;
            }
        }

        if let Some(idx) = best_idx {
            let entry = inner.entries.swap_remove(idx);
            if let Some(token) = entry.timer_token {
                NET_TIMER_WHEEL.cancel(token);
            }
            klog_debug!(
                "neighbor: evicted entry {} ({}) on dev {}",
                entry.entry_id,
                entry.ip,
                entry.dev
            );
        }
    }
}

/// Outcome of [`NeighborCache::resolve`].
pub enum ResolveOutcome {
    /// MAC known — packet returned for the caller to TX.
    Resolved {
        mac: MacAddr,
        pkt: PacketBuf,
        action: Option<NeighborAction>,
    },
    /// Packet queued in an `Incomplete` entry (ARP already in progress).
    Queued,
    /// New `Incomplete` entry created — need to send ARP request.
    ArpNeeded(NeighborAction),
    /// Entry is `Failed` — packet dropped.
    Failed(NetError),
}

fn current_tick_approx() -> u64 {
    slopos_kernel_services::platform::timer_ticks()
}

/// Convert a tick span to milliseconds, saturating at `u32::MAX`; answers 0
/// while the timer frequency is not yet known.
fn ticks_to_ms(ticks: u64) -> u32 {
    let freq = slopos_kernel_services::platform::timer_frequency() as u64;
    if freq == 0 {
        return 0;
    }
    ticks
        .saturating_mul(1000)
        .checked_div(freq)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}
