//! Shared userland DNS resolver.  Every tool that turns a host
//! string into an IPv4 address routes through [`resolve_host`];
//! [`syscall::net::resolve`](crate::syscall::net) is `pub(crate)` so
//! there is no bypass path.

use core::fmt;

use slopos_slibc::SyscallError;

use crate::syscall::net as syscall_net;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    #[inline]
    pub const fn octets(self) -> [u8; 4] {
        self.0
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = &self.0;
        write!(f, "{}.{}.{}.{}", o[0], o[1], o[2], o[3])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    InvalidHostname,
    NoDnsServer,
    Transient,
    NameNotFound,
    Unknown(i32),
}

impl From<SyscallError> for ResolveError {
    fn from(err: SyscallError) -> Self {
        match err.errno() {
            e if e == SyscallError::EINVAL.errno() => Self::InvalidHostname,
            e if e == SyscallError::ENETUNREACH.errno() => Self::NoDnsServer,
            e if e == SyscallError::EAGAIN.errno() => Self::Transient,
            e if e == SyscallError::EHOSTUNREACH.errno() => Self::NameNotFound,
            other => Self::Unknown(other),
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostname => f.write_str("invalid hostname"),
            Self::NoDnsServer => f.write_str("no DNS server configured (network not ready)"),
            Self::Transient => f.write_str("DNS query failed (timeout or transmit error)"),
            Self::NameNotFound => f.write_str("no address found for host"),
            Self::Unknown(errno) => write!(f, "DNS resolve failed (errno {})", errno),
        }
    }
}

fn parse_ipv4_literal(host: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut iter = host.split('.');
    for slot in out.iter_mut() {
        let piece = iter.next()?;
        if piece.is_empty() || piece.len() > 3 {
            return None;
        }
        let mut val: u32 = 0;
        for b in piece.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            val = val * 10 + (b - b'0') as u32;
            if val > 255 {
                return None;
            }
        }
        *slot = val as u8;
    }
    if iter.next().is_some() {
        return None;
    }
    Some(out)
}

pub fn resolve_host(host: &str) -> Result<Ipv4Addr, ResolveError> {
    if host.is_empty() {
        return Err(ResolveError::InvalidHostname);
    }
    if let Some(octets) = parse_ipv4_literal(host) {
        return Ok(Ipv4Addr(octets));
    }
    syscall_net::resolve(host.as_bytes())
        .map(Ipv4Addr)
        .map_err(ResolveError::from)
}

pub fn resolve_host_raw(host: &str) -> Result<[u8; 4], ResolveError> {
    resolve_host(host).map(|addr| addr.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ipv4_literal_accepts_standard() {
        assert_eq!(parse_ipv4_literal("10.0.0.1"), Some([10, 0, 0, 1]));
        assert_eq!(parse_ipv4_literal("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4_literal("255.255.255.255"), Some([255; 4]));
    }

    #[test]
    fn parse_ipv4_literal_rejects_malformed() {
        assert_eq!(parse_ipv4_literal(""), None);
        assert_eq!(parse_ipv4_literal("10.0.0"), None);
        assert_eq!(parse_ipv4_literal("10.0.0.1.2"), None);
        assert_eq!(parse_ipv4_literal("10.0.0.256"), None);
        assert_eq!(parse_ipv4_literal("abc.def.ghi.jkl"), None);
        assert_eq!(parse_ipv4_literal("10..0.1"), None);
        assert_eq!(parse_ipv4_literal("10.0.0.1 "), None);
    }

    #[test]
    fn resolve_error_display_messages() {
        assert_eq!(
            format!("{}", ResolveError::InvalidHostname),
            "invalid hostname"
        );
        assert_eq!(
            format!("{}", ResolveError::NameNotFound),
            "no address found for host"
        );
        assert_eq!(
            format!("{}", ResolveError::Unknown(42)),
            "DNS resolve failed (errno 42)"
        );
    }

    #[test]
    fn resolve_error_from_syscall_error_is_total() {
        assert_eq!(
            ResolveError::from(SyscallError::EINVAL),
            ResolveError::InvalidHostname
        );
        assert_eq!(
            ResolveError::from(SyscallError::ENETUNREACH),
            ResolveError::NoDnsServer
        );
        assert_eq!(
            ResolveError::from(SyscallError::EAGAIN),
            ResolveError::Transient
        );
        assert_eq!(
            ResolveError::from(SyscallError::EHOSTUNREACH),
            ResolveError::NameNotFound
        );
        assert_eq!(
            ResolveError::from(SyscallError::from_errno(99)),
            ResolveError::Unknown(99)
        );
    }
}
