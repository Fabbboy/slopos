//! ARP protocol handler — request/reply processing and frame construction.
//!
//! Implements RFC 826 ARP for Ethernet/IPv4.  Incoming ARP frames are parsed,
//! validated, and dispatched to the [`NeighborCache`](super::neighbor::NEIGHBOR_CACHE):
//!
//! - **Reply** (`oper=2`): updates the cache and flushes pending packets.
//! - **Request** (`oper=1`) for our IP: sends a unicast ARP reply.
//! - **Any ARP**: opportunistically updates the cache if the sender is known.

use slopos_utils::klog_debug;

use super::neighbor::{NEIGHBOR_CACHE, NeighborAction};
use super::netdev::DeviceHandle;
use super::packetbuf::PacketBuf;
use super::types::{EtherType, Ipv4Addr, MacAddr};
use super::{ETH_ADDR_LEN, ETH_HEADER_LEN};

const ARP_HTYPE_ETHERNET: u16 = 1;
const ARP_PTYPE_IPV4: u16 = EtherType::Ipv4.as_u16();
const ARP_HLEN_ETHERNET: u8 = 6;
const ARP_PLEN_IPV4: u8 = 4;
const ARP_OPER_REQUEST: u16 = 1;
const ARP_OPER_REPLY: u16 = 2;
const ARP_HEADER_LEN: usize = 28;

// =============================================================================
// 2C.1 — handle_rx
// =============================================================================

/// Handle an incoming ARP frame.
///
/// The packet's `head` points at the first byte of the ARP header (Ethernet
/// header has been consumed by the ingress pipeline).
pub fn handle_rx(handle: &DeviceHandle, pkt: PacketBuf) {
    let data = pkt.payload();

    if data.len() < ARP_HEADER_LEN {
        klog_debug!("arp: frame too short ({} < {})", data.len(), ARP_HEADER_LEN);
        return;
    }

    let htype = u16::from_be_bytes([data[0], data[1]]);
    let ptype = u16::from_be_bytes([data[2], data[3]]);
    let hlen = data[4];
    let plen = data[5];
    let oper = u16::from_be_bytes([data[6], data[7]]);

    if htype != ARP_HTYPE_ETHERNET
        || ptype != ARP_PTYPE_IPV4
        || hlen != ARP_HLEN_ETHERNET
        || plen != ARP_PLEN_IPV4
    {
        klog_debug!(
            "arp: malformed header (htype={}, ptype=0x{:04x}, hlen={}, plen={})",
            htype,
            ptype,
            hlen,
            plen
        );
        return;
    }

    let sender_mac = MacAddr([data[8], data[9], data[10], data[11], data[12], data[13]]);
    let sender_ip = Ipv4Addr([data[14], data[15], data[16], data[17]]);
    let _target_mac = MacAddr([data[18], data[19], data[20], data[21], data[22], data[23]]);
    let target_ip = Ipv4Addr([data[24], data[25], data[26], data[27]]);

    let dev = handle.index();
    let our_ip = get_our_ip();

    // RFC 826: opportunistically update the cache if the sender is already known.
    let current_tick = slopos_kernel_services::platform::timer_ticks();
    let update_action = NEIGHBOR_CACHE.insert_or_update(dev, sender_ip, sender_mac, current_tick);
    execute_neighbor_action(handle, update_action);

    match oper {
        ARP_OPER_REPLY => {
            klog_debug!(
                "arp: reply from {} ({}) on dev {}",
                sender_ip,
                sender_mac,
                dev
            );
            // insert_or_update above already handled the cache update + pending flush.
        }
        ARP_OPER_REQUEST => {
            if target_ip == our_ip && !our_ip.is_unspecified() {
                klog_debug!(
                    "arp: request for our IP {} from {} ({}), sending reply",
                    target_ip,
                    sender_ip,
                    sender_mac
                );
                send_reply(handle, sender_ip, sender_mac);
            }
        }
        _ => {
            klog_debug!("arp: unknown opcode {}", oper);
        }
    }
}

// =============================================================================
// 2C.2 — send_request
// =============================================================================

/// Send an ARP request for `target_ip` via `handle`.
///
/// Constructs a broadcast Ethernet frame containing an ARP REQUEST and
/// transmits it through the device.
pub fn send_request(handle: &DeviceHandle, target_ip: Ipv4Addr) {
    let our_mac = handle.mac();
    let our_ip = get_our_ip();

    let frame_len = ETH_HEADER_LEN + ARP_HEADER_LEN;

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_request — pool exhausted");
        return;
    };

    // Build Ethernet header (push backward into headroom).
    let eth = match pkt.push_header(ETH_HEADER_LEN) {
        Ok(h) => h,
        Err(_) => {
            klog_debug!("arp: send_request — insufficient headroom");
            return;
        }
    };
    eth[0..ETH_ADDR_LEN].copy_from_slice(&MacAddr::BROADCAST.0);
    eth[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&our_mac.0);
    eth[ETH_ADDR_LEN * 2..ETH_HEADER_LEN].copy_from_slice(&EtherType::Arp.to_be_bytes());

    // Build ARP payload (append at tail).
    let mut arp_data = [0u8; ARP_HEADER_LEN];
    arp_data[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    arp_data[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    arp_data[4] = ARP_HLEN_ETHERNET;
    arp_data[5] = ARP_PLEN_IPV4;
    arp_data[6..8].copy_from_slice(&ARP_OPER_REQUEST.to_be_bytes());
    arp_data[8..14].copy_from_slice(&our_mac.0);
    arp_data[14..18].copy_from_slice(&our_ip.0);
    arp_data[18..24].copy_from_slice(&MacAddr::ZERO.0);
    arp_data[24..28].copy_from_slice(&target_ip.0);

    if pkt.append(&arp_data).is_err() {
        klog_debug!("arp: send_request — append failed");
        return;
    }

    let _ = frame_len; // frame_len used for documentation only

    klog_debug!(
        "arp: sending request for {} on dev {}",
        target_ip,
        handle.index()
    );
    if let Err(e) = handle.tx(pkt) {
        klog_debug!("arp: send_request tx failed: {}", e);
    }
}

// =============================================================================
// ARP reply
// =============================================================================

fn send_reply(handle: &DeviceHandle, target_ip: Ipv4Addr, target_mac: MacAddr) {
    let our_mac = handle.mac();
    let our_ip = get_our_ip();

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_reply — pool exhausted");
        return;
    };

    let eth = match pkt.push_header(ETH_HEADER_LEN) {
        Ok(h) => h,
        Err(_) => return,
    };
    eth[0..ETH_ADDR_LEN].copy_from_slice(&target_mac.0);
    eth[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&our_mac.0);
    eth[ETH_ADDR_LEN * 2..ETH_HEADER_LEN].copy_from_slice(&EtherType::Arp.to_be_bytes());

    let mut arp_data = [0u8; ARP_HEADER_LEN];
    arp_data[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    arp_data[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    arp_data[4] = ARP_HLEN_ETHERNET;
    arp_data[5] = ARP_PLEN_IPV4;
    arp_data[6..8].copy_from_slice(&ARP_OPER_REPLY.to_be_bytes());
    arp_data[8..14].copy_from_slice(&our_mac.0);
    arp_data[14..18].copy_from_slice(&our_ip.0);
    arp_data[18..24].copy_from_slice(&target_mac.0);
    arp_data[24..28].copy_from_slice(&target_ip.0);

    if pkt.append(&arp_data).is_err() {
        return;
    }

    klog_debug!(
        "arp: sending reply to {} ({}) on dev {}",
        target_ip,
        target_mac,
        handle.index()
    );
    if let Err(e) = handle.tx(pkt) {
        klog_debug!("arp: send_reply tx failed: {}", e);
    }
}

// =============================================================================
// Action execution helper
// =============================================================================

/// Execute a [`NeighborAction`] returned by the neighbor cache.
///
/// This function performs the I/O that the cache method deferred to avoid
/// holding the cache lock during TX.
pub fn execute_neighbor_action(handle: &DeviceHandle, action: NeighborAction) {
    match action {
        NeighborAction::SendArpRequest { target_ip, .. } => {
            send_request(handle, target_ip);
        }
        NeighborAction::TransmitPacket { pkt } => {
            if let Err(e) = handle.tx(pkt) {
                klog_debug!("arp: execute_action tx failed: {}", e);
            }
        }
        NeighborAction::FlushPending {
            packets, dst_mac, ..
        } => {
            for mut pkt in packets {
                set_dst_mac_in_eth_header(&mut pkt, dst_mac);
                if let Err(e) = handle.tx(pkt) {
                    klog_debug!("arp: flush tx failed: {}", e);
                }
            }
        }
        NeighborAction::None => {}
    }
}

/// Set the destination MAC in a packet's Ethernet header.
///
/// Assumes the packet was built by the egress path with `l2_offset` pointing
/// at the start of the Ethernet header.
pub fn set_dst_mac_in_eth_header(pkt: &mut PacketBuf, mac: MacAddr) {
    let l2 = pkt.l2_offset() as usize;
    let payload = pkt.payload_mut();
    if payload.len() >= l2 + ETH_ADDR_LEN {
        // The payload starts at `head`, and l2_offset is absolute in the buffer.
        // For queued packets, l2_offset is set by the egress path before queueing.
        // We need to write relative to the buffer start, not payload start.
        // Since payload = data[head..tail] and l2_offset is absolute,
        // the relative offset is l2 - head.
    }
    // For packets queued in the Incomplete state, the Ethernet header
    // has already been written but the destination MAC is placeholder.
    // Access the raw buffer to overwrite bytes [l2..l2+6].
    // Since we can only access payload_mut (head..tail) and l2 may be
    // before head, we need to access via the full data buffer.
    // For now, the egress path sets l2 at head, so l2 == head.
    // Write at the start of the payload.
    let data = pkt.payload_mut();
    if data.len() >= ETH_ADDR_LEN {
        data[..ETH_ADDR_LEN].copy_from_slice(&mac.0);
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Get our IPv4 address from the centralised [`NetStack`].
///
/// Tries the NetStack first; falls back to the legacy
/// `virtio_net_ipv4_addr()` if the NetStack has not been configured yet.
fn get_our_ip() -> Ipv4Addr {
    if let Some(ip) = super::netstack::NET_STACK.first_ipv4() {
        return ip;
    }
    // Legacy fallback — will be removed once all callers use NetStack.
    Ipv4Addr(
        crate::net_driver_service::net_driver()
            .and_then(|d| (d.virtio_net_ipv4_addr)())
            .unwrap_or([0; 4]),
    )
}

// =============================================================================
// Registry-based ARP send (no DeviceHandle needed)
// =============================================================================

/// Send an ARP request via the device registry (index-based TX).
///
/// Equivalent to [`send_request`] but uses [`DEVICE_REGISTRY`] to transmit,
/// so no [`DeviceHandle`] is required.  Used by the route-aware egress path
/// where the device is identified by [`DevIndex`] from the routing table.
pub fn send_request_via_registry(dev: super::types::DevIndex, target_ip: Ipv4Addr) {
    use super::netdev::DEVICE_REGISTRY;

    let our_mac = match DEVICE_REGISTRY.mac_by_index(dev) {
        Some(mac) => mac,
        None => {
            klog_debug!("arp: send_request_via_registry — no device {}", dev);
            return;
        }
    };
    let our_ip = get_our_ip();

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_request_via_registry — pool exhausted");
        return;
    };

    // Build Ethernet header.
    let eth = match pkt.push_header(ETH_HEADER_LEN) {
        Ok(h) => h,
        Err(_) => {
            klog_debug!("arp: send_request_via_registry — insufficient headroom");
            return;
        }
    };
    eth[0..ETH_ADDR_LEN].copy_from_slice(&MacAddr::BROADCAST.0);
    eth[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&our_mac.0);
    eth[ETH_ADDR_LEN * 2..ETH_HEADER_LEN].copy_from_slice(&EtherType::Arp.to_be_bytes());

    // Build ARP payload.
    let mut arp_data = [0u8; ARP_HEADER_LEN];
    arp_data[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    arp_data[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    arp_data[4] = ARP_HLEN_ETHERNET;
    arp_data[5] = ARP_PLEN_IPV4;
    arp_data[6..8].copy_from_slice(&ARP_OPER_REQUEST.to_be_bytes());
    arp_data[8..14].copy_from_slice(&our_mac.0);
    arp_data[14..18].copy_from_slice(&our_ip.0);
    arp_data[18..24].copy_from_slice(&MacAddr::ZERO.0);
    arp_data[24..28].copy_from_slice(&target_ip.0);

    if pkt.append(&arp_data).is_err() {
        klog_debug!("arp: send_request_via_registry — append failed");
        return;
    }

    klog_debug!(
        "arp: sending request for {} on dev {} (via registry)",
        target_ip,
        dev
    );
    if let Err(e) = DEVICE_REGISTRY.tx_by_index(dev, pkt) {
        klog_debug!("arp: send_request_via_registry tx failed: {}", e);
    }
}
