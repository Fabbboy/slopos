use super::RawFd;
use super::error::{SyscallError, SyscallResult};
use super::numbers::{
    SYSCALL_NET_ADDR_CTL, SYSCALL_NET_IFACE_CTL, SYSCALL_NET_MONITOR, SYSCALL_NET_QUERY,
    SYSCALL_NET_RESOLVER_SET, SYSCALL_NET_ROUTE_CTL, SYSCALL_RESOLVE,
};
use super::raw::{syscall2, syscall3, syscall4};
use slopos_abi::net::{
    NET_ADDROP_ADD, NET_ADDROP_DEL, NET_ROUTEOP_ADD, NET_ROUTEOP_DEL, SockAddrIn, UserAddrReq,
    UserResolverReq, UserRouteReq,
};
use slopos_abi::syscall::{F_GETFL, F_SETFL, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

/// Turn a raw syscall return into a result; every syscall below reports failure
/// as a negated errno.
#[inline]
fn checked(raw: i64) -> SyscallResult<u64> {
    if raw < 0 {
        Err(SyscallError::from_errno((-raw) as i32))
    } else {
        Ok(raw as u64)
    }
}

/// Enumerate one class of network state into `buf`.
///
/// `buf` receives a `UserNetQueryHdr` followed by `record_count` records of
/// `record_size` bytes; completeness is read from the header's `total_count`,
/// not from the return value, so a header-sized buffer is the sizing query.
pub fn net_query(what: u32, ifindex: u32, buf: &mut [u8]) -> SyscallResult<usize> {
    let raw = unsafe {
        syscall4(
            SYSCALL_NET_QUERY,
            what as u64,
            ifindex as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    };
    checked(raw).map(|n| n as usize)
}

/// Per-interface control, and the two global operations addressed to
/// `NET_IFINDEX_GLOBAL`. Requires `TASK_FLAG_NET_ADMIN`.
pub fn net_iface_ctl(ifindex: u32, op: u32, arg: u64) -> SyscallResult<()> {
    let raw = unsafe { syscall3(SYSCALL_NET_IFACE_CTL, ifindex as u64, op as u64, arg) as i64 };
    checked(raw).map(|_| ())
}

/// Add (`add == true`) or remove an interface address. Requires
/// `TASK_FLAG_NET_ADMIN`.
pub fn net_addr_ctl(req: &UserAddrReq, add: bool) -> SyscallResult<()> {
    let raw = unsafe {
        syscall3(
            SYSCALL_NET_ADDR_CTL,
            u64::from(if add { NET_ADDROP_ADD } else { NET_ADDROP_DEL }),
            req as *const UserAddrReq as u64,
            core::mem::size_of::<UserAddrReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Add (`add == true`) or remove a route. Requires `TASK_FLAG_NET_ADMIN`.
pub fn net_route_ctl(req: &UserRouteReq, add: bool) -> SyscallResult<()> {
    let raw = unsafe {
        syscall3(
            SYSCALL_NET_ROUTE_CTL,
            u64::from(if add {
                NET_ROUTEOP_ADD
            } else {
                NET_ROUTEOP_DEL
            }),
            req as *const UserRouteReq as u64,
            core::mem::size_of::<UserRouteReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Replace the resolver configuration. Requires `TASK_FLAG_NET_ADMIN`.
pub fn net_resolver_set(req: &UserResolverReq) -> SyscallResult<()> {
    let raw = unsafe {
        syscall2(
            SYSCALL_NET_RESOLVER_SET,
            req as *const UserResolverReq as u64,
            core::mem::size_of::<UserResolverReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Open a network-state monitor. The fd becomes `POLLIN`-ready whenever the
/// stack's configuration changes and reads drain whole `NetEvent` records.
pub fn net_monitor(mask: u32, flags: u32) -> SyscallResult<super::OwnedFd> {
    let raw = unsafe { syscall2(SYSCALL_NET_MONITOR, mask as u64, flags as u64) as i64 };
    // SAFETY: a non-negative return is an fd the kernel just allocated to us.
    checked(raw).map(|fd| unsafe { super::OwnedFd::from_raw(fd as i32) })
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

/// Bind an AF_UNIX socket to `path` (a `SockAddrUn`, `addrlen = 2 + path.len()`).
pub fn bind_unix(fd: RawFd, path: &[u8]) -> SyscallResult<()> {
    let mut addr = slopos_abi::unix::SockAddrUn::default();
    addr.family = slopos_abi::net::AF_UNIX;
    let n = path.len().min(slopos_abi::unix::UNIX_PATH_MAX);
    addr.path[..n].copy_from_slice(&path[..n]);
    Sys::bind(fd, &addr as *const _ as *const u8, (2 + n) as u32).map_err(Into::into)
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

pub(crate) fn resolve(hostname: &[u8]) -> SyscallResult<[u8; 4]> {
    let mut result = [0u8; 4];
    let rc = unsafe {
        syscall3(
            SYSCALL_RESOLVE,
            hostname.as_ptr() as u64,
            hostname.len() as u64,
            &mut result as *mut [u8; 4] as u64,
        )
    };
    let signed = rc as i64;
    if signed < 0 {
        Err(SyscallError::from_errno((-signed) as i32))
    } else {
        Ok(result)
    }
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
