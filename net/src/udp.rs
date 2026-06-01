use slopos_ostd::klog_debug;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::packetbuf::PacketBuf;
use super::types::{Ipv4Addr, NetError, Port};

// =============================================================================
// Hash-bucket UDP demux table
// =============================================================================

/// Number of hash buckets. Must be a power of two.
const UDP_DEMUX_BUCKETS: usize = 16;

/// Maximum entries per bucket.
const UDP_ENTRIES_PER_BUCKET: usize = 8;

/// Hash a local port to a bucket index.
#[inline]
fn udp_demux_hash(port: Port) -> usize {
    let h = (port.0 as u64).wrapping_mul(0x9E3779B97F4A7C15u64);
    (h as usize >> 48) & (UDP_DEMUX_BUCKETS - 1)
}

#[derive(Clone, Copy)]
struct UdpDemuxEntry {
    local_ip: Ipv4Addr,
    local_port: Port,
    sock_idx: u32,
}

/// A single hash bucket in the UDP demux table.
pub struct UdpDemuxBucket {
    entries: [Option<UdpDemuxEntry>; UDP_ENTRIES_PER_BUCKET],
}

impl UdpDemuxBucket {
    const fn new() -> Self {
        Self {
            entries: [None; UDP_ENTRIES_PER_BUCKET],
        }
    }

    fn register(
        &mut self,
        local_ip: Ipv4Addr,
        local_port: Port,
        sock_idx: u32,
        reuse_addr: bool,
    ) -> Result<(), NetError> {
        for slot in &mut self.entries {
            if let Some(entry) = slot
                && entry.local_ip == local_ip
                && entry.local_port == local_port
            {
                if !reuse_addr {
                    return Err(NetError::AddressInUse);
                }
                entry.sock_idx = sock_idx;
                return Ok(());
            }
        }

        for slot in &mut self.entries {
            if slot.is_none() {
                *slot = Some(UdpDemuxEntry {
                    local_ip,
                    local_port,
                    sock_idx,
                });
                return Ok(());
            }
        }

        Err(NetError::NoBufferSpace)
    }

    fn unregister(&mut self, local_ip: Ipv4Addr, local_port: Port, sock_idx: u32) {
        for slot in &mut self.entries {
            if let Some(entry) = slot
                && entry.local_ip == local_ip
                && entry.local_port == local_port
                && entry.sock_idx == sock_idx
            {
                *slot = None;
            }
        }
    }

    fn lookup_exact(&self, dst_ip: Ipv4Addr, dst_port: Port) -> Option<u32> {
        for entry in self.entries.iter().flatten() {
            if entry.local_ip == dst_ip && entry.local_port == dst_port {
                return Some(entry.sock_idx);
            }
        }
        None
    }

    fn lookup_wildcard(&self, dst_port: Port) -> Option<u32> {
        for entry in self.entries.iter().flatten() {
            if entry.local_ip == Ipv4Addr::UNSPECIFIED && entry.local_port == dst_port {
                return Some(entry.sock_idx);
            }
        }
        None
    }

    fn clear(&mut self) {
        self.entries = [None; UDP_ENTRIES_PER_BUCKET];
    }
}

/// Shim that provides the same public API as the old monolithic `UdpDemuxTable`
/// but delegates to the per-bucket statics. This lets existing test code that
/// calls `UDP_DEMUX.lock().register(...)` keep compiling.
pub struct UdpDemuxTable;

impl UdpDemuxTable {
    pub const fn new() -> Self {
        Self
    }

    pub fn register(
        &mut self,
        local_ip: Ipv4Addr,
        local_port: Port,
        sock_idx: u32,
        reuse_addr: bool,
    ) -> Result<(), NetError> {
        let idx = udp_demux_hash(local_port);
        UDP_DEMUX_BUCKETS_TABLE[idx]
            .lock()
            .register(local_ip, local_port, sock_idx, reuse_addr)
    }

    pub fn unregister(&mut self, local_ip: Ipv4Addr, local_port: Port, sock_idx: u32) {
        let idx = udp_demux_hash(local_port);
        UDP_DEMUX_BUCKETS_TABLE[idx]
            .lock()
            .unregister(local_ip, local_port, sock_idx);
    }

    pub fn lookup(&self, dst_ip: Ipv4Addr, dst_port: Port) -> Option<u32> {
        let idx = udp_demux_hash(dst_port);
        let bucket = UDP_DEMUX_BUCKETS_TABLE[idx].lock();
        // Exact match first, then wildcard fallback.
        if let Some(sock) = bucket.lookup_exact(dst_ip, dst_port) {
            return Some(sock);
        }
        bucket.lookup_wildcard(dst_port)
    }

    pub fn clear(&mut self) {
        for bucket_mutex in UDP_DEMUX_BUCKETS_TABLE.iter() {
            bucket_mutex.lock().clear();
        }
    }
}

/// Per-bucket locks for the UDP demux table.
static UDP_DEMUX_BUCKETS_TABLE: [SpinLock<UdpDemuxBucket>; UDP_DEMUX_BUCKETS] = {
    const BUCKET: SpinLock<UdpDemuxBucket> =
        SpinLock::new(UdpDemuxBucket::new(), LOCK_LEVEL_REGISTRY);
    [BUCKET; UDP_DEMUX_BUCKETS]
};

/// Compatibility shim: existing code locks this to call register/unregister/lookup.
/// The actual per-bucket locking happens inside the method implementations.
pub static UDP_DEMUX: SpinLock<UdpDemuxTable> =
    SpinLock::new(UdpDemuxTable::new(), LOCK_LEVEL_REGISTRY);

pub(crate) fn parse_udp_header(payload: &[u8]) -> Option<(u16, u16, &[u8])> {
    if payload.len() < 8 {
        return None;
    }

    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    let udp_len = u16::from_be_bytes([payload[4], payload[5]]) as usize;

    if udp_len < 8 || udp_len > payload.len() {
        return None;
    }

    Some((src_port, dst_port, &payload[8..udp_len]))
}

pub fn handle_rx(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let Some((src_port, dst_port, udp_payload)) = parse_udp_header(pkt.payload()) else {
        return;
    };

    if src_port == super::dns::DNS_PORT {
        if let Some(d) = crate::net_driver_service::net_driver() {
            (d.dns_intercept_response)(udp_payload);
        }
    }

    let sock_idx = UDP_DEMUX.lock().lookup(Ipv4Addr(dst_ip), Port(dst_port));
    if let Some(sock_idx) = sock_idx {
        super::socket::socket_deliver_udp(sock_idx, src_ip, src_port, udp_payload);
        return;
    }

    klog_debug!(
        "udp: drop no socket for {}.{}.{}.{}:{}",
        dst_ip[0],
        dst_ip[1],
        dst_ip[2],
        dst_ip[3],
        dst_port
    );
}

pub fn udp_bind(
    sock_idx: u32,
    local_ip: Ipv4Addr,
    local_port: Port,
    reuse_addr: bool,
) -> Result<(), NetError> {
    UDP_DEMUX
        .lock()
        .register(local_ip, local_port, sock_idx, reuse_addr)
}

pub fn udp_unbind(sock_idx: u32, local_ip: Ipv4Addr, local_port: Port) {
    UDP_DEMUX.lock().unregister(local_ip, local_port, sock_idx);
}

pub fn udp_sendto(
    local_ip: [u8; 4],
    dst_ip: [u8; 4],
    local_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Result<usize, NetError> {
    if payload.len() > 1472 {
        return Err(NetError::InvalidArgument);
    }

    let mut pkt = PacketBuf::alloc().ok_or(NetError::NoBufferSpace)?;
    pkt.append(payload)?;

    let udp_len = 8 + payload.len();
    {
        let udp_hdr = pkt.push_header(8)?;
        udp_hdr[0..2].copy_from_slice(&local_port.to_be_bytes());
        udp_hdr[2..4].copy_from_slice(&dst_port.to_be_bytes());
        udp_hdr[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        udp_hdr[6..8].copy_from_slice(&0u16.to_be_bytes());
    }

    pkt.prepend_ipv4(local_ip, dst_ip, super::IpProtocol::Udp.as_u8(), udp_len)?;

    let src_mac = crate::net_driver_service::net_driver()
        .and_then(|d| (d.virtio_net_mac)())
        .unwrap_or([0; 6]);
    pkt.prepend_eth(src_mac, super::MacAddr::BROADCAST.0)?;
    pkt.set_ipv4_offsets();

    let udp_checksum = pkt.compute_udp_checksum(Ipv4Addr(local_ip), Ipv4Addr(dst_ip));
    let udp_start = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
    let frame = pkt.payload_mut();
    frame[udp_start + 6..udp_start + 8].copy_from_slice(&udp_checksum.to_be_bytes());

    super::ipv4::send(Ipv4Addr(dst_ip), pkt).map_err(|_| NetError::NetworkUnreachable)?;
    Ok(payload.len())
}

/// Single-direct-copy `udp_sendto`: the datagram payload is volatile-copied
/// **once**, straight from the pinned user pages (via `reader`) into the packet
/// buffer — no kernel staging scratch. Everything else (headers, checksum over
/// the in-packet payload) matches [`udp_sendto`]. The payload length is
/// `reader.remain()`.
pub fn udp_sendto_from(
    local_ip: [u8; 4],
    dst_ip: [u8; 4],
    local_port: u16,
    dst_port: u16,
    reader: &mut slopos_ostd::mm::VmReader<'_>,
) -> Result<usize, NetError> {
    let payload_len = reader.remain();
    if payload_len > 1472 {
        return Err(NetError::InvalidArgument);
    }

    let mut pkt = PacketBuf::alloc().ok_or(NetError::NoBufferSpace)?;
    let copied = pkt.append_from(reader, payload_len)?;

    let udp_len = 8 + copied;
    {
        let udp_hdr = pkt.push_header(8)?;
        udp_hdr[0..2].copy_from_slice(&local_port.to_be_bytes());
        udp_hdr[2..4].copy_from_slice(&dst_port.to_be_bytes());
        udp_hdr[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        udp_hdr[6..8].copy_from_slice(&0u16.to_be_bytes());
    }

    pkt.prepend_ipv4(local_ip, dst_ip, super::IpProtocol::Udp.as_u8(), udp_len)?;

    let src_mac = crate::net_driver_service::net_driver()
        .and_then(|d| (d.virtio_net_mac)())
        .unwrap_or([0; 6]);
    pkt.prepend_eth(src_mac, super::MacAddr::BROADCAST.0)?;
    pkt.set_ipv4_offsets();

    let udp_checksum = pkt.compute_udp_checksum(Ipv4Addr(local_ip), Ipv4Addr(dst_ip));
    let udp_start = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
    let frame = pkt.payload_mut();
    frame[udp_start + 6..udp_start + 8].copy_from_slice(&udp_checksum.to_be_bytes());

    super::ipv4::send(Ipv4Addr(dst_ip), pkt).map_err(|_| NetError::NetworkUnreachable)?;
    Ok(copied)
}

/// True NIC-DMA zero-copy `udp_sendto` (SlopRing `OP_SEND_ZC`): the
/// datagram payload is **never** copied — the NIC DMAs it straight from the
/// pinned user pages (`runs` = coalesced `(paddr, len)` physical runs summing to
/// `total_len`). Only the 42-byte L2/L3/L4 header is built (in a stack buffer);
/// the UDP checksum is offloaded to the device via the pseudo-header seed +
/// `CsumOffload`, so the CPU touches 0 payload bytes.
///
/// Eligibility (any miss returns [`ZcSendOutcome::NotEligible`] so the caller
/// falls back to the single-copy leaf, which queues + drives ARP and computes
/// the checksum CPU-side): unicast destination, a route, a **resolved** neighbor
/// MAC (non-queuing cache peek — no ARP issued here), TX checksum offload, and
/// `total_len <= 1472`. A full TX ring returns [`ZcSendOutcome::WouldBlock`]
/// (the ring defers and re-attempts). `keepalive`/`token` are handed to the
/// driver, which holds them until the NIC reclaims the descriptor.
pub fn udp_sendto_zerocopy(
    local_ip: [u8; 4],
    dst_ip: [u8; 4],
    local_port: u16,
    dst_port: u16,
    runs: &[(u64, u32)],
    total_len: usize,
    keepalive: slopos_ostd::KVec<
        slopos_ostd::mm::uframe::UFrame<slopos_ostd::mm::frame::AnonymousMeta>,
    >,
    token: slopos_ostd::TxReclaimToken,
) -> crate::socket::ZcSendOutcome {
    use super::netdev::{CsumOffload, DEVICE_REGISTRY, NetDeviceFeatures};
    use crate::socket::ZcSendOutcome;

    if total_len == 0 || total_len > 1472 {
        return ZcSendOutcome::NotEligible;
    }
    let dst = Ipv4Addr(dst_ip);
    if dst.is_loopback() || dst.is_broadcast() || dst.is_multicast() {
        return ZcSendOutcome::NotEligible;
    }
    let Some((dev, next_hop)) = super::route::ROUTE_TABLE.lookup(dst) else {
        return ZcSendOutcome::NotEligible;
    };
    if next_hop.is_loopback() {
        return ZcSendOutcome::NotEligible;
    }
    let Some(dst_mac) = super::neighbor::NEIGHBOR_CACHE.lookup(dev, next_hop) else {
        return ZcSendOutcome::NotEligible; // cache miss → copy path queues + ARPs
    };
    match DEVICE_REGISTRY.features_by_index(dev) {
        Some(f) if f.contains(NetDeviceFeatures::CHECKSUM_TX) => {}
        _ => return ZcSendOutcome::NotEligible,
    }
    let Some(src_mac) = DEVICE_REGISTRY.mac_by_index(dev) else {
        return ZcSendOutcome::NotEligible;
    };

    let udp_len = 8 + total_len;
    let ip_total = super::IPV4_HEADER_LEN + udp_len;
    let mut hdr = [0u8; super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN + 8];
    // Ethernet (0..14).
    hdr[0..6].copy_from_slice(&dst_mac.0);
    hdr[6..12].copy_from_slice(&src_mac.0);
    hdr[12..14].copy_from_slice(&super::EtherType::Ipv4.to_be_bytes());
    // IPv4 (14..34).
    {
        let ip = &mut hdr[super::ETH_HEADER_LEN..super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN];
        ip[0] = 0x45;
        ip[1] = 0;
        ip[2..4].copy_from_slice(&(ip_total as u16).to_be_bytes());
        ip[4..8].copy_from_slice(&[0; 4]);
        ip[8] = 64;
        ip[9] = super::IpProtocol::Udp.as_u8();
        ip[10..12].copy_from_slice(&[0; 2]);
        ip[12..16].copy_from_slice(&local_ip);
        ip[16..20].copy_from_slice(&dst_ip);
        let ip_csum = super::checksum::internet_checksum(ip);
        ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());
    }
    // UDP (34..42). Checksum field holds the pseudo-header seed; the NIC sums
    // the DMA'd payload over [csum_start..] and completes it (NEEDS_CSUM).
    {
        let l4 = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
        let udp = &mut hdr[l4..l4 + 8];
        udp[0..2].copy_from_slice(&local_port.to_be_bytes());
        udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
        udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        let seed = super::checksum::pseudo_header_seed(
            local_ip,
            dst_ip,
            super::IpProtocol::Udp.as_u8(),
            udp_len,
        );
        udp[6..8].copy_from_slice(&seed.to_be_bytes());
    }

    let csum = CsumOffload {
        csum_start: (super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN) as u16,
        csum_offset: 6,
    };
    match DEVICE_REGISTRY.tx_zerocopy_by_index(dev, &hdr, runs, Some(csum), keepalive, token) {
        Ok(()) => ZcSendOutcome::Submitted(total_len),
        Err(NetError::NoBufferSpace) => ZcSendOutcome::WouldBlock,
        Err(_) => ZcSendOutcome::NotEligible,
    }
}

pub fn udp_recvfrom() -> Result<(), NetError> {
    Err(NetError::WouldBlock)
}
