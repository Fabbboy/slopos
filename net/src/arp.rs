//! ARP protocol handler — request/reply processing and frame construction.
//!
//! RFC 826 ARP for Ethernet/IPv4, feeding the
//! [`NeighborCache`](super::neighbor::NEIGHBOR_CACHE).

use slopos_ostd::klog_debug;

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

/// Handle an incoming ARP frame; `pkt`'s head is at the ARP header, the
/// Ethernet header having been consumed by the ingress pipeline.
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
            // Cache update and pending flush already done above.
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

/// Send an ARP request for `target_ip` via `handle`.
pub fn send_request(handle: &DeviceHandle, target_ip: Ipv4Addr) {
    let our_mac = handle.mac();
    let our_ip = get_our_ip();

    let frame_len = ETH_HEADER_LEN + ARP_HEADER_LEN;

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_request — pool exhausted");
        return;
    };

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

    let _ = frame_len;

    klog_debug!(
        "arp: sending request for {} on dev {}",
        target_ip,
        handle.index()
    );
    if let Err(e) = handle.tx(pkt) {
        klog_debug!("arp: send_request tx failed: {}", e);
    }
}

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

/// Perform the I/O the neighbor cache deferred so TX never runs under its lock.
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

/// Set the destination MAC, assuming the egress path left `l2_offset` at the
/// start of the Ethernet header.
pub fn set_dst_mac_in_eth_header(pkt: &mut PacketBuf, mac: MacAddr) {
    let l2 = pkt.l2_offset() as usize;
    let payload = pkt.payload_mut();
    if payload.len() >= l2 + ETH_ADDR_LEN {
        // TODO(tech-debt): dead branch; the write below assumes l2_offset == head.
    }
    let data = pkt.payload_mut();
    if data.len() >= ETH_ADDR_LEN {
        data[..ETH_ADDR_LEN].copy_from_slice(&mac.0);
    }
}

/// Our IPv4 address, or `UNSPECIFIED` when no interface is configured yet;
/// callers check before claiming it in a reply.
fn get_our_ip() -> Ipv4Addr {
    super::iface::first_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED)
}

/// Send an ARP request by device index, for the route-aware egress path that
/// holds no [`DeviceHandle`].
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
