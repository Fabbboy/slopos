use super::RawFd;
use super::error::{SyscallError, SyscallResult};
use super::numbers::{SYSCALL_NET_INFO, SYSCALL_NET_SCAN, SYSCALL_RESOLVE};
use super::raw::{syscall1, syscall3};
use slopos_abi::net::{SockAddrIn, UserNetInfo, UserNetMember};
use slopos_abi::syscall::{F_GETFL, F_SETFL, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

#[inline(always)]
pub fn net_scan(out: &mut [UserNetMember], active_probe: bool) -> i64 {
    if out.is_empty() {
        return 0;
    }

    unsafe {
        syscall3(
            SYSCALL_NET_SCAN,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            if active_probe { 1 } else { 0 },
        ) as i64
    }
}

#[inline(always)]
pub fn net_info(out: &mut UserNetInfo) -> i64 {
    unsafe { syscall1(SYSCALL_NET_INFO, out as *mut UserNetInfo as u64) as i64 }
}

pub fn socket(domain: u16, sock_type: u16, protocol: u16) -> SyscallResult<super::OwnedFd> {
    Sys::socket(domain as i32, sock_type as i32, protocol as i32)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
        .map_err(Into::into)
}

pub fn bind(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    Sys::bind(
        fd,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn listen(fd: RawFd, backlog: u32) -> SyscallResult<()> {
    Sys::listen(fd, backlog as i32).map_err(Into::into)
}

pub fn accept(fd: RawFd, peer: Option<&mut SockAddrIn>) -> SyscallResult<super::OwnedFd> {
    let mut addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    let peer_ptr = peer
        .map(|p| p as *mut _ as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    let addrlen_ptr = if peer_ptr.is_null() {
        core::ptr::null_mut()
    } else {
        &mut addrlen as *mut u32
    };
    Sys::accept(fd, peer_ptr, addrlen_ptr)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
        .map_err(Into::into)
}

pub fn connect(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    Sys::connect(
        fd,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn send(fd: RawFd, data: &[u8], flags: u32) -> SyscallResult<usize> {
    Sys::send(fd, data.as_ptr(), data.len(), flags as i32).map_err(Into::into)
}

pub fn recv(fd: RawFd, buf: &mut [u8], flags: u32) -> SyscallResult<usize> {
    Sys::recv(fd, buf.as_mut_ptr(), buf.len(), flags as i32).map_err(Into::into)
}

pub fn sendto(fd: RawFd, data: &[u8], flags: u32, addr: &SockAddrIn) -> SyscallResult<usize> {
    Sys::sendto(
        fd,
        data.as_ptr(),
        data.len(),
        flags as i32,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn recvfrom(
    fd: RawFd,
    buf: &mut [u8],
    flags: u32,
    src_addr: Option<&mut SockAddrIn>,
) -> SyscallResult<usize> {
    let mut addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    let src_addr_ptr = src_addr
        .map(|a| a as *mut _ as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    let addrlen_ptr = if src_addr_ptr.is_null() {
        core::ptr::null_mut()
    } else {
        &mut addrlen as *mut u32
    };
    Sys::recvfrom(
        fd,
        buf.as_mut_ptr(),
        buf.len(),
        flags as i32,
        src_addr_ptr,
        addrlen_ptr,
    )
    .map_err(Into::into)
}

pub fn setsockopt(fd: RawFd, level: i32, optname: i32, val: &[u8]) -> SyscallResult<()> {
    Sys::setsockopt(fd, level, optname, val.as_ptr(), val.len() as u32).map_err(Into::into)
}

pub fn getsockopt(fd: RawFd, level: i32, optname: i32, buf: &mut [u8]) -> SyscallResult<usize> {
    let mut optlen = buf.len() as u32;
    Sys::getsockopt(
        fd,
        level,
        optname,
        buf.as_mut_ptr(),
        &mut optlen as *mut u32,
    )
    .map(|_| optlen as usize)
    .map_err(Into::into)
}

pub fn shutdown(fd: RawFd, how: i32) -> SyscallResult<()> {
    Sys::shutdown(fd, how).map_err(Into::into)
}

pub fn set_reuse_addr(fd: RawFd) -> SyscallResult<()> {
    let val: i32 = 1;
    setsockopt(
        fd,
        slopos_abi::syscall::SOL_SOCKET,
        slopos_abi::syscall::SO_REUSEADDR,
        &val.to_ne_bytes(),
    )
}

/// Resolve a hostname to an IPv4 address via the in-kernel DNS client.
///
/// Returns `Some([a, b, c, d])` on success, or `None` if resolution fails.
pub fn resolve(hostname: &[u8]) -> Option<[u8; 4]> {
    let mut result = [0u8; 4];
    let rc = unsafe {
        syscall3(
            SYSCALL_RESOLVE,
            hostname.as_ptr() as u64,
            hostname.len() as u64,
            &mut result as *mut [u8; 4] as u64,
        )
    };
    if (rc as i64) < 0 { None } else { Some(result) }
}

pub fn bind_any(fd: RawFd, port: u16) -> SyscallResult<()> {
    let addr = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: port.to_be(),
        addr: [0; 4],
        _pad: [0; 8],
    };
    bind(fd, &addr)
}

pub fn bind_addr(fd: RawFd, ip: [u8; 4], port: u16) -> SyscallResult<()> {
    let addr = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: port.to_be(),
        addr: ip,
        _pad: [0; 8],
    };
    bind(fd, &addr)
}

pub fn set_nonblocking(fd: RawFd) -> SyscallResult<()> {
    let current = Sys::fcntl(fd, F_GETFL as i32, 0).map_err(SyscallError::from)?;
    let flags = (current as u64) | O_NONBLOCK;
    let _ = Sys::fcntl(fd, F_SETFL as i32, flags).map_err(SyscallError::from)?;
    Ok(())
}

pub fn udp_echo_test() -> SyscallResult<()> {
    Ok(())
}

// =============================================================================
// Typestate Socket Wrapper
// =============================================================================

use core::marker::PhantomData;

/// Marker for socket states. Sealed to prevent external implementation.
pub trait SocketState: private::Sealed {}

/// A freshly-created socket that has not been bound or connected.
pub struct Unbound;
/// A socket that has been bound to a local address.
pub struct Bound;
/// A bound socket that is listening for incoming connections.
pub struct Listening;
/// A socket with an established connection (client-side connect or server-side accept).
pub struct Connected;

impl SocketState for Unbound {}
impl SocketState for Bound {}
impl SocketState for Listening {}
impl SocketState for Connected {}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Unbound {}
    impl Sealed for super::Bound {}
    impl Sealed for super::Listening {}
    impl Sealed for super::Connected {}
}

/// Type-safe socket wrapper enforcing the socket state machine at compile time.
///
/// State transitions consume `self` and return a new `Socket` in the target
/// state, making illegal sequences (e.g. `send` on an unbound socket) a
/// compile error.  The underlying file descriptor is owned via [`OwnedFd`]
/// and closed automatically on drop.
///
/// The raw free-functions in this module remain available as a low-level
/// escape hatch for protocols that don't fit the standard state machine
/// (e.g. raw/ICMP sockets, UDP sendto without connect).
pub struct Socket<S: SocketState> {
    fd: super::OwnedFd,
    _state: PhantomData<S>,
}

impl Socket<Unbound> {
    /// Create a new unbound socket.
    pub fn new(domain: u16, sock_type: u16, protocol: u16) -> SyscallResult<Self> {
        let fd = socket(domain, sock_type, protocol)?;
        Ok(Self {
            fd,
            _state: PhantomData,
        })
    }

    /// Bind to a specific IPv4 address and port, transitioning to Bound state.
    pub fn bind(self, addr: &SockAddrIn) -> SyscallResult<Socket<Bound>> {
        bind(self.fd.raw(), addr)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Bind to any local address on the given port, transitioning to Bound state.
    pub fn bind_any(self, port: u16) -> SyscallResult<Socket<Bound>> {
        bind_any(self.fd.raw(), port)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Bind to a specific local address and port, transitioning to Bound state.
    pub fn bind_addr(self, ip: [u8; 4], port: u16) -> SyscallResult<Socket<Bound>> {
        bind_addr(self.fd.raw(), ip, port)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Connect to a remote address, transitioning to Connected state.
    pub fn connect(self, addr: &SockAddrIn) -> SyscallResult<Socket<Connected>> {
        connect(self.fd.raw(), addr)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Enable SO_REUSEADDR on this socket.
    pub fn set_reuse_addr(&self) -> SyscallResult<()> {
        set_reuse_addr(self.fd.raw())
    }

    /// Set the socket to non-blocking mode.
    pub fn set_nonblocking(&self) -> SyscallResult<()> {
        set_nonblocking(self.fd.raw())
    }
}

impl Socket<Bound> {
    /// Start listening for incoming connections.
    pub fn listen(self, backlog: u32) -> SyscallResult<Socket<Listening>> {
        listen(self.fd.raw(), backlog)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Connect to a remote address after binding to a local address.
    ///
    /// This is the standard POSIX pattern for specifying a source port:
    /// `socket() -> bind(local) -> connect(remote)`.
    pub fn connect(self, addr: &SockAddrIn) -> SyscallResult<Socket<Connected>> {
        connect(self.fd.raw(), addr)?;
        Ok(Socket {
            fd: self.fd,
            _state: PhantomData,
        })
    }

    /// Enable SO_REUSEADDR on this socket.
    pub fn set_reuse_addr(&self) -> SyscallResult<()> {
        set_reuse_addr(self.fd.raw())
    }

    /// Set the socket to non-blocking mode.
    pub fn set_nonblocking(&self) -> SyscallResult<()> {
        set_nonblocking(self.fd.raw())
    }
}

impl Socket<Listening> {
    /// Accept a new connection. Returns a Connected socket for the peer.
    ///
    /// If `peer` is `Some`, the peer's address is written into it.
    pub fn accept(&self, peer: Option<&mut SockAddrIn>) -> SyscallResult<Socket<Connected>> {
        let new_fd = accept(self.fd.raw(), peer)?;
        Ok(Socket {
            fd: new_fd,
            _state: PhantomData,
        })
    }

    /// Set the socket to non-blocking mode.
    pub fn set_nonblocking(&self) -> SyscallResult<()> {
        set_nonblocking(self.fd.raw())
    }
}

impl Socket<Connected> {
    /// Send data on a connected socket.
    pub fn send(&self, data: &[u8], flags: u32) -> SyscallResult<usize> {
        send(self.fd.raw(), data, flags)
    }

    /// Receive data from a connected socket.
    pub fn recv(&self, buf: &mut [u8], flags: u32) -> SyscallResult<usize> {
        recv(self.fd.raw(), buf, flags)
    }

    /// Shut down part or all of the connection.
    pub fn shutdown(&self, how: i32) -> SyscallResult<()> {
        shutdown(self.fd.raw(), how)
    }

    /// Set the socket to non-blocking mode.
    pub fn set_nonblocking(&self) -> SyscallResult<()> {
        set_nonblocking(self.fd.raw())
    }
}

/// Methods available in every socket state.
impl<S: SocketState> Socket<S> {
    /// Borrow the raw file descriptor for interop with raw APIs.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.raw()
    }

    /// Consume the socket and return the raw fd number without closing it.
    /// The caller takes ownership of the descriptor's lifetime.
    pub fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw()
    }

    /// Consume the socket and return the inner `OwnedFd`.
    pub fn into_fd(self) -> super::OwnedFd {
        self.fd
    }

    /// Set a socket option.
    pub fn setsockopt(&self, level: i32, optname: i32, val: &[u8]) -> SyscallResult<()> {
        setsockopt(self.fd.raw(), level, optname, val)
    }

    /// Get a socket option.
    pub fn getsockopt(&self, level: i32, optname: i32, buf: &mut [u8]) -> SyscallResult<usize> {
        getsockopt(self.fd.raw(), level, optname, buf)
    }
}
