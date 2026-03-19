//! Ingress pipeline — single entry point for all received network packets.
//!
//! Every packet received from any network device passes through [`net_rx`],
//! which parses the Ethernet header, filters by destination MAC, and dispatches
//! to the appropriate protocol handler (ARP, IPv4).
//!
//! This module replaces the inline `dispatch_rx_frame()` that was previously
//! embedded in the VirtIO-net driver, establishing a clean driver–stack boundary.

use slopos_lib::klog_debug;

use super::netdev::{DeviceHandle, NetDeviceFeatures};
use super::packetbuf::PacketBuf;
use super::types::{EtherType, MacAddr};
use super::{ETH_HEADER_LEN, arp, ipv4};

/// Process a received packet through the ingress pipeline.
///
/// This is the **single entry point** for all received packets, called from the
/// NAPI poll loop after [`DeviceHandle::poll_rx`] returns a batch of packets.
///
/// # Processing steps
///
/// 1. Validate minimum Ethernet frame length
/// 2. Parse destination MAC and EtherType from the Ethernet header
/// 3. Filter: accept only packets addressed to our MAC, broadcast, or multicast
/// 4. Set L2/L3 layer offsets on the [`PacketBuf`]
/// 5. Pull the Ethernet header (advance `head` past 14 bytes)
/// 6. Dispatch by EtherType: ARP → [`arp::handle_rx`], IPv4 → [`ipv4::handle_rx`]
///
/// Unknown EtherTypes are silently dropped (no panic).
pub fn net_rx(handle: &DeviceHandle, mut pkt: PacketBuf) {
    // Minimum Ethernet frame: 14 bytes header.
    let frame = pkt.payload();
    if frame.len() < ETH_HEADER_LEN {
        klog_debug!(
            "ingress: frame too short ({} < {})",
            frame.len(),
            ETH_HEADER_LEN
        );
        return;
    }

    // Parse destination MAC and EtherType.
    let dst_mac = MacAddr([frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]]);
    let ethertype_raw = u16::from_be_bytes([frame[12], frame[13]]);

    // Destination MAC filter: accept our MAC, broadcast, or multicast.
    let our_mac = handle.mac();
    if dst_mac != our_mac && !dst_mac.is_broadcast() && !dst_mac.is_multicast() {
        // Not addressed to us — silently drop.
        return;
    }

    // Set layer offsets (absolute positions in the backing buffer).
    // L2 starts at the current head (position 0 for RX-path packets).
    pkt.set_l2(pkt.head());
    // L3 starts right after the Ethernet header.
    pkt.set_l3(pkt.head() + ETH_HEADER_LEN as u16);

    // Pull the Ethernet header so payload() now points at L3 data.
    if pkt.pull_header(ETH_HEADER_LEN).is_err() {
        return;
    }

    let dev = handle.index();
    let checksum_rx = handle.features().contains(NetDeviceFeatures::CHECKSUM_RX);

    // Dispatch by EtherType.
    match EtherType::from_u16(ethertype_raw) {
        Some(EtherType::Arp) => arp::handle_rx(handle, pkt),
        Some(EtherType::Ipv4) => ipv4::handle_rx(dev, pkt, checksum_rx),
        Some(EtherType::Ipv6) => {
            // IPv6 not supported yet — silently drop.
        }
        None => {
            klog_debug!(
                "ingress: unknown EtherType 0x{:04x}, dropping",
                ethertype_raw
            );
        }
    }
}
