//! Type-safe network primitives for the SlopOS networking stack.
//!
//! This module provides newtype wrappers that eliminate entire classes of bugs
//! at compile time: byte-order mixups, address/port confusion, and raw numeric
//! comparisons for protocol fields.  All types are zero-cost (`#[repr(transparent)]`)
//! and designed for a `#![no_std]` kernel environment.

use core::fmt;

use slopos_abi::net::{AF_INET, SockAddrIn};

// =============================================================================
// 1A.1 — Newtype wrappers
// =============================================================================

/// IPv4 address stored in **network byte order** (`[u8; 4]`).
///
/// The inner representation is always big-endian, matching the wire format.
/// Conversion to/from host-order `u32` is explicit via [`from_u32_be`] / [`to_u32_be`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    /// `0.0.0.0` — the unspecified address (bind to any interface).
    pub const UNSPECIFIED: Self = Self([0, 0, 0, 0]);
    /// `255.255.255.255` — the limited broadcast address.
    pub const BROADCAST: Self = Self([255, 255, 255, 255]);
    /// `127.0.0.1` — the loopback address.
    pub const LOCALHOST: Self = Self([127, 0, 0, 1]);

    /// Construct from a big-endian `u32`.
    #[inline]
    pub const fn from_u32_be(val: u32) -> Self {
        Self(val.to_be_bytes())
    }

    /// Return the address as a big-endian `u32`.
    #[inline]
    pub const fn to_u32_be(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// `true` if the address is in the `127.0.0.0/8` loopback range.
    #[inline]
    pub const fn is_loopback(&self) -> bool {
        self.0[0] == 127
    }

    /// `true` if the address is `255.255.255.255`.
    #[inline]
    pub const fn is_broadcast(&self) -> bool {
        self.0[0] == 255 && self.0[1] == 255 && self.0[2] == 255 && self.0[3] == 255
    }

    /// `true` if the address is in the multicast range `224.0.0.0/4`.
    #[inline]
    pub const fn is_multicast(&self) -> bool {
        self.0[0] >= 224 && self.0[0] <= 239
    }

    /// `true` if the address is `0.0.0.0`.
    #[inline]
    pub const fn is_unspecified(&self) -> bool {
        self.0[0] == 0 && self.0[1] == 0 && self.0[2] == 0 && self.0[3] == 0
    }

    /// `true` if `addr` falls within the subnet defined by `self` and `mask`.
    ///
    /// Both `addr` and `mask` are in network byte order.
    #[inline]
    pub const fn in_subnet(addr: Ipv4Addr, network: Ipv4Addr, mask: Ipv4Addr) -> bool {
        let a = addr.to_u32_be();
        let n = network.to_u32_be();
        let m = mask.to_u32_be();
        (a & m) == (n & m)
    }

    /// Convert from a raw `[u8; 4]` (already in network byte order).
    #[inline]
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    /// Return the raw bytes in network byte order.
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
///
/// Conversion to/from network (big-endian) byte order is explicit via
/// [`to_network_bytes`] / [`from_network_bytes`].  This prevents accidentally
/// passing a host-order value where network-order is expected.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Port(pub u16);

impl Port {
    /// Construct a port from a host-order `u16`.
    #[inline]
    pub const fn new(val: u16) -> Self {
        Self(val)
    }

    /// Serialize to big-endian bytes for the wire.
    #[inline]
    pub const fn to_network_bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }

    /// Deserialize from big-endian wire bytes.
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

    /// Return the raw host-order `u16` value.
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

/// Ethernet MAC address (6 bytes).
///
/// Distinct type prevents confusion with other 6-byte arrays.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// `ff:ff:ff:ff:ff:ff` — the broadcast address.
    pub const BROADCAST: Self = Self([0xff; 6]);
    /// `00:00:00:00:00:00` — the zero / unset address.
    pub const ZERO: Self = Self([0; 6]);

    /// `true` if the address is `ff:ff:ff:ff:ff:ff`.
    #[inline]
    pub const fn is_broadcast(&self) -> bool {
        self.0[0] == 0xff
            && self.0[1] == 0xff
            && self.0[2] == 0xff
            && self.0[3] == 0xff
            && self.0[4] == 0xff
            && self.0[5] == 0xff
    }

    /// `true` if the least-significant bit of the first octet is set (multicast).
    #[inline]
    pub const fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }

    /// `true` if the address is all zeros.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.0[0] == 0
            && self.0[1] == 0
            && self.0[2] == 0
            && self.0[3] == 0
            && self.0[4] == 0
            && self.0[5] == 0
    }

    /// Return the raw bytes.
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
///
/// Cannot be confused with a socket index, connection ID, or other `usize`.
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

// =============================================================================
// 1A.2 — NetError
// =============================================================================

/// Comprehensive network error type.
///
/// Internal code uses `NetError` exclusively.  Conversion to POSIX errno
/// happens at the syscall boundary via [`to_errno`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Operation would block (EAGAIN / EWOULDBLOCK).
    WouldBlock,
    /// Connection refused by remote host (ECONNREFUSED).
    ConnectionRefused,
    /// Connection reset by remote host (ECONNRESET).
    ConnectionReset,
    /// Connection aborted locally (ECONNABORTED).
    ConnectionAborted,
    /// Operation timed out (ETIMEDOUT).
    TimedOut,
    /// Address already in use (EADDRINUSE).
    AddressInUse,
    /// Requested address not available on this host (EADDRNOTAVAIL).
    AddressNotAvailable,
    /// Socket is not connected (ENOTCONN).
    NotConnected,
    /// Socket is already connected (EISCONN).
    AlreadyConnected,
    /// Network is unreachable (ENETUNREACH).
    NetworkUnreachable,
    /// Host is unreachable (EHOSTUNREACH).
    HostUnreachable,
    /// Permission denied (EPERM).
    PermissionDenied,
    /// Invalid argument (EINVAL).
    InvalidArgument,
    /// No buffer space available (ENOBUFS).
    NoBufferSpace,
    /// Protocol not supported (EPROTONOSUPPORT).
    ProtocolNotSupported,
    /// Address family not supported (EAFNOSUPPORT).
    AddressFamilyNotSupported,
    /// Socket not bound — `bind()` was not called (EINVAL).
    SocketNotBound,
    /// Non-blocking connect in progress (EINPROGRESS).
    InProgress,
    /// Operation not supported on this socket type (EOPNOTSUPP).
    OperationNotSupported,
    /// Write after shutdown (EPIPE).
    Shutdown,
}

impl NetError {
    /// Convert to a POSIX errno value (negative) for the syscall boundary.
    ///
    /// Each variant maps to exactly one Linux errno constant.
    pub const fn to_errno(&self) -> i32 {
        match self {
            Self::WouldBlock => -11,                // EAGAIN
            Self::ConnectionRefused => -111,        // ECONNREFUSED
            Self::ConnectionReset => -104,          // ECONNRESET
            Self::ConnectionAborted => -103,        // ECONNABORTED
            Self::TimedOut => -110,                 // ETIMEDOUT
            Self::AddressInUse => -98,              // EADDRINUSE
            Self::AddressNotAvailable => -99,       // EADDRNOTAVAIL
            Self::NotConnected => -107,             // ENOTCONN
            Self::AlreadyConnected => -106,         // EISCONN
            Self::NetworkUnreachable => -101,       // ENETUNREACH
            Self::HostUnreachable => -113,          // EHOSTUNREACH
            Self::PermissionDenied => -1,           // EPERM
            Self::InvalidArgument => -22,           // EINVAL
            Self::NoBufferSpace => -105,            // ENOBUFS
            Self::ProtocolNotSupported => -93,      // EPROTONOSUPPORT
            Self::AddressFamilyNotSupported => -97, // EAFNOSUPPORT
            Self::SocketNotBound => -22,            // EINVAL (bind not called)
            Self::InProgress => -115,               // EINPROGRESS
            Self::OperationNotSupported => -95,     // EOPNOTSUPP
            Self::Shutdown => -32,                  // EPIPE
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

// =============================================================================
// 1A.3 — SockAddr
// =============================================================================

/// Kernel-internal socket address combining an [`Ipv4Addr`] and [`Port`].
///
/// This is the single conversion point between the kernel's type-safe address
/// representation and the userspace-visible [`SockAddrIn`] layout.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SockAddr {
    pub ip: Ipv4Addr,
    pub port: Port,
}

impl SockAddr {
    /// Create a new `SockAddr` from components.
    #[inline]
    pub const fn new(ip: Ipv4Addr, port: Port) -> Self {
        Self { ip, port }
    }

    /// Parse from a userspace [`SockAddrIn`], validating `sin_family == AF_INET`
    /// and converting byte order.
    pub fn from_user(raw: &SockAddrIn) -> Result<Self, NetError> {
        if raw.family != AF_INET {
            return Err(NetError::AddressFamilyNotSupported);
        }
        Ok(Self {
            ip: Ipv4Addr(raw.addr),
            // SockAddrIn.port stores htons(port) — convert back to host order.
            port: Port(u16::from_be(raw.port)),
        })
    }

    /// Serialize to the userspace-visible [`SockAddrIn`] layout.
    pub fn to_user(&self) -> SockAddrIn {
        SockAddrIn {
            family: AF_INET,
            // Store as htons(port) — big-endian u16 value.
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

// =============================================================================
// 1A.4 — EtherType and IpProtocol enums
// =============================================================================

/// Ethernet frame type field values.
///
/// Pattern matching on this enum replaces raw `0x0800` / `0x0806` comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum EtherType {
    /// IPv4 (`0x0800`).
    Ipv4 = 0x0800,
    /// ARP (`0x0806`).
    Arp = 0x0806,
    /// IPv6 (`0x86DD`).
    Ipv6 = 0x86DD,
}

impl EtherType {
    /// Parse from a raw big-endian `u16` value.  Returns `None` for unknown types.
    #[inline]
    pub const fn from_u16(val: u16) -> Option<Self> {
        match val {
            0x0800 => Some(Self::Ipv4),
            0x0806 => Some(Self::Arp),
            0x86DD => Some(Self::Ipv6),
            _ => None,
        }
    }

    /// Return the raw `u16` value.
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

impl fmt::Display for EtherType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4 => write!(f, "IPv4"),
            Self::Arp => write!(f, "ARP"),
            Self::Ipv6 => write!(f, "IPv6"),
        }
    }
}

/// IP protocol number field values.
///
/// Pattern matching on this enum replaces raw `6` / `17` comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum IpProtocol {
    /// ICMP (`1`).
    Icmp = 1,
    /// TCP (`6`).
    Tcp = 6,
    /// UDP (`17`).
    Udp = 17,
}

impl IpProtocol {
    /// Parse from a raw `u8` value.  Returns `None` for unknown protocols.
    #[inline]
    pub const fn from_u8(val: u8) -> Option<Self> {
        match val {
            1 => Some(Self::Icmp),
            6 => Some(Self::Tcp),
            17 => Some(Self::Udp),
            _ => None,
        }
    }

    /// Return the raw `u8` value.
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
