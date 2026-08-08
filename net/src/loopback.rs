//! Loopback network device (`lo`).
//!
//! # Architecture
//!
//! The loopback device implements [`NetDevice`] with a trivial internal queue:
//! `tx()` pushes packets onto a [`VecDeque`], `poll_rx()` drains them back out.
//! Packets transmitted on loopback are delivered to the local ingress pipeline
//! on the next NAPI poll — no wire, no checksums, no ARP.
//!
//! The loopback device is registered at `DevIndex(0)` by convention, before
//! any physical NIC.  It is configured with `127.0.0.1/8` and a connected
//! route for `127.0.0.0/8`.
//!
//! # Concurrency
//!
//! The internal queue is protected by an [`SpinLock`] since both `tx()` (from
//! any socket context) and `poll_rx()` (from the NAPI loop) access it.

use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec, KVecDeque};

use super::netdev::{NetDevice, NetDeviceFeatures, NetDeviceStats};
use super::packetbuf::PacketBuf;
use super::pool::PacketPool;
use super::types::{MacAddr, NetError};

/// Maximum number of packets queued in the loopback device.
const LOOPBACK_QUEUE_CAPACITY: usize = 256;

/// Inner state of the loopback device, behind [`SpinLock`].
struct LoopbackInner {
    /// Packets waiting to be "received" by the ingress pipeline.
    queue: KVecDeque<PacketBuf>,
    /// Cumulative statistics.
    stats: NetDeviceStats,
    /// Cleared by `set_down`. Shares the lock with `queue` so a send racing
    /// retirement either lands before the drain and is cleared by it, or
    /// observes the device down and is rejected — never left queued on a
    /// device nothing will poll again.
    up: bool,
}

/// The loopback network device (`lo`).
///
/// Registered at `DevIndex(0)` during kernel init.  All packets sent to
/// `127.0.0.0/8` are routed here and immediately available for local delivery.
pub struct LoopbackDev {
    inner: SpinLock<LoopbackInner>,
}

// SAFETY: All mutable state is behind SpinLock.

impl LoopbackDev {
    /// Create a new loopback device with an empty queue.
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(
                LoopbackInner {
                    queue: KVecDeque::with_capacity(64).expect("loopback: alloc"),
                    stats: NetDeviceStats::new(),
                    up: true,
                },
                lock_class!("LoopbackDev.inner", LOCK_LEVEL_RESOURCE),
            ),
        }
    }
}

impl NetDevice for LoopbackDev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        {
            let mut inner = self.inner.lock();
            if !inner.up {
                inner.stats.tx_dropped += 1;
                return Err(NetError::NetworkUnreachable);
            }
            if inner.queue.len() >= LOOPBACK_QUEUE_CAPACITY {
                inner.stats.tx_dropped += 1;
                return Err(NetError::NoBufferSpace);
            }
            let len = pkt.len();
            let _ = inner.queue.push_back(pkt);
            inner.stats.tx_packets += 1;
            inner.stats.tx_bytes += len as u64;
        }
        // Wake the netpoll kthread so it drains the loopback queue
        // back through `poll_loopback` → `ingress::net_rx`. Required
        // because the loopback device has no IRQ — without this
        // wake, packets sent to 127.0.0.0/8 would sit in the queue
        // until some unrelated NIC RX IRQ wakes the kthread.
        // Safe no-op when no driver has registered (loopback-only
        // boot fixtures, kernel-side tests pre-virtio-net probe).
        crate::napi::wake_napi();
        Ok(())
    }

    fn poll_rx(&self, budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        let mut inner = self.inner.lock();
        if !inner.up {
            return KVec::new();
        }
        let count = budget.min(inner.queue.len());
        let mut packets = KVec::with_capacity(count).unwrap_or_else(|_| KVec::new());
        for _ in 0..count {
            if let Some(pkt) = inner.queue.pop_front() {
                inner.stats.rx_packets += 1;
                inner.stats.rx_bytes += pkt.len() as u64;
                let _ = packets.push(pkt);
            }
        }
        packets
    }

    fn set_up(&self) {
        self.inner.lock().up = true;
    }

    fn set_down(&self) {
        let mut inner = self.inner.lock();
        inner.up = false;
        inner.queue.clear();
    }

    fn mtu(&self) -> u16 {
        65535
    }

    fn mac(&self) -> MacAddr {
        MacAddr::ZERO
    }

    fn stats(&self) -> NetDeviceStats {
        self.inner.lock().stats
    }

    fn features(&self) -> NetDeviceFeatures {
        // Loopback never needs checksum computation — packets stay in memory.
        NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX
    }

    fn kind(&self) -> crate::iface::IfaceKind {
        crate::iface::IfaceKind::Loopback
    }

    /// Loopback has no lower layer, so its carrier is a constant rather than an
    /// assumption — reporting it as *detected* is the honest answer.
    fn carrier(&self) -> bool {
        true
    }

    fn carrier_detect(&self) -> bool {
        true
    }
}

// =============================================================================
// 3C.2 — Loopback registration
// =============================================================================

use slopos_ostd::klog_info;

/// Register the loopback device in the global device registry, give it an
/// interface, and configure its IPv4 address and route.
///
/// **Must be called before any physical NIC registration** so that loopback
/// gets `DevIndex(0)` by convention.
///
/// Sets up:
/// - The `lo` interface
/// - `127.0.0.1/8` on it, with host scope
/// - Connected route `127.0.0.0/8 → DevIndex(0)`
pub fn init_loopback() {
    use super::iface::{self, AddrOrigin, AddrScope, IfaceAddr, IfaceKind};
    use super::netdev::DEVICE_REGISTRY;
    use super::route::{ROUTE_TABLE, RouteEntry};
    use super::types::Ipv4Addr;

    let dev: KArc<dyn NetDevice + Send + Sync> = match KArc::try_new(LoopbackDev::new()) {
        Ok(d) => d,
        Err(_) => {
            klog_info!("loopback: alloc failed");
            return;
        }
    };
    let Some(handle) = DEVICE_REGISTRY.register(dev) else {
        klog_info!("loopback: failed to register in device registry");
        return;
    };

    let lo_index = handle.index();
    klog_info!("loopback: registered as dev {}", lo_index);

    // Attach the interface *after* registration returns, never from inside it:
    // an administrative down takes the interface table and then the registry,
    // so creating the row under the registry lock would close a lock cycle.
    let ifindex = match iface::attach(
        lo_index,
        IfaceKind::Loopback,
        super::types::MacAddr::ZERO,
        65535,
        true,
        true,
    ) {
        Ok(idx) => idx,
        Err(err) => {
            klog_info!("loopback: failed to attach interface: {:?}", err);
            return;
        }
    };

    // 127.0.0.1/8, host scope: the address is meaningful only to this machine,
    // which is also why `first_ipv4` skips it when picking a source address.
    if let Err(err) = iface::add_addr(
        ifindex,
        IfaceAddr::permanent(Ipv4Addr::LOCALHOST, 8, AddrScope::Host, AddrOrigin::Static),
    ) {
        klog_info!("loopback: failed to assign 127.0.0.1/8: {:?}", err);
    }

    // The connected route, added with the interface-table lock already dropped
    // (`add_addr` released it) — the route table is never taken while holding
    // the interface table.
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr::from_bytes([127, 0, 0, 0]),
        prefix_len: 8,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: lo_index,
        metric: 0,
    });

    if let Some((dev, _next_hop)) = ROUTE_TABLE.lookup(Ipv4Addr::LOCALHOST) {
        klog_info!("loopback: route 127.0.0.0/8 -> dev {} confirmed", dev);
    } else {
        klog_info!("loopback: WARNING — no route for 127.0.0.1");
    }
}
