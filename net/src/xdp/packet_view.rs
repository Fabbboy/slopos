//! Safe view over a received frame for XDP filters.

use crate::checksum;
use crate::packetbuf::PacketBuf;
use crate::tcp::checksum as tcp_checksum;
use crate::tcp::header::{TcpHeader, parse_header};
use crate::types::{EtherType, IpProtocol, Ipv4Addr, MacAddr};
use crate::{ETH_HEADER_LEN, IPV4_HEADER_LEN};

#[derive(Clone, Copy, Debug)]
pub struct EthernetView {
    pub dst: MacAddr,
    pub src: MacAddr,
    /// Host order.
    pub ethertype: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Ipv4View {
    /// In bytes (IHL × 4).
    pub header_len: usize,
    pub protocol: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
}

#[derive(Clone, Copy, Debug)]
pub struct UdpView {
    /// Host order.
    pub src_port: u16,
    /// Host order.
    pub dst_port: u16,
    /// Host order.
    pub length: u16,
}

pub struct PacketView<'a> {
    pkt: &'a mut PacketBuf,
}

impl<'a> PacketView<'a> {
    /// The packet's active region must be the full L2 frame (the case at the
    /// ingress hook).
    #[inline]
    pub fn new(pkt: &'a mut PacketBuf) -> Self {
        Self { pkt }
    }

    #[inline]
    pub fn frame(&self) -> &[u8] {
        self.pkt.payload()
    }

    #[inline]
    pub fn frame_mut(&mut self) -> &mut [u8] {
        self.pkt.payload_mut()
    }

    /// Alias for [`frame_mut`](Self::frame_mut).
    #[inline]
    pub fn payload_mut(&mut self) -> &mut [u8] {
        self.pkt.payload_mut()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.pkt.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pkt.is_empty()
    }

    /// Parse the Ethernet header. Returns `None` if the frame is too short.
    pub fn ethernet(&self) -> Option<EthernetView> {
        let f = self.frame();
        if f.len() < ETH_HEADER_LEN {
            return None;
        }
        Some(EthernetView {
            dst: MacAddr([f[0], f[1], f[2], f[3], f[4], f[5]]),
            src: MacAddr([f[6], f[7], f[8], f[9], f[10], f[11]]),
            ethertype: u16::from_be_bytes([f[12], f[13]]),
        })
    }

    /// Parse the IPv4 header. Returns `None` unless the frame carries a
    /// well-formed IPv4 packet (version 4, IHL ≥ 5, header present).
    pub fn ipv4(&self) -> Option<Ipv4View> {
        let eth = self.ethernet()?;
        if eth.ethertype != EtherType::Ipv4.as_u16() {
            return None;
        }
        let ip = self.frame().get(ETH_HEADER_LEN..)?;
        if ip.len() < IPV4_HEADER_LEN || (ip[0] >> 4) != 4 {
            return None;
        }
        let header_len = ((ip[0] & 0x0F) as usize) * 4;
        if header_len < IPV4_HEADER_LEN || ip.len() < header_len {
            return None;
        }
        Some(Ipv4View {
            header_len,
            protocol: ip[9],
            src: Ipv4Addr([ip[12], ip[13], ip[14], ip[15]]),
            dst: Ipv4Addr([ip[16], ip[17], ip[18], ip[19]]),
        })
    }

    /// Parse the TCP header (over IPv4). Returns `None` unless the packet is
    /// IPv4/TCP with a well-formed TCP header.
    pub fn tcp(&self) -> Option<TcpHeader> {
        let ip = self.ipv4()?;
        if ip.protocol != IpProtocol::Tcp.as_u8() {
            return None;
        }
        let l4 = self.frame().get(ETH_HEADER_LEN + ip.header_len..)?;
        parse_header(l4)
    }

    /// Parse the UDP header (over IPv4). Returns `None` unless the packet is
    /// IPv4/UDP with at least an 8-byte UDP header.
    pub fn udp(&self) -> Option<UdpView> {
        let ip = self.ipv4()?;
        if ip.protocol != IpProtocol::Udp.as_u8() {
            return None;
        }
        let l4 = self.frame().get(ETH_HEADER_LEN + ip.header_len..)?;
        if l4.len() < 8 {
            return None;
        }
        Some(UdpView {
            src_port: u16::from_be_bytes([l4[0], l4[1]]),
            dst_port: u16::from_be_bytes([l4[2], l4[3]]),
            length: u16::from_be_bytes([l4[4], l4[5]]),
        })
    }

    /// Recompute the IPv4 header checksum in place. Call after mutating any
    /// IPv4 header field. Returns `false` if the frame is not IPv4.
    pub fn recompute_ipv4_checksum(&mut self) -> bool {
        let Some(ip) = self.ipv4() else {
            return false;
        };
        let end = ETH_HEADER_LEN + ip.header_len;
        let frame = self.frame_mut();
        if frame.len() < end {
            return false;
        }
        let header = &mut frame[ETH_HEADER_LEN..end];
        header[10] = 0;
        header[11] = 0;
        let csum = checksum::internet_checksum(header);
        header[10..12].copy_from_slice(&csum.to_be_bytes());
        true
    }

    /// Recompute the TCP checksum in place over the pseudo-header + segment.
    /// Call after mutating TCP header fields or payload. Returns `false` if the
    /// frame is not IPv4/TCP.
    pub fn recompute_tcp_checksum(&mut self) -> bool {
        let Some(ip) = self.ipv4() else {
            return false;
        };
        if ip.protocol != IpProtocol::Tcp.as_u8() {
            return false;
        }
        let l4_start = ETH_HEADER_LEN + ip.header_len;
        let (src, dst) = (ip.src.0, ip.dst.0);
        let frame = self.frame_mut();
        if frame.len() <= l4_start + 18 {
            return false;
        }
        frame[l4_start + 16] = 0;
        frame[l4_start + 17] = 0;
        let csum = tcp_checksum::tcp_checksum(src, dst, &frame[l4_start..]);
        frame[l4_start + 16..l4_start + 18].copy_from_slice(&csum.to_be_bytes());
        true
    }
}
