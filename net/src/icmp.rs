use slopos_abi::net::MAX_SOCKETS;
use slopos_ostd::klog_debug;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::packetbuf::PacketBuf;
use super::types::{Ipv4Addr, NetError};

pub const ICMP_HEADER_LEN: usize = 8;
pub const ICMP_TYPE_ECHO_REPLY: u8 = 0;
pub const ICMP_TYPE_DEST_UNREACHABLE: u8 = 3;
pub const ICMP_TYPE_ECHO_REQUEST: u8 = 8;
pub const ICMP_TYPE_TIME_EXCEEDED: u8 = 11;

#[derive(Clone, Copy)]
struct IcmpDemuxEntry {
    identifier: u16,
    sock_idx: u32,
}

pub struct IcmpDemuxTable {
    entries: [Option<IcmpDemuxEntry>; MAX_SOCKETS],
}

impl IcmpDemuxTable {
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_SOCKETS],
        }
    }

    pub fn register(
        &mut self,
        identifier: u16,
        sock_idx: u32,
        reuse_addr: bool,
    ) -> Result<(), NetError> {
        for slot in &mut self.entries {
            if let Some(entry) = slot
                && entry.identifier == identifier
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
                *slot = Some(IcmpDemuxEntry {
                    identifier,
                    sock_idx,
                });
                return Ok(());
            }
        }

        Err(NetError::NoBufferSpace)
    }

    pub fn unregister(&mut self, identifier: u16, sock_idx: u32) {
        for slot in &mut self.entries {
            if let Some(entry) = slot
                && entry.identifier == identifier
                && entry.sock_idx == sock_idx
            {
                *slot = None;
            }
        }
    }

    pub fn lookup(&self, identifier: u16) -> Option<u32> {
        for entry in self.entries.iter().flatten() {
            if entry.identifier == identifier {
                return Some(entry.sock_idx);
            }
        }
        None
    }

    pub fn clear(&mut self) {
        self.entries = [None; MAX_SOCKETS];
    }
}

pub static ICMP_DEMUX: SpinLock<IcmpDemuxTable> = SpinLock::new(
    IcmpDemuxTable::new(),
    lock_class!("ICMP_DEMUX", LOCK_LEVEL_REGISTRY),
);

pub fn icmp_checksum(data: &[u8]) -> u16 {
    super::checksum::internet_checksum(data)
}

pub fn handle_rx(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let icmp = pkt.payload();
    if icmp.len() < ICMP_HEADER_LEN {
        klog_debug!("icmp: drop short packet len={}", icmp.len());
        return;
    }

    let icmp_type = icmp[0];
    let code = icmp[1];
    let _rx_checksum = u16::from_be_bytes([icmp[2], icmp[3]]);
    let identifier = u16::from_be_bytes([icmp[4], icmp[5]]);
    let sequence = u16::from_be_bytes([icmp[6], icmp[7]]);
    let payload = &icmp[ICMP_HEADER_LEN..];

    if icmp_checksum(icmp) != 0 {
        klog_debug!(
            "icmp: drop bad checksum from {}.{}.{}.{}",
            src_ip[0],
            src_ip[1],
            src_ip[2],
            src_ip[3]
        );
        return;
    }

    match icmp_type {
        ICMP_TYPE_ECHO_REQUEST => {
            if code != 0 {
                klog_debug!("icmp: drop echo request with code {}", code);
                return;
            }
            klog_debug!(
                "icmp: echo request from {}.{}.{}.{} id={} seq={}",
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3],
                identifier,
                sequence
            );
            if let Err(err) = send_echo_reply(src_ip, dst_ip, identifier, sequence, payload) {
                klog_debug!("icmp: echo reply send failed: {}", err);
            }
        }
        ICMP_TYPE_ECHO_REPLY => {
            if code != 0 {
                klog_debug!("icmp: drop echo reply with code {}", code);
                return;
            }
            // Passive connectivity evidence, recorded before the demux: a reply
            // from the gateway confirms the first hop whether it was answering
            // this stack's own probe or somebody's `ping`, and a reply that
            // belongs to no socket is dropped a few lines below. Two atomic
            // loads and a store — nothing here takes a lock, which is what
            // makes it legal on the receive path.
            crate::connectivity::note_gateway_answer(crate::types::Ipv4Addr(src_ip));
            // An echo reply from off-link is direct proof the path beyond the
            // gateway works — the same class of evidence as a TCP connection
            // reaching ESTABLISHED.
            crate::connectivity::note_wan_peer(crate::types::Ipv4Addr(src_ip));
            klog_debug!(
                "icmp: echo reply from {}.{}.{}.{} id={} seq={}",
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3],
                identifier,
                sequence
            );

            let sock_idx = ICMP_DEMUX.lock().lookup(identifier);
            if let Some(sock_idx) = sock_idx {
                super::socket::socket_deliver_icmp(sock_idx, src_ip, icmp);
                return;
            }

            klog_debug!(
                "icmp: drop echo reply no socket for identifier {}",
                identifier
            );
        }
        _ => {
            klog_debug!("icmp: unhandled type {} code {}", icmp_type, code);
        }
    }
}

fn send_echo_reply(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Result<usize, NetError> {
    send_echo(
        ICMP_TYPE_ECHO_REPLY,
        dst_ip,
        src_ip,
        identifier,
        sequence,
        payload,
    )
}

pub fn send_echo_request(
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Result<usize, NetError> {
    let src_ip = crate::iface::source_ip_for(crate::types::Ipv4Addr(dst_ip))
        .map(|ip| ip.0)
        .unwrap_or([0; 4]);
    send_echo(
        ICMP_TYPE_ECHO_REQUEST,
        src_ip,
        dst_ip,
        identifier,
        sequence,
        payload,
    )
}

fn send_echo(
    icmp_type: u8,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> Result<usize, NetError> {
    if payload.len() > 1472 {
        return Err(NetError::InvalidArgument);
    }

    let mut pkt = PacketBuf::alloc().ok_or(NetError::NoBufferSpace)?;
    pkt.append(payload)?;

    let icmp_len = ICMP_HEADER_LEN + payload.len();
    {
        let icmp_hdr = pkt.push_header(ICMP_HEADER_LEN)?;
        icmp_hdr[0] = icmp_type;
        icmp_hdr[1] = 0;
        icmp_hdr[2..4].copy_from_slice(&0u16.to_be_bytes());
        icmp_hdr[4..6].copy_from_slice(&identifier.to_be_bytes());
        icmp_hdr[6..8].copy_from_slice(&sequence.to_be_bytes());
    }

    pkt.prepend_ipv4(src_ip, dst_ip, super::IpProtocol::Icmp.as_u8(), icmp_len)?;

    let src_mac = crate::net_driver_service::net_driver()
        .and_then(|d| (d.virtio_net_mac)())
        .unwrap_or([0; 6]);
    pkt.prepend_eth(src_mac, super::MacAddr::BROADCAST.0)?;
    pkt.set_ipv4_offsets();

    let icmp_start = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
    let frame = pkt.payload_mut();
    let checksum = icmp_checksum(&frame[icmp_start..]);
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&checksum.to_be_bytes());

    klog_debug!(
        "icmp: tx type={} dst={}.{}.{}.{} id={} seq={} len={}",
        icmp_type,
        dst_ip[0],
        dst_ip[1],
        dst_ip[2],
        dst_ip[3],
        identifier,
        sequence,
        payload.len()
    );

    super::ipv4::send(Ipv4Addr(dst_ip), pkt).map_err(|_| NetError::NetworkUnreachable)?;
    Ok(payload.len())
}

/// Single-direct-copy [`send_echo_request`]: the echo payload is volatile-copied
/// **once**, straight from the pinned user pages (via `reader`) into the packet
/// buffer — no kernel staging scratch. The checksum is computed over the
/// in-packet bytes. Payload length is `reader.remain()`.
pub fn send_echo_request_from(
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    reader: &mut slopos_ostd::mm::VmReader<'_>,
) -> Result<usize, NetError> {
    let src_ip = crate::iface::source_ip_for(crate::types::Ipv4Addr(dst_ip))
        .map(|ip| ip.0)
        .unwrap_or([0; 4]);

    let payload_len = reader.remain();
    if payload_len > 1472 {
        return Err(NetError::InvalidArgument);
    }

    let mut pkt = PacketBuf::alloc().ok_or(NetError::NoBufferSpace)?;
    let copied = pkt.append_from(reader, payload_len)?;

    let icmp_len = ICMP_HEADER_LEN + copied;
    {
        let icmp_hdr = pkt.push_header(ICMP_HEADER_LEN)?;
        icmp_hdr[0] = ICMP_TYPE_ECHO_REQUEST;
        icmp_hdr[1] = 0;
        icmp_hdr[2..4].copy_from_slice(&0u16.to_be_bytes());
        icmp_hdr[4..6].copy_from_slice(&identifier.to_be_bytes());
        icmp_hdr[6..8].copy_from_slice(&sequence.to_be_bytes());
    }

    pkt.prepend_ipv4(src_ip, dst_ip, super::IpProtocol::Icmp.as_u8(), icmp_len)?;

    let src_mac = crate::net_driver_service::net_driver()
        .and_then(|d| (d.virtio_net_mac)())
        .unwrap_or([0; 6]);
    pkt.prepend_eth(src_mac, super::MacAddr::BROADCAST.0)?;
    pkt.set_ipv4_offsets();

    let icmp_start = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
    let frame = pkt.payload_mut();
    let checksum = icmp_checksum(&frame[icmp_start..]);
    frame[icmp_start + 2..icmp_start + 4].copy_from_slice(&checksum.to_be_bytes());

    super::ipv4::send(Ipv4Addr(dst_ip), pkt).map_err(|_| NetError::NetworkUnreachable)?;
    Ok(copied)
}

/// True NIC-DMA zero-copy echo request (SlopRing `OP_SEND_ZC`): the
/// echo payload is **never** copied into a packet buffer — the NIC DMAs it
/// straight from the pinned user pages (`runs`). Only the 42-byte header is
/// built. ICMP has no pseudo-header and QEMU virtio-net does not offload ICMP
/// checksums, so the checksum is computed over the 8-byte ICMP header + the
/// payload read **once** via `reader` (a single volatile pass, no staging copy)
/// and written into the header; the device does no checksum work (`csum=None`).
///
/// Eligibility mirrors [`crate::udp::udp_sendto_zerocopy`] (unicast, route,
/// resolved neighbor, size); a miss returns [`ZcSendOutcome::NotEligible`] so
/// the caller falls back to the single-copy leaf.
pub fn send_echo_request_zerocopy(
    dst_ip: [u8; 4],
    identifier: u16,
    sequence: u16,
    runs: &[(u64, u32)],
    reader: &mut slopos_ostd::mm::VmReader<'_>,
    total_len: usize,
    keepalive: slopos_ostd::KVec<
        slopos_ostd::mm::uframe::UFrame<slopos_ostd::mm::frame::AnonymousMeta>,
    >,
    token: slopos_ostd::TxReclaimToken,
) -> crate::socket::ZcSendOutcome {
    use super::netdev::DEVICE_REGISTRY;
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
        return ZcSendOutcome::NotEligible;
    };
    let Some(src_mac) = DEVICE_REGISTRY.mac_by_index(dev) else {
        return ZcSendOutcome::NotEligible;
    };
    let src_ip = crate::iface::source_ip_for(Ipv4Addr(dst_ip))
        .map(|ip| ip.0)
        .unwrap_or([0; 4]);

    let icmp_len = ICMP_HEADER_LEN + total_len;
    let ip_total = super::IPV4_HEADER_LEN + icmp_len;
    let mut hdr = [0u8; super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN + ICMP_HEADER_LEN];
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
        ip[9] = super::IpProtocol::Icmp.as_u8();
        ip[10..12].copy_from_slice(&[0; 2]);
        ip[12..16].copy_from_slice(&src_ip);
        ip[16..20].copy_from_slice(&dst_ip);
        let ip_csum = super::checksum::internet_checksum(ip);
        ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());
    }
    // ICMP (34..42): header with checksum=0, then compute over header + payload.
    let l4 = super::ETH_HEADER_LEN + super::IPV4_HEADER_LEN;
    {
        let icmp = &mut hdr[l4..l4 + ICMP_HEADER_LEN];
        icmp[0] = ICMP_TYPE_ECHO_REQUEST;
        icmp[1] = 0;
        icmp[2..4].copy_from_slice(&0u16.to_be_bytes());
        icmp[4..6].copy_from_slice(&identifier.to_be_bytes());
        icmp[6..8].copy_from_slice(&sequence.to_be_bytes());
    }
    let mut sum = super::checksum::ones_complement_sum(&hdr[l4..l4 + ICMP_HEADER_LEN]);
    sum = sum.wrapping_add(super::checksum::ones_complement_sum_reader(
        reader, total_len,
    ));
    let icmp_csum = super::checksum::fold(sum);
    hdr[l4 + 2..l4 + 4].copy_from_slice(&icmp_csum.to_be_bytes());

    match DEVICE_REGISTRY.tx_zerocopy_by_index(dev, &hdr, runs, None, keepalive, token) {
        Ok(()) => ZcSendOutcome::Submitted(total_len),
        Err(NetError::NoBufferSpace) => ZcSendOutcome::WouldBlock,
        Err(_) => ZcSendOutcome::NotEligible,
    }
}

pub fn icmp_bind(sock_idx: u32, identifier: u16, reuse_addr: bool) -> Result<(), NetError> {
    ICMP_DEMUX.lock().register(identifier, sock_idx, reuse_addr)
}

pub fn icmp_unbind(sock_idx: u32, identifier: u16) {
    ICMP_DEMUX.lock().unregister(identifier, sock_idx);
}
