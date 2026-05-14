use slopos_abi::net::MAX_SOCKETS;
use slopos_ostd::klog_debug;
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

pub static ICMP_DEMUX: SpinLock<IcmpDemuxTable> =
    SpinLock::new(IcmpDemuxTable::new(), LOCK_LEVEL_REGISTRY);

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
    let src_ip = crate::netstack::NET_STACK
        .first_ipv4()
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

pub fn icmp_bind(sock_idx: u32, identifier: u16, reuse_addr: bool) -> Result<(), NetError> {
    ICMP_DEMUX.lock().register(identifier, sock_idx, reuse_addr)
}

pub fn icmp_unbind(sock_idx: u32, identifier: u16) {
    ICMP_DEMUX.lock().unregister(identifier, sock_idx);
}
