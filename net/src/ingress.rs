//! Ingress pipeline — every packet received from any network device passes
//! through [`net_rx`].

use slopos_ostd::klog_debug;

use super::netdev::{DeviceHandle, NetDeviceFeatures};
use super::packetbuf::PacketBuf;
use super::types::{EtherType, MacAddr};
use super::{ETH_HEADER_LEN, arp, ipv4};

/// Called from the NAPI poll loop after [`DeviceHandle::poll_rx`] returns a
/// batch of packets.
pub fn net_rx(handle: &DeviceHandle, mut pkt: PacketBuf) {
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
