//! Ingress pipeline — every packet received from any network device passes
//! through [`net_rx`].
//!
//! It is also where a net test fixture holds the data plane still: the kernel's
//! networking kthreads keep running and keep draining the RX ring, they just
//! find the door locked here. Nothing is frozen, so nothing can time out.

#[cfg(feature = "test-hooks")]
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::klog_debug;

use super::netdev::{DeviceHandle, NetDeviceFeatures};
use super::packetbuf::PacketBuf;
use super::types::{EtherType, MacAddr};
use super::{ETH_HEADER_LEN, arp, ipv4};

/// Nesting count, not a flag: a helper that enters a scope inside a test that
/// already holds one must not reopen the gate when it returns.
#[cfg(feature = "test-hooks")]
static QUIESCE_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Whether a net test fixture is currently holding the data plane still.
#[cfg(feature = "test-hooks")]
#[inline]
pub fn dataplane_quiesced() -> bool {
    QUIESCE_DEPTH.load(Ordering::Relaxed) != 0
}

/// Whether a net test fixture is currently holding the data plane still.
#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
pub fn dataplane_quiesced() -> bool {
    false
}

#[cfg(feature = "test-hooks")]
pub fn quiesce_begin() {
    QUIESCE_DEPTH.fetch_add(1, Ordering::AcqRel);
}

#[cfg(feature = "test-hooks")]
pub fn quiesce_end() {
    let _ = QUIESCE_DEPTH.fetch_update(Ordering::AcqRel, Ordering::Acquire, |d| {
        Some(d.saturating_sub(1))
    });
}

#[cfg(feature = "test-hooks")]
pub fn quiesce_depth() -> u32 {
    QUIESCE_DEPTH.load(Ordering::Relaxed)
}

/// Reopen the gate unconditionally. A test that panicked inside a scope never
/// ran its `Drop`, and a gate left shut kills networking for the rest of the
/// boot.
#[cfg(feature = "test-hooks")]
pub fn quiesce_clear() {
    QUIESCE_DEPTH.store(0, Ordering::Release);
}

/// Called from the NAPI poll loop after [`DeviceHandle::poll_rx`] returns a
/// batch of packets.
///
/// Loopback does not come through here — the NAPI loop hands its packets
/// straight to [`ipv4::handle_rx`] — so quiescing the data plane cuts off the
/// physical NIC without cutting off local traffic.
pub fn net_rx(handle: &DeviceHandle, pkt: PacketBuf) {
    if dataplane_quiesced() {
        return;
    }
    net_rx_inner(handle, pkt)
}

/// [`net_rx`] with the quiesce gate bypassed, for the tests that drive the
/// pipeline with a synthetic frame and a device of their own.
#[cfg(feature = "test-hooks")]
pub fn net_rx_injected(handle: &DeviceHandle, pkt: PacketBuf) {
    net_rx_inner(handle, pkt)
}

fn net_rx_inner(handle: &DeviceHandle, mut pkt: PacketBuf) {
    let frame = pkt.payload();
    if frame.len() < ETH_HEADER_LEN {
        klog_debug!(
            "ingress: frame too short ({} < {})",
            frame.len(),
            ETH_HEADER_LEN
        );
        return;
    }

    let dst_mac = MacAddr([frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]]);
    let ethertype_raw = u16::from_be_bytes([frame[12], frame[13]]);

    // `execute` scopes its own RCU/epoch read guard, so it is released before
    // the TX paths below take any lock.  Bind the verdict to a local so the
    // borrowing `PacketView` is dropped before `pkt` is moved.
    let verdict = crate::xdp::XDP.execute(&mut crate::xdp::PacketView::new(&mut pkt));
    match verdict {
        crate::xdp::XdpAction::Pass => {}
        crate::xdp::XdpAction::Drop => return,
        crate::xdp::XdpAction::Tx => {
            let _ = handle.tx(pkt);
            return;
        }
        crate::xdp::XdpAction::Redirect(dev) => {
            let _ = crate::netdev::DEVICE_REGISTRY.tx_by_index(dev, pkt);
            return;
        }
    }

    let our_mac = handle.mac();
    if dst_mac != our_mac && !dst_mac.is_broadcast() && !dst_mac.is_multicast() {
        return;
    }

    pkt.set_l2(pkt.head());
    pkt.set_l3(pkt.head() + ETH_HEADER_LEN as u16);

    if pkt.pull_header(ETH_HEADER_LEN).is_err() {
        return;
    }

    let dev = handle.index();
    let checksum_rx = handle.features().contains(NetDeviceFeatures::CHECKSUM_RX);

    match EtherType::from_u16(ethertype_raw) {
        Some(EtherType::Arp) => arp::handle_rx(handle, pkt),
        Some(EtherType::Ipv4) => ipv4::handle_rx(dev, pkt, checksum_rx),
        None => {
            klog_debug!(
                "ingress: unknown EtherType 0x{:04x}, dropping",
                ethertype_raw
            );
        }
    }
}
