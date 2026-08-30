//! Shared userland DNS resolver, plus re-exports of the `slopos-net-core`
//! address types. [`syscall::net::resolve`](crate::syscall::net) is
//! `pub(crate)`, so [`resolve_host`] has no bypass path.

use core::fmt;

use slopos_slibc::SyscallError;

pub use slopos_net_core::{Cidr, Ipv4, Mac};

use crate::syscall::net as syscall_net;

/// Spelling used by tools that also name `std::net::Ipv4Addr`.
pub type Ipv4Addr = Ipv4;

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

pub fn resolve_host(host: &str) -> Result<Ipv4Addr, ResolveError> {
    if host.is_empty() {
        return Err(ResolveError::InvalidHostname);
    }
    if let Some(addr) = Ipv4::from_str_bytes(host.as_bytes()) {
        return Ok(addr);
    }
    syscall_net::resolve(host.as_bytes())
        .map(Ipv4)
        .map_err(ResolveError::from)
}

pub fn resolve_host_raw(host: &str) -> Result<[u8; 4], ResolveError> {
    resolve_host(host).map(|addr| addr.0)
}

/// TCP echo peer that QEMU's user-mode backend answers on, configured by
/// `scripts/qemu_run.sh` as a `guestfwd` that forks `/bin/cat` per connection.
///
/// Tests needing a peer that completes a handshake and returns bytes dial this
/// rather than a public address: it is reached over `eth0` with the same route,
/// ARP and source-selection paths as any off-box destination, but does not
/// require the host to have egress. A test that dials the internet instead can
/// only fail for reasons that are not about SlopOS.
pub const ECHO_PEER_ADDR: [u8; 4] = [10, 0, 2, 100];
pub const ECHO_PEER_PORT: u16 = 9999;

#[cfg(test)]
mod tests {
    use super::*;

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
