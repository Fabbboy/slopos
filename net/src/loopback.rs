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
//! The internal queue is protected by an [`IrqMutex`] since both `tx()` (from
//! any socket context) and `poll_rx()` (from the NAPI loop) access it.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use slopos_sync::IrqMutex;

use super::netdev::{NetDevice, NetDeviceFeatures, NetDeviceStats};
use super::packetbuf::PacketBuf;
use super::pool::PacketPool;
use super::types::{MacAddr, NetError};

/// Maximum number of packets queued in the loopback device.
const LOOPBACK_QUEUE_CAPACITY: usize = 256;

/// Inner state of the loopback device, behind [`IrqMutex`].
struct LoopbackInner {
    /// Packets waiting to be "received" by the ingress pipeline.
    queue: VecDeque<PacketBuf>,
    /// Cumulative statistics.
    stats: NetDeviceStats,
}

/// The loopback network device (`lo`).
///
/// Registered at `DevIndex(0)` during kernel init.  All packets sent to
/// `127.0.0.0/8` are routed here and immediately available for local delivery.
pub struct LoopbackDev {
    inner: IrqMutex<LoopbackInner>,
}

// SAFETY: All mutable state is behind IrqMutex.
unsafe impl Send for LoopbackDev {}
unsafe impl Sync for LoopbackDev {}

impl LoopbackDev {
    /// Create a new loopback device with an empty queue.
    pub fn new() -> Self {
        Self {
            inner: IrqMutex::new(LoopbackInner {
                queue: VecDeque::with_capacity(64),
                stats: NetDeviceStats::new(),
            }),
        }
    }
}

impl NetDevice for LoopbackDev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let mut inner = self.inner.lock();
        if inner.queue.len() >= LOOPBACK_QUEUE_CAPACITY {
            inner.stats.tx_dropped += 1;
            return Err(NetError::NoBufferSpace);
        }
        let len = pkt.len();
        inner.queue.push_back(pkt);
        inner.stats.tx_packets += 1;
        inner.stats.tx_bytes += len as u64;
        Ok(())
    }

    fn poll_rx(&self, budget: usize, _pool: &'static PacketPool) -> Vec<PacketBuf> {
        let mut inner = self.inner.lock();
        let count = budget.min(inner.queue.len());
        let mut packets = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(pkt) = inner.queue.pop_front() {
                inner.stats.rx_packets += 1;
                inner.stats.rx_bytes += pkt.len() as u64;
                packets.push(pkt);
            }
        }
        packets
    }

    fn set_up(&self) {
        // Loopback is always up — nothing to do.
    }

    fn set_down(&self) {
        let mut inner = self.inner.lock();
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
}

// =============================================================================
// 3C.2 — Loopback registration
// =============================================================================

use alloc::boxed::Box;
use slopos_utils::klog_info;

/// Register the loopback device in the global device registry and configure
/// its IPv4 address and route.
///
/// **Must be called before any physical NIC registration** so that loopback
/// gets `DevIndex(0)` by convention.
///
/// Sets up:
/// - `127.0.0.1/8` on the loopback interface
/// - Connected route `127.0.0.0/8 → DevIndex(0)`
pub fn init_loopback() {
    use super::netdev::DEVICE_REGISTRY;
    use super::netstack::NET_STACK;
    use super::route::ROUTE_TABLE;
    use super::types::Ipv4Addr;

    let dev = Box::new(LoopbackDev::new());
    let Some(handle) = DEVICE_REGISTRY.register(dev) else {
        klog_info!("loopback: failed to register in device registry");
        return;
    };

    let lo_index = handle.index();
    klog_info!("loopback: registered as dev {}", lo_index);

    // Configure 127.0.0.1/8 on the loopback interface.
    // Use configure() which also adds routes, but loopback's "gateway" is
    // UNSPECIFIED (no default route through loopback).
    NET_STACK.configure(
        lo_index,
        Ipv4Addr::LOCALHOST,                            // 127.0.0.1
        Ipv4Addr::from_bytes([255, 0, 0, 0]),           // /8 netmask
        Ipv4Addr::UNSPECIFIED,                          // no gateway
        [Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED], // no DNS
    );

    // Verify the loopback route was added by configure().
    if let Some((dev, _next_hop)) = ROUTE_TABLE.lookup(Ipv4Addr::LOCALHOST) {
        klog_info!("loopback: route 127.0.0.0/8 -> dev {} confirmed", dev);
    } else {
        klog_info!("loopback: WARNING — no route for 127.0.0.1");
    }
}
