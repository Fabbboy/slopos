#![deny(unsafe_op_in_unsafe_fn)]

//! SlopOS platform implementation for `std::net`.
//!
//! Provides the `Socket` wrapper, `LookupHost` iterator, and the `netc` module
//! of C-compatible types, constants, and extern declarations that the shared
//! `mod.rs` in `sys/net/connection/socket/` consumes.
//!
//! All unsafe is confined to the extern "C" calls and raw-pointer plumbing
//! inside Socket methods.

use crate::ffi::c_int;
use crate::io::{self, BorrowedBuf, BorrowedCursor, ErrorKind, IoSlice, IoSliceMut};
use crate::mem::MaybeUninit;
use crate::net::{Shutdown, SocketAddr, SocketAddrV4, SocketAddrV6};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
use crate::sys::fd::FileDesc;
use crate::sys::net::connection::each_addr;
use crate::sys::net::{getsockopt, setsockopt};
use crate::sys::{AsInner, FromInner, IntoInner};
use crate::time::{Duration, Instant};
use crate::{cmp, mem, ptr};

use super::{socket_addr_from_c, socket_addr_to_c};

// ---------------------------------------------------------------------------
// netc — C types, constants, and extern function declarations
// ---------------------------------------------------------------------------

/// C-compatible networking primitives for the SlopOS platform.
///
/// This module serves the same role as `libc` on Unix targets: it provides the
/// raw types and extern declarations that the shared socket code in
/// `sys/net/connection/socket/mod.rs` calls through.
pub mod netc {
    #![allow(non_camel_case_types, dead_code)]

    use core::ffi::c_void;

    // -- scalar aliases --
    pub type c_int = i32;
    pub type c_uint = u32;
    pub type c_ushort = u16;
    pub type c_char = core::ffi::c_char;
    pub type sa_family_t = u16;
    pub type socklen_t = u32;
    pub type in_port_t = u16;
    pub type in_addr_t = u32;
    pub type ssize_t = isize;
    pub type size_t = usize;
    pub type time_t = i64;
    pub type suseconds_t = i64;

    // -- address structures --

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct in_addr {
        pub s_addr: in_addr_t,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct in6_addr {
        pub s6_addr: [u8; 16],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct sockaddr {
        pub sa_family: sa_family_t,
        pub sa_data: [u8; 14],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct sockaddr_in {
        pub sin_family: sa_family_t,
        pub sin_port: in_port_t,
        pub sin_addr: in_addr,
        pub sin_zero: [u8; 8],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct sockaddr_in6 {
        pub sin6_family: sa_family_t,
        pub sin6_port: in_port_t,
        pub sin6_flowinfo: u32,
        pub sin6_addr: in6_addr,
        pub sin6_scope_id: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct sockaddr_storage {
        pub ss_family: sa_family_t,
        __pad: [u8; 126],
    }

    #[repr(C)]
    pub struct addrinfo {
        pub ai_flags: c_int,
        pub ai_family: c_int,
        pub ai_socktype: c_int,
        pub ai_protocol: c_int,
        pub ai_addrlen: socklen_t,
        pub ai_addr: *mut sockaddr,
        pub ai_canonname: *mut c_char,
        pub ai_next: *mut addrinfo,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct linger {
        pub l_onoff: c_int,
        pub l_linger: c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ip_mreq {
        pub imr_multiaddr: in_addr,
        pub imr_interface: in_addr,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ipv6_mreq {
        pub ipv6mr_multiaddr: in6_addr,
        pub ipv6mr_interface: c_uint,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct timeval {
        pub tv_sec: time_t,
        pub tv_usec: suseconds_t,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct pollfd {
        pub fd: c_int,
        pub events: i16,
        pub revents: i16,
    }

    // -- constants --

    pub const AF_INET: c_int = 2;
    pub const AF_INET6: c_int = 10;

    pub const SOCK_STREAM: c_int = 1;
    pub const SOCK_DGRAM: c_int = 2;

    pub const SOL_SOCKET: c_int = 1;
    pub const IPPROTO_IP: c_int = 0;
    pub const IPPROTO_TCP: c_int = 6;
    pub const IPPROTO_IPV6: c_int = 41;

    pub const SO_REUSEADDR: c_int = 2;
    pub const SO_ERROR: c_int = 4;
    pub const SO_BROADCAST: c_int = 6;
    pub const SO_SNDBUF: c_int = 7;
    pub const SO_RCVBUF: c_int = 8;
    pub const SO_KEEPALIVE: c_int = 9;
    pub const SO_LINGER: c_int = 13;
    pub const SO_RCVTIMEO: c_int = 20;
    pub const SO_SNDTIMEO: c_int = 21;

    pub const TCP_NODELAY: c_int = 1;

    pub const IP_TTL: c_int = 2;
    pub const IP_MULTICAST_TTL: c_int = 33;
    pub const IP_MULTICAST_LOOP: c_int = 34;
    pub const IP_ADD_MEMBERSHIP: c_int = 35;
    pub const IP_DROP_MEMBERSHIP: c_int = 36;

    pub const IPV6_V6ONLY: c_int = 26;
    pub const IPV6_MULTICAST_LOOP: c_int = 19;
    pub const IPV6_ADD_MEMBERSHIP: c_int = 20;
    pub const IPV6_DROP_MEMBERSHIP: c_int = 21;

    pub const SHUT_RD: c_int = 0;
    pub const SHUT_WR: c_int = 1;
    pub const SHUT_RDWR: c_int = 2;

    pub const MSG_PEEK: c_int = 0x2;
    pub const MSG_NOSIGNAL: c_int = 0x4000;

    pub const POLLIN: i16 = 0x0001;
    pub const POLLOUT: i16 = 0x0004;
    pub const POLLERR: i16 = 0x0008;
    pub const POLLHUP: i16 = 0x0010;

    pub const EAI_SYSTEM: c_int = -11;

    // SlopOS uses Linux errno values (slopos-abi::syscall::errno_defs).
    // The BSD-style values that the template-copied PAL had (EISCONN=56,
    // EINPROGRESS=36) caused `Socket::connect`'s EISCONN fast-path and
    // `Socket::connect_timeout`'s EINPROGRESS poll arm to mis-fire,
    // surfacing as connect-fails with a raw errno that std could not
    // interpret. Keep these in lock-step with `errno_defs.rs`.
    pub const EINTR: c_int = 4;
    pub const EISCONN: c_int = 106;
    pub const EINPROGRESS: c_int = 115;

    pub const F_GETFL: c_int = 3;
    pub const F_SETFL: c_int = 4;
    pub const O_NONBLOCK: c_int = 0x800;

    // -- extern C functions (link to slibc #[no_mangle] exports) --

    unsafe extern "C" {
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn bind(fd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
        pub fn listen(fd: c_int, backlog: c_int) -> c_int;
        pub fn accept(fd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
        pub fn connect(fd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;

        pub fn send(
            fd: c_int,
            buf: *const c_void,
            len: size_t,
            flags: c_int,
        ) -> ssize_t;
        pub fn recv(
            fd: c_int,
            buf: *mut c_void,
            len: size_t,
            flags: c_int,
        ) -> ssize_t;
        pub fn sendto(
            fd: c_int,
            buf: *const c_void,
            len: size_t,
            flags: c_int,
            dest_addr: *const sockaddr,
            addrlen: socklen_t,
        ) -> ssize_t;
        pub fn recvfrom(
            fd: c_int,
            buf: *mut c_void,
            len: size_t,
            flags: c_int,
            src_addr: *mut sockaddr,
            addrlen: *mut socklen_t,
        ) -> ssize_t;

        pub fn setsockopt(
            fd: c_int,
            level: c_int,
            optname: c_int,
            optval: *const c_void,
            optlen: socklen_t,
        ) -> c_int;
        pub fn getsockopt(
            fd: c_int,
            level: c_int,
            optname: c_int,
            optval: *mut c_void,
            optlen: *mut socklen_t,
        ) -> c_int;
        pub fn shutdown(fd: c_int, how: c_int) -> c_int;

        pub fn getpeername(
            fd: c_int,
            addr: *mut sockaddr,
            addrlen: *mut socklen_t,
        ) -> c_int;
        pub fn getsockname(
            fd: c_int,
            addr: *mut sockaddr,
            addrlen: *mut socklen_t,
        ) -> c_int;

        pub fn getaddrinfo(
            node: *const c_char,
            service: *const c_char,
            hints: *const addrinfo,
            res: *mut *mut addrinfo,
        ) -> c_int;
        pub fn freeaddrinfo(res: *mut addrinfo);
        pub fn gai_strerror(errcode: c_int) -> *const c_char;

        pub fn poll(fds: *mut pollfd, nfds: u32, timeout: c_int) -> c_int;
        pub fn fcntl(fd: c_int, cmd: c_int, arg: i64) -> c_int;
    }
}

use netc as c;

// Re-export for mod.rs
pub use crate::sys::{cvt, cvt_r};

#[expect(non_camel_case_types)]
pub type wrlen_t = c::size_t;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn init() {}

pub fn cvt_gai(err: c_int) -> io::Result<()> {
    if err == 0 {
        return Ok(());
    }
    if err == c::EAI_SYSTEM {
        return Err(io::Error::last_os_error());
    }
    let detail = unsafe {
        let ptr = c::gai_strerror(err);
        if ptr.is_null() {
            ""
        } else {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr as *const u8, len))
        }
    };
    Err(io::Error::new(
        io::ErrorKind::Uncategorized,
        &format!("failed to lookup address information: {detail}")[..],
    ))
}

fn on_resolver_failure() {}

fn errno() -> c_int {
    crate::sys::pal::errno()
}

// ---------------------------------------------------------------------------
// LookupHost
// ---------------------------------------------------------------------------

pub struct LookupHost {
    original: *mut c::addrinfo,
    cur: *mut c::addrinfo,
    port: u16,
}

impl Iterator for LookupHost {
    type Item = SocketAddr;
    fn next(&mut self) -> Option<SocketAddr> {
        loop {
            unsafe {
                let cur = self.cur.as_ref()?;
                self.cur = cur.ai_next;
                match socket_addr_from_c(
                    cur.ai_addr.cast(),
                    cur.ai_addrlen as usize,
                ) {
                    Ok(mut addr) => {
                        addr.set_port(self.port);
                        return Some(addr);
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}

unsafe impl Sync for LookupHost {}
unsafe impl Send for LookupHost {}

impl Drop for LookupHost {
    fn drop(&mut self) {
        unsafe { c::freeaddrinfo(self.original) }
    }
}

pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    init();
    crate::sys::helpers::run_with_cstr(host.as_bytes(), &|c_host| {
        let mut hints: c::addrinfo = unsafe { mem::zeroed() };
        hints.ai_socktype = c::SOCK_STREAM;
        let mut res = ptr::null_mut();
        unsafe {
            cvt_gai(c::getaddrinfo(
                c_host.as_ptr(),
                ptr::null(),
                &hints,
                &mut res,
            ))
            .map(|_| LookupHost {
                original: res,
                cur: res,
                port,
            })
        }
    })
}

// ---------------------------------------------------------------------------
// Socket
// ---------------------------------------------------------------------------

pub struct Socket(FileDesc);

impl Socket {
    pub fn new(family: c_int, ty: c_int) -> io::Result<Socket> {
        let fd = cvt(unsafe { c::socket(family, ty, 0) })?;
        let fd = unsafe { FileDesc::from_raw_fd(fd) };
        fd.set_cloexec()?;
        Ok(Socket(fd))
    }

    pub fn new_pair(_fam: c_int, _ty: c_int) -> io::Result<(Socket, Socket)> {
        Err(io::const_error!(
            ErrorKind::Unsupported,
            "socketpair not supported on SlopOS"
        ))
    }

    pub fn connect(&self, addr: &SocketAddr) -> io::Result<()> {
        let (addr, len) = socket_addr_to_c(addr);
        loop {
            let result =
                unsafe { c::connect(self.as_raw_fd(), addr.as_ptr(), len) };
            if result == -1 {
                let err = errno();
                match err {
                    c::EINTR => continue,
                    c::EISCONN => return Ok(()),
                    _ => return Err(io::Error::from_raw_os_error(err)),
                }
            }
            return Ok(());
        }
    }

    pub fn connect_timeout(
        &self,
        addr: &SocketAddr,
        timeout: Duration,
    ) -> io::Result<()> {
        self.set_nonblocking(true)?;
        let r = unsafe {
            let (addr, len) = socket_addr_to_c(addr);
            cvt(c::connect(self.as_raw_fd(), addr.as_ptr(), len))
        };
        self.set_nonblocking(false)?;

        match r {
            Ok(_) => return Ok(()),
            Err(ref e) if e.raw_os_error() == Some(c::EINPROGRESS) => {}
            Err(e) => return Err(e),
        }

        let mut pollfd = c::pollfd {
            fd: self.as_raw_fd(),
            events: c::POLLOUT,
            revents: 0,
        };

        if timeout.as_secs() == 0 && timeout.subsec_nanos() == 0 {
            return Err(io::Error::ZERO_TIMEOUT);
        }

        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(io::const_error!(
                    io::ErrorKind::TimedOut,
                    "connection timed out"
                ));
            }

            let remaining = timeout - elapsed;
            let mut timeout_ms = remaining
                .as_secs()
                .saturating_mul(1_000)
                .saturating_add(remaining.subsec_nanos() as u64 / 1_000_000);
            if timeout_ms == 0 {
                timeout_ms = 1;
            }
            let timeout_ms = cmp::min(timeout_ms, c_int::MAX as u64) as c_int;

            match unsafe { c::poll(&mut pollfd, 1, timeout_ms) } {
                -1 => {
                    let err = io::Error::last_os_error();
                    if !err.is_interrupted() {
                        return Err(err);
                    }
                }
                0 => {}
                _ => {
                    // Check for POLLHUP/POLLERR
                    if pollfd.revents & (c::POLLHUP | c::POLLERR) != 0 {
                        let e = self.take_error()?.unwrap_or_else(|| {
                            io::const_error!(
                                io::ErrorKind::Uncategorized,
                                "no error set after POLLHUP",
                            )
                        });
                        return Err(e);
                    }
                    return Ok(());
                }
            }
        }
    }

    pub fn accept(
        &self,
        storage: *mut c::sockaddr,
        len: *mut c::socklen_t,
    ) -> io::Result<Socket> {
        let fd =
            cvt_r(|| unsafe { c::accept(self.as_raw_fd(), storage, len) })?;
        let fd = unsafe { FileDesc::from_raw_fd(fd) };
        fd.set_cloexec()?;
        Ok(Socket(fd))
    }

    pub fn duplicate(&self) -> io::Result<Socket> {
        self.0.duplicate().map(Socket)
    }

    pub fn send_with_flags(
        &self,
        buf: &[u8],
        flags: c_int,
    ) -> io::Result<usize> {
        let len = cmp::min(buf.len(), <wrlen_t>::MAX as usize) as wrlen_t;
        let ret = cvt(unsafe {
            c::send(
                self.as_raw_fd(),
                buf.as_ptr() as *const _,
                len,
                flags,
            )
        })?;
        Ok(ret as usize)
    }

    fn recv_with_flags(
        &self,
        mut buf: BorrowedCursor<'_>,
        flags: c_int,
    ) -> io::Result<()> {
        let ret = cvt(unsafe {
            c::recv(
                self.as_raw_fd(),
                buf.as_mut().as_mut_ptr() as *mut _,
                buf.capacity(),
                flags,
            )
        })?;
        unsafe {
            buf.advance(ret as usize);
        }
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut buf = BorrowedBuf::from(buf);
        self.recv_with_flags(buf.unfilled(), 0)?;
        Ok(buf.len())
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut buf = BorrowedBuf::from(buf);
        self.recv_with_flags(buf.unfilled(), c::MSG_PEEK)?;
        Ok(buf.len())
    }

    pub fn read_buf(&self, buf: BorrowedCursor<'_>) -> io::Result<()> {
        self.recv_with_flags(buf, 0)
    }

    pub fn read_vectored(
        &self,
        bufs: &mut [IoSliceMut<'_>],
    ) -> io::Result<usize> {
        self.0.read_vectored(bufs)
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        self.0.is_read_vectored()
    }

    fn recv_from_with_flags(
        &self,
        buf: &mut [u8],
        flags: c_int,
    ) -> io::Result<(usize, SocketAddr)> {
        let mut storage: MaybeUninit<c::sockaddr_storage> =
            MaybeUninit::uninit();
        let mut addrlen =
            size_of_val(&storage) as c::socklen_t;

        let n = cvt(unsafe {
            c::recvfrom(
                self.as_raw_fd(),
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                flags,
                (&raw mut storage) as *mut _,
                &mut addrlen,
            )
        })?;
        Ok((
            n as usize,
            unsafe {
                socket_addr_from_c(storage.as_ptr(), addrlen as usize)?
            },
        ))
    }

    pub fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        self.recv_from_with_flags(buf, 0)
    }

    pub fn peek_from(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        self.recv_from_with_flags(buf, c::MSG_PEEK)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    pub fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
    ) -> io::Result<usize> {
        self.0.write_vectored(bufs)
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    pub fn set_timeout(
        &self,
        dur: Option<Duration>,
        kind: c::c_int,
    ) -> io::Result<()> {
        let timeout = match dur {
            Some(dur) => {
                if dur.as_secs() == 0 && dur.subsec_nanos() == 0 {
                    return Err(io::Error::ZERO_TIMEOUT);
                }
                let secs = if dur.as_secs() > c::time_t::MAX as u64 {
                    c::time_t::MAX
                } else {
                    dur.as_secs() as c::time_t
                };
                let mut timeout = c::timeval {
                    tv_sec: secs,
                    tv_usec: dur.subsec_micros() as c::suseconds_t,
                };
                if timeout.tv_sec == 0 && timeout.tv_usec == 0 {
                    timeout.tv_usec = 1;
                }
                timeout
            }
            None => c::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        unsafe { setsockopt(self, c::SOL_SOCKET, kind, timeout) }
    }

    pub fn timeout(
        &self,
        kind: c::c_int,
    ) -> io::Result<Option<Duration>> {
        let raw: c::timeval =
            unsafe { getsockopt(self, c::SOL_SOCKET, kind)? };
        if raw.tv_sec == 0 && raw.tv_usec == 0 {
            Ok(None)
        } else {
            let sec = raw.tv_sec as u64;
            let nsec = (raw.tv_usec as u32) * 1000;
            Ok(Some(Duration::new(sec, nsec)))
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let how = match how {
            Shutdown::Write => c::SHUT_WR,
            Shutdown::Read => c::SHUT_RD,
            Shutdown::Both => c::SHUT_RDWR,
        };
        cvt(unsafe { c::shutdown(self.as_raw_fd(), how) })?;
        Ok(())
    }

    pub fn set_linger(
        &self,
        linger: Option<Duration>,
    ) -> io::Result<()> {
        let linger = c::linger {
            l_onoff: linger.is_some() as c::c_int,
            l_linger: linger.unwrap_or_default().as_secs() as c::c_int,
        };
        unsafe { setsockopt(self, c::SOL_SOCKET, c::SO_LINGER, linger) }
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        let val: c::linger =
            unsafe { getsockopt(self, c::SOL_SOCKET, c::SO_LINGER)? };
        Ok(
            (val.l_onoff != 0)
                .then(|| Duration::from_secs(val.l_linger as u64)),
        )
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        unsafe {
            setsockopt(
                self,
                c::IPPROTO_TCP,
                c::TCP_NODELAY,
                nodelay as c_int,
            )
        }
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        let raw: c_int = unsafe {
            getsockopt(self, c::IPPROTO_TCP, c::TCP_NODELAY)?
        };
        Ok(raw != 0)
    }

    pub fn set_keepalive(&self, keepalive: bool) -> io::Result<()> {
        unsafe {
            setsockopt(
                self,
                c::SOL_SOCKET,
                c::SO_KEEPALIVE,
                keepalive as c_int,
            )
        }
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        let raw: c_int = unsafe {
            getsockopt(self, c::SOL_SOCKET, c::SO_KEEPALIVE)?
        };
        Ok(raw != 0)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let flags = cvt(unsafe {
            c::fcntl(self.as_raw_fd(), c::F_GETFL, 0)
        })?;
        let flags = if nonblocking {
            flags | c::O_NONBLOCK
        } else {
            flags & !c::O_NONBLOCK
        };
        cvt(unsafe {
            c::fcntl(self.as_raw_fd(), c::F_SETFL, flags as i64)
        })?;
        Ok(())
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        let raw: c_int = unsafe {
            getsockopt(self, c::SOL_SOCKET, c::SO_ERROR)?
        };
        if raw == 0 {
            Ok(None)
        } else {
            Ok(Some(io::Error::from_raw_os_error(raw)))
        }
    }

    pub fn as_raw(&self) -> RawFd {
        self.as_raw_fd()
    }
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

impl AsInner<FileDesc> for Socket {
    #[inline]
    fn as_inner(&self) -> &FileDesc {
        &self.0
    }
}

impl IntoInner<FileDesc> for Socket {
    fn into_inner(self) -> FileDesc {
        self.0
    }
}

impl FromInner<FileDesc> for Socket {
    fn from_inner(file_desc: FileDesc) -> Self {
        Self(file_desc)
    }
}

impl AsFd for Socket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for Socket {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for Socket {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for Socket {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self(unsafe { FromRawFd::from_raw_fd(raw_fd) })
    }
}
