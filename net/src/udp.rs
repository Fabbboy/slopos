use slopos_abi::net::MAX_SOCKETS;
use slopos_lib::{IrqMutex, klog_debug};

use super::packetbuf::PacketBuf;
use super::types::{Ipv4Addr, NetError, Port};

#[derive(Clone, Copy)]
struct UdpDemuxEntry {
    local_ip: Ipv4Addr,
    local_port: Port,
    sock_idx: u32,
}

pub struct UdpDemuxTable {
    entries: [Option<UdpDemuxEntry>; MAX_SOCKETS],
}

impl UdpDemuxTable {
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_SOCKETS],
        }
    }

    pub fn register(
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

    pub fn unregister(&mut self, local_ip: Ipv4Addr, local_port: Port, sock_idx: u32) {
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

    pub fn lookup(&self, dst_ip: Ipv4Addr, dst_port: Port) -> Option<u32> {
        for entry in self.entries.iter().flatten() {
            if entry.local_ip == dst_ip && entry.local_port == dst_port {
                return Some(entry.sock_idx);
            }
        }

        for entry in self.entries.iter().flatten() {
            if entry.local_ip == Ipv4Addr::UNSPECIFIED && entry.local_port == dst_port {
                return Some(entry.sock_idx);
            }
        }

        None
    }

    pub fn clear(&mut self) {
        self.entries = [None; MAX_SOCKETS];
    }
}

pub static UDP_DEMUX: IrqMutex<UdpDemuxTable> = IrqMutex::new(UdpDemuxTable::new());

pub fn handle_rx(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let Some((src_port, dst_port, udp_payload)) = super::parse_udp_header(pkt.payload()) else {
        return;
    };

    if src_port == super::dns::DNS_PORT {
        crate::driver_hooks::dns_intercept_response(udp_payload);
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

    pkt.prepend_ipv4(local_ip, dst_ip, super::IPPROTO_UDP, udp_len)?;

    let src_mac = crate::driver_hooks::virtio_net_mac().unwrap_or([0; 6]);
    pkt.prepend_eth(src_mac, super::ETH_BROADCAST)?;
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
