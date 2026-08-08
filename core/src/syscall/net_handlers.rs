use slopos_abi::Errno;
use slopos_abi::file_ops::FileKind;
use slopos_abi::net::{AF_INET, AF_UNIX, IPPROTO_ICMP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn};
use slopos_abi::syscall::SOL_SOCKET;
use slopos_abi::unix::SockAddrUn;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};
use slopos_net::types::{Ipv4Addr, Port, SockAddr};
use slopos_net::unix_socket::SocketHandle;
use slopos_net::{dns, socket, unix_socket, unix_socket_file_ops};

use crate::syscall::args::{Fd, UserBytes, UserPtr};
use crate::syscall::common::{errno_from_neg, errno_from_neg64};

fn rc_i32_to_unit(rc: i32) -> Result<(), Errno> {
    if rc < 0 {
        Err(errno_from_neg(rc))
    } else {
        Ok(())
    }
}

fn rc_i32_to_u64(rc: i32) -> Result<u64, Errno> {
    if rc < 0 {
        Err(errno_from_neg(rc))
    } else {
        Ok(rc as u64)
    }
}

fn rc_i64_to_u64(rc: i64) -> Result<u64, Errno> {
    if rc < 0 {
        Err(errno_from_neg64(rc))
    } else {
        Ok(rc as u64)
    }
}

/// Socket fd lookup result: either a unix socket handle or a raw IP socket index.
enum SocketFd {
    /// AF_UNIX socket — contains a [`SocketHandle`].
    Unix(SocketHandle),
    /// AF_INET socket — contains the raw socket pool index.
    Inet(u32),
}

/// Retrieve the socket handle for `fd`, distinguishing AF_UNIX from AF_INET.
fn socket_fd_for(process_id: u32, fd: i32) -> Result<SocketFd, Errno> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(process_id, fd) else {
        return Err(Errno::ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    if ops.is_unix_socket() {
        Ok(SocketFd::Unix(SocketHandle::from_usize(handle)))
    } else {
        Ok(SocketFd::Inet(handle as u32))
    }
}

define_syscall!(syscall_socket
    (ctx, domain: u32, sock_type: u32, protocol: u32)
    requires(let process_id: process_id, let task_id: task_id)
    -> Result<u64, Errno>
{
    let domain = domain as u16;
    let sock_type = sock_type as u16;
    let protocol = protocol as u16;

    if domain == AF_UNIX {
        if sock_type != SOCK_STREAM {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let handle = unix_socket::unix_create().ok_or(Errno::ENOMEM)?;
        // The backing owns the endpoint from here: a failed install (or a
        // failed backing allocation) closes it.
        let backing =
            unix_socket_file_ops::unix_socket_backing(handle).ok_or(Errno::ENOMEM)?;
        let fd = slopos_fs::fileio_open_fd_with_ops(
            process_id,
            &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
            handle.as_usize(),
            Some(backing),
            slopos_fs::FdFlags::NONE,
        );
        if fd < 0 {
            return Err(Errno::ENOMEM);
        }
        return Ok(fd as u64);
    }

    if domain != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let _icmp_datagram = sock_type == SOCK_DGRAM && protocol == IPPROTO_ICMP;

    // Both halves of the owner come from the syscall context, never from
    // userland: `net_query` gates owner disclosure by comparing against it, so a
    // caller able to name its own owner could name someone else's.
    let owner = socket::SocketOwner { process_id, task_id };
    let sock_idx = socket::socket_create(domain, sock_type, protocol, owner);
    if sock_idx < 0 {
        return Err(errno_from_neg(sock_idx));
    }

    let backing =
        slopos_net::socket_file_ops::socket_backing(sock_idx as u32).ok_or(Errno::ENOMEM)?;
    let fd = slopos_fs::fileio_open_socket_fd(process_id, sock_idx as u32, Some(backing));
    if fd < 0 {
        return Err(Errno::ENOMEM);
    }

    Ok(fd as u64)
});

define_syscall!(syscall_bind
    (ctx, fd: Fd, addr_ptr: u64, addr_len: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if addr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let addr_len = addr_len as usize;

    match sock_fd {
        SocketFd::Unix(sh) => {
            if addr_len < 4 {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrUn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return Err(Errno::EINVAL);
            }
            rc_i32_to_unit(unix_socket::unix_bind(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if addr_len < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let port = u16::from_be(sock_addr.port);
            rc_i32_to_unit(socket::socket_bind(sock_idx, sock_addr.addr, port))
        }
    }
});

define_syscall!(syscall_listen
    (ctx, fd: Fd, backlog: u32)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    match sock_fd {
        SocketFd::Unix(sh) => rc_i32_to_unit(unix_socket::unix_listen(sh, backlog)),
        SocketFd::Inet(sock_idx) => rc_i32_to_unit(socket::socket_listen(sock_idx, backlog)),
    }
});

define_syscall!(syscall_accept
    (ctx, fd: Fd, peer_ptr: u64, peer_len: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    let peer_len = peer_len as usize;

    match sock_fd {
        SocketFd::Unix(sh) => {
            let accepted_handle =
                unix_socket::unix_accept(sh).map_err(errno_from_neg)?;
            let backing = unix_socket_file_ops::unix_socket_backing(accepted_handle)
                .ok_or(Errno::ENOMEM)?;
            let new_fd = slopos_fs::fileio_open_fd_with_ops(
                process_id,
                &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                accepted_handle.as_usize(),
                Some(backing),
                slopos_fs::FdFlags::NONE,
            );
            if new_fd < 0 {
                return Err(Errno::ENOMEM);
            }
            Ok(new_fd as u64)
        }
        SocketFd::Inet(sock_idx) => {
            let mut peer_ip = [0u8; 4];
            let mut peer_port = 0u16;
            let want_peer = peer_ptr != 0;
            if want_peer && peer_len < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }

            let accepted_idx = socket::socket_accept(
                sock_idx,
                if want_peer { &mut peer_ip as *mut [u8; 4] } else { core::ptr::null_mut() },
                if want_peer { &mut peer_port as *mut u16 } else { core::ptr::null_mut() },
            );
            if accepted_idx < 0 {
                return Err(errno_from_neg(accepted_idx));
            }

            let backing = slopos_net::socket_file_ops::socket_backing(accepted_idx as u32)
                .ok_or(Errno::ENOMEM)?;
            let new_fd =
                slopos_fs::fileio_open_socket_fd(process_id, accepted_idx as u32, Some(backing));
            if new_fd < 0 {
                return Err(Errno::ENOMEM);
            }

            if want_peer {
                let peer = SockAddrIn {
                    family: AF_INET,
                    port: peer_port.to_be(),
                    addr: peer_ip,
                    _pad: [0; 8],
                };
                let user_peer = MmUserPtr::<SockAddrIn>::try_new(peer_ptr)
                    .map_err(|_| Errno::EFAULT)?;
                copy_to_user(user_peer, &peer).map_err(|_| Errno::EFAULT)?;
            }

            Ok(new_fd as u64)
        }
    }
});

define_syscall!(syscall_connect
    (ctx, fd: Fd, addr_ptr: u64, addr_len: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if addr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let addr_len = addr_len as usize;

    match sock_fd {
        SocketFd::Unix(sh) => {
            if addr_len < 4 {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrUn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return Err(Errno::EINVAL);
            }
            rc_i32_to_unit(unix_socket::unix_connect(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if addr_len < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let port = u16::from_be(sock_addr.port);
            rc_i32_to_unit(socket::socket_connect(sock_idx, sock_addr.addr, port))
        }
    }
});

define_syscall!(syscall_send
    (ctx, fd: Fd, buf: UserBytes, _flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if buf.base_u64() == 0 && buf.len() != 0 {
        return Err(Errno::EFAULT);
    }

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;

    match sock_fd {
        SocketFd::Unix(sh) => {
            if len > 0 {
                let user_data = MmUserBytes::try_new(buf.base_u64(), len).map_err(|_| Errno::EFAULT)?;
                let copied = copy_bytes_from_user(user_data, &mut scratch[..len])
                    .map_err(|_| Errno::EFAULT)?;
                return rc_i32_to_u64(unix_socket::unix_send(sh, &scratch[..copied]));
            }
            Ok(0)
        }
        SocketFd::Inet(sock_idx) => {
            if len > 0 {
                let user_data = MmUserBytes::try_new(buf.base_u64(), len).map_err(|_| Errno::EFAULT)?;
                let copied = copy_bytes_from_user(user_data, &mut scratch[..len])
                    .map_err(|_| Errno::EFAULT)?;
                return rc_i64_to_u64(socket::socket_send(sock_idx, &scratch[..copied]));
            }
            rc_i64_to_u64(socket::socket_send(sock_idx, &[]))
        }
    }
});

define_syscall!(syscall_recv
    (ctx, fd: Fd, buf: UserBytes, _flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if buf.base_u64() == 0 && buf.len() != 0 {
        return Err(Errno::EFAULT);
    }

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;

    match sock_fd {
        SocketFd::Unix(sh) => {
            let rc = unix_socket::unix_recv(sh, &mut scratch[..len]);
            if rc < 0 {
                return Err(errno_from_neg(rc));
            }
            let copied = rc as usize;
            if copied > 0 {
                let user_out = MmUserBytes::try_new(buf.base_u64(), copied).map_err(|_| Errno::EFAULT)?;
                copy_bytes_to_user(user_out, &scratch[..copied]).map_err(|_| Errno::EFAULT)?;
            }
            Ok(copied as u64)
        }
        SocketFd::Inet(sock_idx) => {
            let rc = socket::socket_recv(sock_idx, &mut scratch[..len]);
            if rc < 0 {
                return Err(errno_from_neg64(rc));
            }
            let copied = rc as usize;
            if copied > 0 {
                let user_out = MmUserBytes::try_new(buf.base_u64(), copied).map_err(|_| Errno::EFAULT)?;
                copy_bytes_to_user(user_out, &scratch[..copied]).map_err(|_| Errno::EFAULT)?;
            }
            Ok(copied as u64)
        }
    }
});

define_syscall!(syscall_sendto
    (ctx, fd: Fd, buf: UserBytes, _flags: u32, addr_ptr: u64, addr_len: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    if buf.base_u64() == 0 && buf.len() != 0 {
        return Err(Errno::EFAULT);
    }
    if addr_ptr == 0 {
        return Err(Errno::EDESTADDRREQ);
    }
    if (addr_len as usize) < core::mem::size_of::<SockAddrIn>() {
        return Err(Errno::EINVAL);
    }

    let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
    let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
    if sock_addr.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;
    let copied = if len > 0 {
        let user_data = MmUserBytes::try_new(buf.base_u64(), len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_data, &mut scratch[..len]).map_err(|_| Errno::EFAULT)?
    } else {
        0
    };

    rc_i64_to_u64(socket::socket_sendto(
        sock_idx,
        &scratch[..copied],
        sock_addr.addr,
        u16::from_be(sock_addr.port),
    ))
});

define_syscall!(syscall_recvfrom
    (ctx, fd: Fd, buf: UserBytes, _flags: u32, src_ptr: u64, src_len: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    if buf.base_u64() == 0 && buf.len() != 0 {
        return Err(Errno::EFAULT);
    }
    let want_src = src_ptr != 0;
    if want_src && (src_len as usize) < core::mem::size_of::<SockAddrIn>() {
        return Err(Errno::EINVAL);
    }

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;
    let mut src = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));

    let rc = socket::socket_recvfrom(
        sock_idx,
        &mut scratch[..len],
        want_src.then_some(&mut src),
    );
    if rc < 0 {
        return Err(errno_from_neg64(rc));
    }

    let copied = rc as usize;
    if copied > 0 {
        let user_out = MmUserBytes::try_new(buf.base_u64(), copied).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_out, &scratch[..copied]).map_err(|_| Errno::EFAULT)?;
    }

    if want_src {
        let peer = SockAddrIn {
            family: AF_INET,
            port: src.port.0.to_be(),
            addr: src.ip.0,
            _pad: [0; 8],
        };
        let user_peer = MmUserPtr::<SockAddrIn>::try_new(src_ptr).map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_peer, &peer).map_err(|_| Errno::EFAULT)?;
    }

    Ok(copied as u64)
});

define_syscall!(syscall_setsockopt
    (ctx, fd: Fd, level: u32, optname: u32, optval: UserBytes)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    if optval.base_u64() == 0 && optval.len() > 0 {
        return Err(Errno::EFAULT);
    }

    let optlen = optval.len().min(64);
    let mut scratch = [0u8; 64];
    if optlen > 0 {
        let user_data = MmUserBytes::try_new(optval.base_u64(), optlen).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_data, &mut scratch[..optlen]).map_err(|_| Errno::EFAULT)?;
    }

    rc_i32_to_unit(socket::socket_setsockopt(sock_idx, level as i32, optname as i32, &scratch[..optlen]))
});

define_syscall!(syscall_getsockopt
    (ctx, fd: Fd, level: u32, optname: u32, optval_ptr: u64, optlen_ptr_raw: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    if optval_ptr == 0 || optlen_ptr_raw == 0 {
        return Err(Errno::EFAULT);
    }

    let user_optlen = MmUserPtr::<u32>::try_new(optlen_ptr_raw).map_err(|_| Errno::EFAULT)?;
    let optlen = copy_from_user(user_optlen).map_err(|_| Errno::EFAULT)? as usize;
    let optlen = optlen.min(64);

    let mut scratch = [0u8; 64];
    let rc = socket::socket_getsockopt(sock_idx, level as i32, optname as i32, &mut scratch[..optlen]);
    if rc < 0 {
        return Err(errno_from_neg(rc));
    }

    let written = rc as usize;
    if written > 0 {
        let user_data = MmUserBytes::try_new(optval_ptr, written).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_data, &scratch[..written]).map_err(|_| Errno::EFAULT)?;
    }

    let actual_len = written as u32;
    copy_to_user(user_optlen, &actual_len).map_err(|_| Errno::EFAULT)?;

    Ok(())
});

define_syscall!(syscall_shutdown
    (ctx, fd: Fd, how: u32)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    rc_i32_to_unit(socket::socket_shutdown(sock_idx, how as i32))
});

define_syscall!(syscall_resolve
    (ctx, hostname_ptr: u64, hostname_len: u64, result_ptr: u64)
    requires(let _process_id: process_id)
    -> Result<(), Errno>
{
    if hostname_ptr == 0 || result_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let hostname_len = hostname_len as usize;
    if hostname_len == 0 || hostname_len > 253 {
        return Err(Errno::EINVAL);
    }

    let mut hostname_buf = [0u8; 253];
    let user_hostname = MmUserBytes::try_new(hostname_ptr, hostname_len).map_err(|_| Errno::EFAULT)?;
    let copied = copy_bytes_from_user(user_hostname, &mut hostname_buf[..hostname_len])
        .map_err(|_| Errno::EFAULT)?;
    if copied != hostname_len {
        return Err(Errno::EFAULT);
    }

    let result_addr = match dns::dns_resolve(&hostname_buf[..hostname_len]) {
        Ok(addr) => addr,
        Err(dns::DnsResolveError::InvalidHostname) => return Err(Errno::EINVAL),
        Err(dns::DnsResolveError::NoDnsServer) => return Err(Errno::ENETUNREACH),
        Err(dns::DnsResolveError::Timeout | dns::DnsResolveError::TransmitFailed) => {
            return Err(Errno::EAGAIN);
        }
        Err(dns::DnsResolveError::ParseFailed) => return Err(Errno::EHOSTUNREACH),
    };

    let user_result = MmUserBytes::try_new(result_ptr, 4).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user_result, &result_addr).map_err(|_| Errno::EFAULT)?;

    Ok(())
});

// ---------------------------------------------------------------------------
// sendmsg / recvmsg — fd passing via SCM_RIGHTS
// ---------------------------------------------------------------------------

define_syscall!(syscall_sendmsg
    (ctx, fd: Fd, msg_ptr: UserPtr<slopos_abi::syscall::MsgHdr>, _flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    use slopos_abi::syscall::{CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS};

    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    let sh = match sock_fd {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return Err(Errno::ENOTSOCK),
    };

    let msg: MsgHdr = copy_from_user(msg_ptr.inner()).map_err(|_| Errno::EFAULT)?;

    let data_len = (msg.iov_len as usize).min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;
    if data_len > 0 && msg.iov_base != 0 {
        let user_data = MmUserBytes::try_new(msg.iov_base, data_len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_data, &mut scratch[..data_len]).map_err(|_| Errno::EFAULT)?;
    }

    // Owned aliases of the fds being passed. Each shares the sender's
    // open-file description (offset, flags, backing) per POSIX fd-passing
    // semantics; on any error return the vec drops, closing them.
    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(SCM_MAX_FDS).map_err(|_| Errno::ENOMEM)?;

    if msg.control_len >= core::mem::size_of::<CmsgHdr>() as u64 && msg.control != 0 {
        let cmsg_ptr = MmUserPtr::<CmsgHdr>::try_new(msg.control).map_err(|_| Errno::EFAULT)?;
        let cmsg: CmsgHdr = copy_from_user(cmsg_ptr).map_err(|_| Errno::EFAULT)?;

        if cmsg.cmsg_type == SCM_RIGHTS {
            let hdr_size = core::mem::size_of::<CmsgHdr>();
            let fd_data_len = cmsg.cmsg_len as usize - hdr_size;
            let n_fds = (fd_data_len / 4).min(SCM_MAX_FDS);

            if n_fds > 0 {
                let fd_array_addr = msg.control + hdr_size as u64;
                let mut fd_buf = [0i32; SCM_MAX_FDS];
                let fd_bytes = n_fds * 4;
                let user_fds = MmUserBytes::try_new(fd_array_addr, fd_bytes)
                    .map_err(|_| Errno::EFAULT)?;
                let fd_buf_bytes =
                    &mut slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(&mut fd_buf)
                        [..fd_bytes];
                copy_bytes_from_user(user_fds, fd_buf_bytes).map_err(|_| Errno::EFAULT)?;

                for &send_fd in fd_buf.iter().take(n_fds) {
                    let file = slopos_fs::fileio_clone_file_ref(process_id, send_fd)
                        .ok_or(Errno::EBADF)?;
                    files.push(file).map_err(|_| Errno::ENOMEM)?;
                }
            }
        }
    }

    let rc = unix_socket::unix_sendmsg(sh, &scratch[..data_len], &mut files);
    if rc < 0 {
        // Uncommitted aliases drop with `files`.
        return Err(errno_from_neg(rc));
    }
    Ok(rc as u64)
});

#[inline(never)]
fn recvmsg_writeback_cmsg(
    process_id: u32,
    msg: &slopos_abi::syscall::MsgHdr,
    mut received: slopos_ostd::KVec<slopos_fs::FileRef>,
    msg_ptr: UserPtr<slopos_abi::syscall::MsgHdr>,
) -> Result<(), Errno> {
    use slopos_abi::syscall::{CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS};

    let n_fds = received.len();
    if msg.control == 0 {
        // No control buffer to report them in — the aliases drop.
        return Ok(());
    }

    let hdr_size = core::mem::size_of::<CmsgHdr>();
    let needed = hdr_size + n_fds * 4;
    if (msg.control_len as usize) < needed {
        drop(received);
        let updated_msg = MsgHdr {
            iov_base: msg.iov_base,
            iov_len: msg.iov_len,
            control: msg.control,
            control_len: 0,
        };
        copy_to_user(msg_ptr.inner(), &updated_msg).map_err(|_| Errno::EFAULT)?;
        return Ok(());
    }

    debug_assert!(n_fds <= SCM_MAX_FDS);
    let mut fd_nums = [0i32; SCM_MAX_FDS];
    for (j, file) in received.drain(..).enumerate() {
        let new_fd = slopos_fs::fileio_install_file_ref(process_id, file);
        if new_fd < 0 {
            // The failed install dropped its alias; ending the drain drops
            // the rest. Roll back the fds installed so far (a partial
            // install with no surviving cmsg writeback would orphan them).
            for &fd in fd_nums.iter().take(j) {
                let _ = slopos_fs::fileio::file_close_fd(process_id, fd);
            }
            return Err(Errno::ENOMEM);
        }
        fd_nums[j] = new_fd;
    }

    // From here the fds are installed in the caller's table. If any copy
    // back to user memory faults, close every installed fd before
    // returning the error — otherwise the caller never learns the fd
    // numbers and the fds are orphaned (fd-table-exhaustion DoS).
    let writeback = || -> Result<(), Errno> {
        let cmsg = CmsgHdr {
            cmsg_len: needed as u32,
            cmsg_level: SOL_SOCKET as u32,
            cmsg_type: SCM_RIGHTS,
        };
        let cmsg_ptr = MmUserPtr::<CmsgHdr>::try_new(msg.control).map_err(|_| Errno::EFAULT)?;
        copy_to_user(cmsg_ptr, &cmsg).map_err(|_| Errno::EFAULT)?;

        let fd_bytes = &slopos_ostd::util::byte_view::pod_slice_as_bytes(&fd_nums[..])[..n_fds * 4];
        let fd_out = MmUserBytes::try_new(msg.control + hdr_size as u64, n_fds * 4)
            .map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(fd_out, fd_bytes).map_err(|_| Errno::EFAULT)?;

        let updated_msg = MsgHdr {
            iov_base: msg.iov_base,
            iov_len: msg.iov_len,
            control: msg.control,
            control_len: needed as u64,
        };
        copy_to_user(msg_ptr.inner(), &updated_msg).map_err(|_| Errno::EFAULT)?;
        Ok(())
    };

    if let Err(e) = writeback() {
        for &fd in fd_nums.iter().take(n_fds) {
            let _ = slopos_fs::fileio::file_close_fd(process_id, fd);
        }
        return Err(e);
    }
    Ok(())
}

#[inline(never)]
fn recvmsg_impl(
    process_id: u32,
    fd: Fd,
    msg_ptr: UserPtr<slopos_abi::syscall::MsgHdr>,
) -> Result<u64, Errno> {
    use slopos_abi::syscall::{MsgHdr, SCM_MAX_FDS};

    let sh = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return Err(Errno::ENOTSOCK),
    };

    let msg: MsgHdr = copy_from_user(msg_ptr.inner()).map_err(|_| Errno::EFAULT)?;

    let data_len = (msg.iov_len as usize).min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;

    let mut received: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(SCM_MAX_FDS).map_err(|_| Errno::ENOMEM)?;
    let (bytes_read, n_fds) =
        unix_socket::unix_recvmsg(sh, &mut scratch[..data_len], &mut received, SCM_MAX_FDS);

    if bytes_read < 0 {
        // `received` drops, closing any drained aliases.
        return Err(errno_from_neg(bytes_read));
    }

    let copied = bytes_read as usize;
    if copied > 0 && msg.iov_base != 0 {
        let user_out = MmUserBytes::try_new(msg.iov_base, copied).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_out, &scratch[..copied]).map_err(|_| Errno::EFAULT)?;
    }

    if n_fds > 0 {
        recvmsg_writeback_cmsg(process_id, &msg, received, msg_ptr)?;
    }

    Ok(copied as u64)
}

define_syscall!(syscall_recvmsg
    (ctx, fd: Fd, msg_ptr: UserPtr<slopos_abi::syscall::MsgHdr>, _flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    recvmsg_impl(process_id, fd, msg_ptr)
});

// `getsockname` / `getpeername` need to materialise a `SockAddrUn`
// (110 bytes) on the unix branch; splitting the branches into
// `#[inline(never)]` helpers keeps the dispatch closure's stack frame
// out of the union of both branches' locals (which together would
// blow the 2 KiB stack-frame gate).
#[inline(never)]
fn write_unix_sockaddr(
    addr_un: &SockAddrUn,
    path_len: usize,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let struct_len = 2 + path_len;
    let user_len_ptr = MmUserPtr::<u32>::try_new(addrlen_ptr).map_err(|_| Errno::EFAULT)?;
    let caller_len = copy_from_user(user_len_ptr).map_err(|_| Errno::EFAULT)? as usize;
    let copy_len = caller_len.min(struct_len);

    if copy_len > 0 {
        let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(addr_un);
        let user_buf = MmUserBytes::try_new(addr_buf, copy_len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]).map_err(|_| Errno::EFAULT)?;
    }
    let actual = struct_len as u32;
    copy_to_user(user_len_ptr, &actual).map_err(|_| Errno::EFAULT)?;
    Ok(())
}

#[inline(never)]
fn write_inet_sockaddr(
    sock_addr_in: &SockAddrIn,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let struct_len = core::mem::size_of::<SockAddrIn>();
    let user_len_ptr = MmUserPtr::<u32>::try_new(addrlen_ptr).map_err(|_| Errno::EFAULT)?;
    let caller_len = copy_from_user(user_len_ptr).map_err(|_| Errno::EFAULT)? as usize;
    let copy_len = caller_len.min(struct_len);

    if copy_len > 0 {
        let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(sock_addr_in);
        let user_buf = MmUserBytes::try_new(addr_buf, copy_len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]).map_err(|_| Errno::EFAULT)?;
    }
    let actual = struct_len as u32;
    copy_to_user(user_len_ptr, &actual).map_err(|_| Errno::EFAULT)?;
    Ok(())
}

#[inline(never)]
fn getpeername_unix(sh: SocketHandle, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let (path, path_len) = unix_socket::unix_get_peer_path(sh).ok_or(Errno::ENOTCONN)?;
    let mut addr_un = SockAddrUn::default();
    addr_un.family = AF_UNIX;
    if path_len > 0 {
        addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
    }
    write_unix_sockaddr(&addr_un, path_len, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getpeername_inet(sock_idx: u32, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let peer = socket::socket_get_peer_addr(sock_idx).ok_or(Errno::ENOTCONN)?;
    let sock_addr_in = peer.to_user();
    write_inet_sockaddr(&sock_addr_in, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getsockname_unix(sh: SocketHandle, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let path_len = unix_socket::unix_get_local_path_len(sh);
    let mut addr_un = SockAddrUn::default();
    addr_un.family = AF_UNIX;
    if path_len > 0 {
        if let Some(path) = unix_socket::unix_get_local_path(sh) {
            addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
        }
    }
    write_unix_sockaddr(&addr_un, path_len, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getsockname_inet(sock_idx: u32, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let local = match socket::socket_get_local_addr(sock_idx) {
        Some(l) => l,
        None => {
            // POSIX: getsockname on an unbound socket returns
            // AF_INET + zero/zero rather than EINVAL.
            let zeroed = SockAddrIn {
                family: AF_INET,
                port: 0,
                addr: [0; 4],
                _pad: [0; 8],
            };
            return write_inet_sockaddr(&zeroed, addr_buf, addrlen_ptr);
        }
    };
    let sock_addr_in = local.to_user();
    write_inet_sockaddr(&sock_addr_in, addr_buf, addrlen_ptr)
}

define_syscall!(syscall_getpeername
    (ctx, fd: Fd, addr_buf: u64, addrlen_ptr: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    if addr_buf == 0 || addrlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Unix(sh) => getpeername_unix(sh, addr_buf, addrlen_ptr),
        SocketFd::Inet(sock_idx) => getpeername_inet(sock_idx, addr_buf, addrlen_ptr),
    }
});

define_syscall!(syscall_getsockname
    (ctx, fd: Fd, addr_buf: u64, addrlen_ptr: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    if addr_buf == 0 || addrlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Unix(sh) => getsockname_unix(sh, addr_buf, addrlen_ptr),
        SocketFd::Inet(sock_idx) => getsockname_inet(sock_idx, addr_buf, addrlen_ptr),
    }
});
