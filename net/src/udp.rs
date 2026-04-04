use slopos_sync::IrqMutex;
use slopos_utils::klog_debug;

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
static UDP_DEMUX_BUCKETS_TABLE: [IrqMutex<UdpDemuxBucket>; UDP_DEMUX_BUCKETS] = {
    const BUCKET: IrqMutex<UdpDemuxBucket> = IrqMutex::new(UdpDemuxBucket::new());
    [BUCKET; UDP_DEMUX_BUCKETS]
};

/// Compatibility shim: existing code locks this to call register/unregister/lookup.
/// The actual per-bucket locking happens inside the method implementations.
pub static UDP_DEMUX: IrqMutex<UdpDemuxTable> = IrqMutex::new(UdpDemuxTable::new());

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

pub fn udp_recvfrom() -> Result<(), NetError> {
    Err(NetError::WouldBlock)
}
