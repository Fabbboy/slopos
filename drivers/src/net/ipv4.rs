//! IPv4 ingress and egress handlers.
//!
//! # Ingress
//!
//! [`handle_rx`] is the single entry point for all received IPv4 packets after
//! Ethernet demux.  It validates the IP header (version, length, checksum, TTL),
//! sets the L4 layer offset on the [`PacketBuf`], and dispatches to the
//! appropriate protocol handler (TCP, UDP, ICMP).
//!
//! # Egress
//!
//! [`send`] is the route-aware egress entry point.  It performs a routing table
//! lookup to determine the outgoing device and next hop, then either transmits
//! directly (broadcast/multicast/loopback) or delegates to the neighbor cache
//! for ARP resolution.
//!
//! [`send_via`] is the lower-level egress path for callers that already have a
//! [`DeviceHandle`] and know the next hop (e.g., timer-driven retransmits).
//!
//! # Scope
//!
//! - Full IPv4 header validation
//! - Protocol dispatch to existing TCP/UDP handlers via the socket layer
//! - DNS response interception for the in-kernel resolver

use slopos_lib::klog_debug;

use super::socket;
use super::tcp;
use super::types::{DevIndex, IpProtocol};
use crate::net::{self as net, NetError, packetbuf::PacketBuf};

/// Handle an incoming IPv4 packet.
///
/// Called from [`super::ingress::net_rx`] after Ethernet demux.  The packet's
/// `head` points at the first byte of the IP header (Ethernet header has been
/// consumed via [`PacketBuf::pull_header`]).
///
/// # Validation
///
/// 1. IP version must be 4
/// 2. IHL ≥ 5 (header length ≥ 20 bytes)
/// 3. Total length ≤ packet size
/// 4. Header checksum must verify (unless device has `CHECKSUM_RX`)
/// 5. TTL > 0 (we don't forward, so TTL=0 is always dropped)
///
/// Packets failing any check are silently dropped with a debug log.
pub fn handle_rx(dev: DevIndex, mut pkt: PacketBuf, checksum_rx: bool) {
    // Extract all fields we need while borrowing the payload immutably.
    // We must drop this borrow before calling pkt.set_l4() / pkt.pull_header().
    let (proto, src_ip, dst_ip, ihl) = {
        let ip_data = pkt.payload();
        if ip_data.len() < net::IPV4_HEADER_LEN {
            klog_debug!(
                "ipv4: packet too short ({} < {})",
                ip_data.len(),
                net::IPV4_HEADER_LEN
            );
            return;
        }

        // Version must be 4.
        let version = (ip_data[0] >> 4) & 0x0F;
        if version != 4 {
            klog_debug!("ipv4: bad version {}", version);
            return;
        }

        // Internet Header Length (in 32-bit words).
        let ihl = ((ip_data[0] & 0x0F) as usize) * 4;
        if ihl < net::IPV4_HEADER_LEN || ip_data.len() < ihl {
            klog_debug!("ipv4: bad IHL {} (packet len {})", ihl, ip_data.len());
            return;
        }

        // Total length sanity check.
        let total_len = u16::from_be_bytes([ip_data[2], ip_data[3]]) as usize;
        if total_len > ip_data.len() {
            klog_debug!(
                "ipv4: total_len {} > packet len {}",
                total_len,
                ip_data.len()
            );
            return;
        }

        // Header checksum verification (skip if device already verified).
        if !checksum_rx && net::ipv4_header_checksum(&ip_data[..ihl]) != 0 {
            klog_debug!("ipv4: bad header checksum");
            return;
        }

        // TTL check — we don't forward, so TTL=0 is always invalid.
        let ttl = ip_data[8];
        if ttl == 0 {
            klog_debug!("ipv4: TTL=0, dropping");
            return;
        }

        let proto = ip_data[9];
        let src_ip: [u8; 4] = ip_data[12..16].try_into().unwrap_or([0; 4]);
        let dst_ip: [u8; 4] = ip_data[16..20].try_into().unwrap_or([0; 4]);

        (proto, src_ip, dst_ip, ihl)
    };
    // Immutable borrow of pkt dropped here.

    // Set L4 offset (absolute position: current head + IHL).
    pkt.set_l4(pkt.head() + ihl as u16);

    // Pull the IP header so payload() now points at the L4 data.
    if pkt.pull_header(ihl).is_err() {
        return;
    }

    // Dispatch to L4 protocol handler.
    match IpProtocol::from_u8(proto) {
        Some(IpProtocol::Tcp) => dispatch_tcp(src_ip, dst_ip, &pkt),
        Some(IpProtocol::Udp) => dispatch_udp(src_ip, dst_ip, &pkt),
        Some(IpProtocol::Icmp) => super::icmp::handle_rx(src_ip, dst_ip, &pkt),
        None => {
            klog_debug!("ipv4: unknown protocol {}, dropping", proto);
        }
    }

    let _ = dev;
}

// =============================================================================
// L4 dispatch helpers
// =============================================================================

/// Dispatch a TCP segment to the TCP state machine and socket layer.
///
/// Uses the [`TcpDemuxTable`] for fast 4-tuple / 2-tuple lookup,
/// then delegates to `tcp_input()` for full state-machine processing.
fn dispatch_tcp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let ip_payload = pkt.payload();

    let Some(hdr) = tcp::parse_header(ip_payload) else {
        return;
    };
    let hdr_len = hdr.header_len();
    if hdr_len < tcp::TCP_HEADER_LEN || ip_payload.len() < hdr_len {
        return;
    }
    let options = &ip_payload[tcp::TCP_HEADER_LEN..hdr_len];
    let payload = &ip_payload[hdr_len..];
    let now_ms = slopos_lib::clock::uptime_ms();

    // Demux table pre-lookup for debug/fast-path validation.
    // The demux table provides O(n) lookup by 4-tuple (established) or
    // 2-tuple (listener). The actual state machine processing still goes
    // through tcp_input() which uses TcpConnectionTable internally.
    {
        use super::tcp_socket::TCP_DEMUX;
        use super::types::{Ipv4Addr, Port};

        let demux = TCP_DEMUX.lock();
        let local_ip = Ipv4Addr(dst_ip);
        let local_port = Port(hdr.dst_port);
        let remote_ip = Ipv4Addr(src_ip);
        let remote_port = Port(hdr.src_port);

        if let Some(conn_id) =
            demux.lookup_established(local_ip, local_port, remote_ip, remote_port)
        {
            klog_debug!(
                "tcp demux: established conn_id={} for {}:{} -> {}:{}",
                conn_id,
                src_ip[0],
                src_ip[1],
                hdr.src_port,
                hdr.dst_port
            );
        } else if let Some(sock_idx) = demux.lookup_listener(local_ip, local_port) {
            klog_debug!(
                "tcp demux: listener sock_idx={} for port {}",
                sock_idx,
                hdr.dst_port
            );
        }
    }

    let result = tcp::tcp_input(src_ip, dst_ip, &hdr, options, payload, now_ms);

    if let Some(seg) = result.response {
        let _ = socket::socket_send_tcp_segment(&seg, &[]);
    }
    socket::socket_notify_tcp_activity(&result);
}

/// Dispatch a UDP datagram to the socket layer, with DNS interception.
///
/// Mirrors the logic previously in `dispatch_rx_frame()` in `virtio_net.rs`.
fn dispatch_udp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    super::udp::handle_rx(src_ip, dst_ip, pkt);
}

// =============================================================================
// Route-aware IPv4 egress
// =============================================================================

/// Route-aware IPv4 send.
///
/// Performs a routing table lookup to determine the outgoing device and next
/// hop, selects the source IP from the outgoing interface, then sends through
/// the neighbor cache (or directly for loopback/broadcast/multicast).
///
/// This is the primary egress entry point for the socket layer.
/// For callers that already hold a [`DeviceHandle`], use [`send_via`] instead.
pub fn send(dst_ip: super::types::Ipv4Addr, pkt: PacketBuf) -> Result<(), NetError> {
    use super::netdev::DEVICE_REGISTRY;
    use super::route::ROUTE_TABLE;

    let (dev, next_hop) = ROUTE_TABLE.lookup(dst_ip).ok_or_else(|| {
        klog_debug!("ipv4::send: no route to {}", dst_ip);
        NetError::NetworkUnreachable
    })?;

    // Loopback: skip neighbor resolution entirely — no ARP on lo.
    if next_hop.is_loopback() || dst_ip.is_loopback() {
        return DEVICE_REGISTRY.tx_by_index(dev, pkt);
    }

    // Broadcast/multicast: skip neighbor resolution, TX directly.
    if dst_ip.is_broadcast() || dst_ip.is_multicast() {
        return DEVICE_REGISTRY.tx_by_index(dev, pkt);
    }

    // Unicast on a physical device: neighbor cache resolution.
    send_on_device(dev, next_hop, pkt)
}

/// Send an IPv4 packet through a specific device via neighbor cache.
///
/// This is the handle-based egress path, preserved for callers
/// that already have a [`DeviceHandle`] (e.g., timer-driven ARP retransmit).
/// For route-aware sending, prefer [`send`].
pub fn send_via(
    handle: &super::netdev::DeviceHandle,
    dst_ip: super::types::Ipv4Addr,
    pkt: PacketBuf,
) -> Result<(), NetError> {
    use super::arp;
    use super::neighbor::{NEIGHBOR_CACHE, ResolveOutcome};

    let dev = handle.index();
    let next_hop = dst_ip;

    // Broadcast/multicast: skip neighbor resolution, TX directly.
    if dst_ip.is_broadcast() || dst_ip.is_multicast() {
        if let Err(e) = handle.tx(pkt) {
            klog_debug!("ipv4::send_via: broadcast tx failed: {}", e);
            return Err(e);
        }
        return Ok(());
    }

    match NEIGHBOR_CACHE.resolve(dev, next_hop, pkt) {
        ResolveOutcome::Resolved {
            mac,
            mut pkt,
            action,
        } => {
            arp::set_dst_mac_in_eth_header(&mut pkt, mac);
            if let Some(act) = action {
                arp::execute_neighbor_action(handle, act);
            }
            if let Err(e) = handle.tx(pkt) {
                klog_debug!("ipv4::send_via: tx failed: {}", e);
                return Err(e);
            }
            Ok(())
        }
        ResolveOutcome::Queued => Ok(()),
        ResolveOutcome::ArpNeeded(action) => {
            arp::execute_neighbor_action(handle, action);
            Ok(())
        }
        ResolveOutcome::Failed(e) => {
            klog_debug!(
                "ipv4::send_via: neighbor resolution failed for {}: {}",
                dst_ip,
                e
            );
            Err(e)
        }
    }
}

/// Internal: send a unicast packet on a specific device via neighbor cache.
///
/// Uses `DEVICE_REGISTRY` for TX (takes registry lock briefly).  This is the
/// code path used by the route-aware [`send`] function for non-loopback,
/// non-broadcast unicast traffic.
fn send_on_device(
    dev: DevIndex,
    next_hop: super::types::Ipv4Addr,
    pkt: PacketBuf,
) -> Result<(), NetError> {
    use super::arp;
    use super::neighbor::{NEIGHBOR_CACHE, ResolveOutcome};
    use super::netdev::DEVICE_REGISTRY;

    match NEIGHBOR_CACHE.resolve(dev, next_hop, pkt) {
        ResolveOutcome::Resolved {
            mac,
            mut pkt,
            action,
        } => {
            arp::set_dst_mac_in_eth_header(&mut pkt, mac);
            if let Some(act) = action {
                execute_neighbor_action_via_registry(dev, act);
            }
            DEVICE_REGISTRY.tx_by_index(dev, pkt)
        }
        ResolveOutcome::Queued => Ok(()),
        ResolveOutcome::ArpNeeded(action) => {
            execute_neighbor_action_via_registry(dev, action);
            Ok(())
        }
        ResolveOutcome::Failed(e) => {
            klog_debug!(
                "ipv4::send: neighbor resolution failed for {}: {}",
                next_hop,
                e
            );
            Err(e)
        }
    }
}

/// Execute a neighbor action (ARP request, flush pending) via the device
/// registry, without requiring a [`DeviceHandle`].
fn execute_neighbor_action_via_registry(_dev: DevIndex, action: super::neighbor::NeighborAction) {
    use super::arp;
    use super::netdev::DEVICE_REGISTRY;

    match action {
        super::neighbor::NeighborAction::SendArpRequest { dev, target_ip } => {
            // Build and send ARP request via registry.
            arp::send_request_via_registry(dev, target_ip);
        }
        super::neighbor::NeighborAction::FlushPending {
            packets,
            dst_mac,
            dev,
        } => {
            for mut pkt in packets {
                arp::set_dst_mac_in_eth_header(&mut pkt, dst_mac);
                let _ = DEVICE_REGISTRY.tx_by_index(dev, pkt);
            }
        }
        super::neighbor::NeighborAction::TransmitPacket { pkt } => {
            // Single packet TX — use default device (dev 1 = VirtIO).
            let _ = DEVICE_REGISTRY.tx_by_index(DevIndex(1), pkt);
        }
        super::neighbor::NeighborAction::None => {}
    }
}
