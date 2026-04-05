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
