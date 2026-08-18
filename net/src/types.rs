//! Type-safe network primitives for the SlopOS networking stack: newtype
//! wrappers for addresses, ports and protocol fields.

use core::fmt;

use slopos_abi::net::{AF_INET, SockAddrIn};

/// IPv4 address stored in **network byte order** (`[u8; 4]`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0, 0, 0, 0]);
    pub const BROADCAST: Self = Self([255, 255, 255, 255]);
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);

    #[inline]
    pub const fn from_u32_be(val: u32) -> Self {
        Self(val.to_be_bytes())
    }

    #[inline]
    pub const fn to_u32_be(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// The network address of `self` under a `prefix_len`-bit mask.
    ///
    /// Prefixes of 0 and 32 are special-cased: shifting a `u32` by 32 overflows.
    #[inline]
    pub const fn masked(self, prefix_len: u8) -> Self {
        if prefix_len == 0 {
            return Self::UNSPECIFIED;
        }
        if prefix_len >= 32 {
            return self;
        }
        Self::from_u32_be(self.to_u32_be() & (u32::MAX << (32 - prefix_len)))
    }

    /// `true` if the address is in the `127.0.0.0/8` loopback range.
    #[inline]
    pub const fn is_loopback(&self) -> bool {
        self.0[0] == 127
    }

    #[inline]
    pub const fn is_broadcast(&self) -> bool {
        self.0[0] == 255 && self.0[1] == 255 && self.0[2] == 255 && self.0[3] == 255
    }

    /// `true` if the address is in the multicast range `224.0.0.0/4`.
    #[inline]
    pub const fn is_multicast(&self) -> bool {
        self.0[0] >= 224 && self.0[0] <= 239
    }

    #[inline]
    pub const fn is_unspecified(&self) -> bool {
        self.0[0] == 0 && self.0[1] == 0 && self.0[2] == 0 && self.0[3] == 0
    }

    #[inline]
    pub const fn in_subnet(addr: Ipv4Addr, network: Ipv4Addr, mask: Ipv4Addr) -> bool {
        let a = addr.to_u32_be();
        let n = network.to_u32_be();
        let m = mask.to_u32_be();
        (a & m) == (n & m)
    }

    #[inline]
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }
}

impl fmt::Debug for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

/// Port number in **host byte order**.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Port(pub u16);

impl Port {
    #[inline]
    pub const fn new(val: u16) -> Self {
        Self(val)
    }

    #[inline]
    pub const fn to_network_bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }

    #[inline]
    pub const fn from_network_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_be_bytes(bytes))
    }

    /// `true` if the port is in the IANA ephemeral range (49152–65535).
    #[inline]
    pub const fn is_ephemeral(&self) -> bool {
        self.0 >= 49152
    }

    /// `true` if the port is in the privileged / well-known range (0–1023).
    #[inline]
    pub const fn is_privileged(&self) -> bool {
        self.0 < 1024
    }

    #[inline]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Port({})", self.0)
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: Self = Self([0xff; 6]);
    pub const ZERO: Self = Self([0; 6]);

    #[inline]
    pub const fn is_broadcast(&self) -> bool {
        self.0[0] == 0xff
            && self.0[1] == 0xff
            && self.0[2] == 0xff
            && self.0[3] == 0xff
            && self.0[4] == 0xff
            && self.0[5] == 0xff
    }

    #[inline]
    pub const fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }

    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.0[0] == 0
            && self.0[1] == 0
            && self.0[2] == 0
            && self.0[3] == 0
            && self.0[4] == 0
            && self.0[5] == 0
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Device index — uniquely identifies a registered network device.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevIndex(pub usize);

impl fmt::Debug for DevIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevIndex({})", self.0)
    }
}

impl fmt::Display for DevIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Kernel-internal network error, converted to a POSIX errno at the syscall
/// boundary via [`to_errno`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    WouldBlock,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    TimedOut,
    AddressInUse,
    AddressNotAvailable,
    NotConnected,
    AlreadyConnected,
    NetworkUnreachable,
    HostUnreachable,
    PermissionDenied,
    InvalidArgument,
    NoBufferSpace,
    ProtocolNotSupported,
    AddressFamilyNotSupported,
    SocketNotBound,
    InProgress,
    OperationNotSupported,
    /// Write after shutdown.
    Shutdown,
}

impl slopos_abi::KernelErrno for NetError {
    fn to_errno(&self) -> i32 {
        use slopos_abi::syscall::*;
        match self {
            Self::WouldBlock => ERRNO_EAGAIN as i32,
            Self::ConnectionRefused => ERRNO_ECONNREFUSED as i32,
            Self::ConnectionReset => ERRNO_ECONNRESET as i32,
            Self::ConnectionAborted => ERRNO_ECONNABORTED as i32,
            Self::TimedOut => ERRNO_ETIMEDOUT as i32,
            Self::AddressInUse => ERRNO_EADDRINUSE as i32,
            Self::AddressNotAvailable => ERRNO_EADDRNOTAVAIL as i32,
            Self::NotConnected => ERRNO_ENOTCONN as i32,
            Self::AlreadyConnected => ERRNO_EISCONN as i32,
            Self::NetworkUnreachable => ERRNO_ENETUNREACH as i32,
            Self::HostUnreachable => ERRNO_EHOSTUNREACH as i32,
            Self::PermissionDenied => ERRNO_EPERM as i32,
            Self::InvalidArgument => ERRNO_EINVAL as i32,
            Self::NoBufferSpace => ERRNO_ENOBUFS as i32,
            Self::ProtocolNotSupported => ERRNO_EPROTONOSUPPORT as i32,
            Self::AddressFamilyNotSupported => ERRNO_EAFNOSUPPORT as i32,
            Self::SocketNotBound => ERRNO_EINVAL as i32,
            Self::InProgress => ERRNO_EINPROGRESS as i32,
            Self::OperationNotSupported => ERRNO_EOPNOTSUPP as i32,
            Self::Shutdown => ERRNO_EPIPE as i32,
        }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldBlock => write!(f, "operation would block"),
            Self::ConnectionRefused => write!(f, "connection refused"),
            Self::ConnectionReset => write!(f, "connection reset by peer"),
            Self::ConnectionAborted => write!(f, "connection aborted"),
            Self::TimedOut => write!(f, "operation timed out"),
            Self::AddressInUse => write!(f, "address already in use"),
            Self::AddressNotAvailable => write!(f, "address not available"),
            Self::NotConnected => write!(f, "socket not connected"),
            Self::AlreadyConnected => write!(f, "socket already connected"),
            Self::NetworkUnreachable => write!(f, "network unreachable"),
            Self::HostUnreachable => write!(f, "host unreachable"),
            Self::PermissionDenied => write!(f, "permission denied"),
            Self::InvalidArgument => write!(f, "invalid argument"),
            Self::NoBufferSpace => write!(f, "no buffer space available"),
            Self::ProtocolNotSupported => write!(f, "protocol not supported"),
            Self::AddressFamilyNotSupported => write!(f, "address family not supported"),
            Self::SocketNotBound => write!(f, "socket not bound"),
            Self::InProgress => write!(f, "operation in progress"),
            Self::OperationNotSupported => write!(f, "operation not supported"),
            Self::Shutdown => write!(f, "broken pipe (shutdown)"),
        }
    }
}

/// Kernel-internal socket address, and the single conversion point to and from
/// the userspace-visible [`SockAddrIn`] layout.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SockAddr {
    pub ip: Ipv4Addr,
    pub port: Port,
}

impl SockAddr {
    #[inline]
    pub const fn new(ip: Ipv4Addr, port: Port) -> Self {
        Self { ip, port }
    }

    /// Parse from a userspace [`SockAddrIn`], validating `sin_family == AF_INET`.
    pub fn from_user(raw: &SockAddrIn) -> Result<Self, NetError> {
        if raw.family != AF_INET {
            return Err(NetError::AddressFamilyNotSupported);
        }
        Ok(Self {
            ip: Ipv4Addr(raw.addr),
            port: Port(u16::from_be(raw.port)),
        })
    }

    pub fn to_user(&self) -> SockAddrIn {
        SockAddrIn {
            family: AF_INET,
            port: self.port.0.to_be(),
            addr: self.ip.0,
            _pad: [0; 8],
        }
    }
}

impl fmt::Debug for SockAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

impl fmt::Display for SockAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum EtherType {
    Ipv4 = 0x0800,
    Arp = 0x0806,
}

impl EtherType {
    #[inline]
    pub const fn from_u16(val: u16) -> Option<Self> {
        match val {
            0x0800 => Some(Self::Ipv4),
            0x0806 => Some(Self::Arp),
            _ => None,
        }
    }

    #[inline]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    #[inline]
    pub const fn to_be_bytes(self) -> [u8; 2] {
        (self as u16).to_be_bytes()
    }
}

impl fmt::Display for EtherType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4 => write!(f, "IPv4"),
            Self::Arp => write!(f, "ARP"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum IpProtocol {
    Icmp = 1,
    Tcp = 6,
    Udp = 17,
}

impl IpProtocol {
    #[inline]
    pub const fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Icmp),
            6 => Some(Self::Tcp),
            17 => Some(Self::Udp),
            _ => None,
        }
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for IpProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Icmp => write!(f, "ICMP"),
            Self::Tcp => write!(f, "TCP"),
            Self::Udp => write!(f, "UDP"),
        }
    }
}
