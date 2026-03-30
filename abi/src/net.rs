#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetMember {
    pub ipv4: [u8; 4],
    pub mac: [u8; 6],
    pub flags: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetInfo {
    pub ipv4: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub mac: [u8; 6],
    pub mtu: u16,
    pub link_up: u8,
    pub nic_ready: u8,
    pub _pad: [u8; 2],
}

pub const USER_NET_MEMBER_FLAG_ARP: u16 = 1 << 0;
pub const USER_NET_MEMBER_FLAG_IPV4: u16 = 1 << 1;

pub const USER_NET_MAX_MEMBERS: usize = 32;

// =============================================================================
// Socket ABI types
// =============================================================================

/// Address family: Unix domain sockets.
pub const AF_UNIX: u16 = 1;

/// Address family: IPv4 Internet protocols.
pub const AF_INET: u16 = 2;

/// Socket type: byte-stream (TCP).
pub const SOCK_STREAM: u16 = 1;
/// Socket type: datagram (UDP).
pub const SOCK_DGRAM: u16 = 2;
/// Socket type: raw (ICMP).
pub const SOCK_RAW: u16 = 3;

/// IP protocol: ICMP.
pub const IPPROTO_ICMP: u16 = 1;

/// IPv4 socket address — mirrors POSIX `sockaddr_in` layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SockAddrIn {
    pub family: u16,
    /// Port in **network** byte order (big-endian).
    pub port: u16,
    /// IPv4 address in network byte order.
    pub addr: [u8; 4],
    pub _pad: [u8; 8],
}

const _: () = assert!(
    core::mem::size_of::<SockAddrIn>() == 16,
    "SockAddrIn must be exactly 16 bytes"
);

/// Maximum number of kernel sockets (shared across all processes).
pub const MAX_SOCKETS: usize = 64;

/// Socket descriptor index indicating "no socket".
pub const INVALID_SOCKET_IDX: u32 = u32::MAX;
