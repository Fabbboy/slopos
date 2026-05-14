use crate::syscall::common::SyscallDisposition;
use crate::syscall::context::SyscallContext;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::net::{AF_INET, AF_UNIX, IPPROTO_ICMP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn};
use slopos_abi::syscall::*;
use slopos_abi::unix::SockAddrUn;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};
use slopos_net::unix_socket::SocketHandle;
use slopos_net::{dns, socket, unix_socket, unix_socket_file_ops};

fn errno_i32(errno: i32) -> u64 {
    (errno as i64) as u64
}

fn rc_i32(ctx: &SyscallContext, rc: i32) -> SyscallDisposition {
    if rc < 0 {
        ctx.err_with(errno_i32(rc))
    } else {
        ctx.ok(rc as u64)
    }
}

fn rc_i64(ctx: &SyscallContext, rc: i64) -> SyscallDisposition {
    if rc < 0 {
        ctx.err_with(rc as u64)
    } else {
        ctx.ok(rc as u64)
    }
}

/// Socket fd lookup result: either a unix socket handle or a raw IP socket index.
enum SocketFd {
    /// AF_UNIX socket — contains a [`SocketHandle`].
    Unix(SocketHandle),
    /// AF_INET socket — contains the raw socket pool index.
    Inet(u32),
}

/// Check whether a `&'static dyn FileOps` points to the unix socket ops
/// singleton by comparing the data pointer of the trait object.
fn is_unix_socket_ops(ops: &'static dyn FileOps) -> bool {
    let data = ops as *const dyn FileOps as *const () as usize;
    let unix_data = &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS as *const _ as *const () as usize;
    data == unix_data
}

/// Retrieve the socket handle for `fd`, distinguishing AF_UNIX from AF_INET.
fn socket_fd_for(process_id: u32, fd: i32) -> Result<SocketFd, u64> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(process_id, fd) else {
        return Err(ERRNO_ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(ERRNO_ENOTSOCK);
    }
    if is_unix_socket_ops(ops) {
        Ok(SocketFd::Unix(SocketHandle::from_usize(handle)))
    } else {
        Ok(SocketFd::Inet(handle as u32))
    }
}

define_syscall!(syscall_socket(ctx, args) requires(let process_id) {
    let domain = args.arg0 as u16;
    let sock_type = args.arg1 as u16;
    let protocol = args.arg2 as u16;

    if domain == AF_UNIX {
        if sock_type != SOCK_STREAM {
            return ctx.err_with(ERRNO_EPROTONOSUPPORT);
        }
        let Some(handle) = unix_socket::unix_create() else {
            return ctx.err_with(ERRNO_ENOMEM);
        };
        let fd = slopos_fs::fileio_open_fd_with_ops(
            process_id,
            &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
            handle.as_usize(),
        );
        if fd < 0 {
            let _ = unix_socket::unix_close(handle);
            return ctx.err_with(ERRNO_ENOMEM);
        }
        return ctx.ok(fd as u64);
    }

    if domain != AF_INET {
        return ctx.err_with(ERRNO_EAFNOSUPPORT);
    }
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return ctx.err_with(ERRNO_EPROTONOSUPPORT);
    }
    let _icmp_datagram = sock_type == SOCK_DGRAM && protocol == IPPROTO_ICMP;

    let sock_idx = socket::socket_create(domain, sock_type, protocol);
    if sock_idx < 0 {
        return ctx.err_with(errno_i32(sock_idx));
    }

    let fd = slopos_fs::fileio_open_socket_fd(process_id, sock_idx as u32);
    if fd < 0 {
        let _ = socket::socket_close(sock_idx as u32);
        return ctx.err_with(ERRNO_ENOMEM);
    }

    ctx.ok(fd as u64)
});

define_syscall!(syscall_bind(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    match sock_fd {
        SocketFd::Unix(sh) => {
            // AF_UNIX bind: parse SockAddrUn from userspace.
            let addr_len = args.arg2_usize();
            if addr_len < 4 {
                return ctx.err_with(ERRNO_EINVAL);
            }
            let user_addr = try_or_err!(ctx, UserPtr::<SockAddrUn>::try_new(args.arg1));
            let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return ctx.err_with(ERRNO_EINVAL);
            }
            rc_i32(&ctx, unix_socket::unix_bind(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
                return ctx.err_with(ERRNO_EINVAL);
            }
            let user_addr = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg1));
            let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
            let port = u16::from_be(sock_addr.port);
            rc_i32(&ctx, socket::socket_bind(sock_idx, sock_addr.addr, port))
        }
    }
});

define_syscall!(syscall_listen(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let backlog = args.arg1_u32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };
    match sock_fd {
        SocketFd::Unix(sh) => rc_i32(&ctx, unix_socket::unix_listen(sh, backlog)),
        SocketFd::Inet(sock_idx) => rc_i32(&ctx, socket::socket_listen(sock_idx, backlog)),
    }
});

define_syscall!(syscall_accept(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            let accepted_handle = match unix_socket::unix_accept(sh) {
                Ok(h) => h,
                Err(errno) => return ctx.err_with(errno_i32(errno)),
            };
            let new_fd = slopos_fs::fileio_open_fd_with_ops(
                process_id,
                &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                accepted_handle.as_usize(),
            );
            if new_fd < 0 {
                let _ = unix_socket::unix_close(accepted_handle);
                return ctx.err_with(ERRNO_ENOMEM);
            }
            ctx.ok(new_fd as u64)
        }
        SocketFd::Inet(sock_idx) => {
            let mut peer_ip = [0u8; 4];
            let mut peer_port = 0u16;
            let want_peer = args.arg1 != 0;
            if want_peer && args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
                return ctx.err_with(ERRNO_EINVAL);
            }

            let accepted_idx = socket::socket_accept(
                sock_idx,
                if want_peer {
                    &mut peer_ip as *mut [u8; 4]
                } else {
                    core::ptr::null_mut()
                },
                if want_peer {
                    &mut peer_port as *mut u16
                } else {
                    core::ptr::null_mut()
                },
            );
            if accepted_idx < 0 {
                return ctx.err_with(errno_i32(accepted_idx));
            }

            let new_fd = slopos_fs::fileio_open_socket_fd(process_id, accepted_idx as u32);
            if new_fd < 0 {
                let _ = socket::socket_close(accepted_idx as u32);
                return ctx.err_with(ERRNO_ENOMEM);
            }

            if want_peer {
                let peer = SockAddrIn {
                    family: AF_INET,
                    port: peer_port.to_be(),
                    addr: peer_ip,
                    _pad: [0; 8],
                };
                let user_peer = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg1));
                try_or_err!(ctx, copy_to_user(user_peer, &peer));
            }

            ctx.ok(new_fd as u64)
        }
    }
});

define_syscall!(syscall_connect(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    match sock_fd {
        SocketFd::Unix(sh) => {
            let addr_len = args.arg2_usize();
            if addr_len < 4 {
                return ctx.err_with(ERRNO_EINVAL);
            }
            let user_addr = try_or_err!(ctx, UserPtr::<SockAddrUn>::try_new(args.arg1));
            let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return ctx.err_with(ERRNO_EINVAL);
            }
            rc_i32(&ctx, unix_socket::unix_connect(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
                return ctx.err_with(ERRNO_EINVAL);
            }
            let user_addr = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg1));
            let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
            let port = u16::from_be(sock_addr.port);
            rc_i32(&ctx, socket::socket_connect(sock_idx, sock_addr.addr, port))
        }
    }
});

define_syscall!(syscall_send(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            if len > 0 {
                let user_data = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, len));
                let copied = try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_data, &mut scratch[..len]));
                return rc_i32(&ctx, unix_socket::unix_send(sh, &scratch[..copied]));
            }
            ctx.ok(0)
        }
        SocketFd::Inet(sock_idx) => {
            if len > 0 {
                let user_data = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, len));
                let copied = try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_data, &mut scratch[..len]));
                return rc_i64(&ctx, socket::socket_send(sock_idx, scratch.as_ptr(), copied));
            }
            rc_i64(&ctx, socket::socket_send(sock_idx, core::ptr::null(), 0))
        }
    }
});

define_syscall!(syscall_recv(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            let rc = unix_socket::unix_recv(sh, &mut scratch[..len]);
            if rc < 0 {
                return ctx.err_with(errno_i32(rc));
            }
            let copied = rc as usize;
            if copied > 0 {
                let user_out = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, copied));
                try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_out, &scratch[..copied]));
            }
            ctx.ok(copied as u64)
        }
        SocketFd::Inet(sock_idx) => {
            let rc = socket::socket_recv(sock_idx, scratch.as_mut_ptr(), len);
            if rc < 0 {
                return ctx.err_with(rc as u64);
            }
            let copied = rc as usize;
            if copied > 0 {
                let user_out = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, copied));
                try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_out, &scratch[..copied]));
            }
            ctx.ok(copied as u64)
        }
    }
});

define_syscall!(syscall_sendto(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_fd_for(process_id, fd) {
        Ok(SocketFd::Inet(idx)) => idx,
        Ok(SocketFd::Unix(_)) => return ctx.err_with(ERRNO_ENOTSOCK),
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }
    if args.arg4 == 0 {
        return ctx.err_with(ERRNO_EDESTADDRREQ);
    }
    if args.arg5_usize() < core::mem::size_of::<SockAddrIn>() {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let user_addr = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg4));
    let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
    if sock_addr.family != AF_INET {
        return ctx.err_with(ERRNO_EAFNOSUPPORT);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };
    let copied = if len > 0 {
        let user_data = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, len));
        try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_data, &mut scratch[..len]))
    } else {
        0
    };

    rc_i64(
        &ctx,
        socket::socket_sendto(
            sock_idx,
            if copied == 0 {
                core::ptr::null()
            } else {
                scratch.as_ptr()
            },
            copied,
            sock_addr.addr,
            u16::from_be(sock_addr.port),
        ),
    )
});

define_syscall!(syscall_recvfrom(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_fd_for(process_id, fd) {
        Ok(SocketFd::Inet(idx)) => idx,
        Ok(SocketFd::Unix(_)) => return ctx.err_with(ERRNO_ENOTSOCK),
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let want_src = args.arg4 != 0;
    if want_src && args.arg5_usize() < core::mem::size_of::<SockAddrIn>() {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };
    let mut src_ip = [0u8; 4];
    let mut src_port = 0u16;

    let rc = socket::socket_recvfrom(
        sock_idx,
        if len == 0 {
            core::ptr::null_mut()
        } else {
            scratch.as_mut_ptr()
        },
        len,
        if want_src {
            &mut src_ip as *mut [u8; 4]
        } else {
            core::ptr::null_mut()
        },
        if want_src {
            &mut src_port as *mut u16
        } else {
            core::ptr::null_mut()
        },
    );
    if rc < 0 {
        return ctx.err_with(rc as u64);
    }

    let copied = rc as usize;
    if copied > 0 {
        let user_out = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, copied));
        try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_out, &scratch[..copied]));
    }

    if want_src {
        let peer = SockAddrIn {
            family: AF_INET,
            port: src_port.to_be(),
            addr: src_ip,
            _pad: [0; 8],
        };
        let user_peer = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg4));
        try_or_err!(ctx, copy_to_user(user_peer, &peer));
    }

    ctx.ok(copied as u64)
});

define_syscall!(syscall_setsockopt(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_fd_for(process_id, fd) {
        Ok(SocketFd::Inet(idx)) => idx,
        Ok(SocketFd::Unix(_)) => return ctx.err_with(ERRNO_ENOTSOCK),
        Err(errno) => return ctx.err_with(errno),
    };

    let level = args.arg1 as i32;
    let optname = args.arg2 as i32;
    let optval_ptr = args.arg3;
    let optlen = args.arg4_usize();

    if optval_ptr == 0 && optlen > 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let optlen = optlen.min(64);
    let mut scratch = [0u8; 64];
    if optlen > 0 {
        let user_data = try_or_err!(ctx, UserBytes::try_new(optval_ptr, optlen));
        try_or_err!(ctx, copy_bytes_from_user(user_data, &mut scratch[..optlen]));
    }

    rc_i32(
        &ctx,
        socket::socket_setsockopt(sock_idx, level, optname, &scratch[..optlen]),
    )
});

define_syscall!(syscall_getsockopt(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_fd_for(process_id, fd) {
        Ok(SocketFd::Inet(idx)) => idx,
        Ok(SocketFd::Unix(_)) => return ctx.err_with(ERRNO_ENOTSOCK),
        Err(errno) => return ctx.err_with(errno),
    };

    let level = args.arg1 as i32;
    let optname = args.arg2 as i32;
    let optval_ptr = args.arg3;
    let optlen_ptr = args.arg4;

    if optval_ptr == 0 || optlen_ptr == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let user_optlen = try_or_err!(ctx, UserPtr::<u32>::try_new(optlen_ptr));
    let optlen = try_or_err!(ctx, copy_from_user(user_optlen)) as usize;
    let optlen = optlen.min(64);

    let mut scratch = [0u8; 64];
    let rc = socket::socket_getsockopt(sock_idx, level, optname, &mut scratch[..optlen]);
    if rc < 0 {
        return ctx.err_with(errno_i32(rc));
    }

    let written = rc as usize;
    if written > 0 {
        let user_data = try_or_err!(ctx, UserBytes::try_new(optval_ptr, written));
        try_or_err!(ctx, copy_bytes_to_user(user_data, &scratch[..written]));
    }

    let actual_len = written as u32;
    try_or_err!(ctx, copy_to_user(user_optlen, &actual_len));

    ctx.ok(0)
});

define_syscall!(syscall_shutdown(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_fd_for(process_id, fd) {
        Ok(SocketFd::Inet(idx)) => idx,
        Ok(SocketFd::Unix(_)) => return ctx.err_with(ERRNO_ENOTSOCK),
        Err(errno) => return ctx.err_with(errno),
    };

    let how = args.arg1 as i32;
    rc_i32(&ctx, socket::socket_shutdown(sock_idx, how))
});

define_syscall!(syscall_resolve(ctx, args) requires(let process_id) {
    // arg0 = hostname pointer, arg1 = hostname length, arg2 = result pointer
    if args.arg0 == 0 || args.arg2 == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let hostname_len = args.arg1_usize();
    if hostname_len == 0 || hostname_len > 253 {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let mut hostname_buf = [0u8; 253];
    let user_hostname = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg0, hostname_len));
    let copied = try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_hostname, &mut hostname_buf[..hostname_len]));
    if copied != hostname_len {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let result_addr = match dns::dns_resolve(&hostname_buf[..hostname_len]) {
        Ok(addr) => addr,
        Err(dns::DnsResolveError::InvalidHostname) => return ctx.err_with(ERRNO_EINVAL),
        Err(dns::DnsResolveError::NoDnsServer) => return ctx.err_with(ERRNO_ENETUNREACH),
        Err(dns::DnsResolveError::Timeout | dns::DnsResolveError::TransmitFailed) => {
            return ctx.err_with(ERRNO_EAGAIN);
        }
        Err(dns::DnsResolveError::ParseFailed) => return ctx.err_with(ERRNO_EHOSTUNREACH),
    };

    let user_result = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg2, 4));
    try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_result, &result_addr));

    ctx.ok(0)
});

// ---------------------------------------------------------------------------
// sendmsg / recvmsg — fd passing via SCM_RIGHTS
// ---------------------------------------------------------------------------

define_syscall!(syscall_sendmsg(ctx, args) requires(let process_id) {
    use slopos_abi::syscall::{MsgHdr, CmsgHdr, SCM_RIGHTS, SCM_MAX_FDS};

    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };
    let sh = match sock_fd {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return ctx.err_with(ERRNO_ENOTSOCK),
    };

    // Copy MsgHdr from userspace.
    let msg_ptr = try_or_err!(ctx, UserPtr::<MsgHdr>::try_new(args.arg1));
    let msg: MsgHdr = try_or_err!(ctx, copy_from_user(msg_ptr));

    // Copy data bytes.
    let data_len = (msg.iov_len as usize).min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };
    if data_len > 0 && msg.iov_base != 0 {
        let user_data = try_or_err!(ctx, UserBytes::try_new(msg.iov_base, data_len));
        try_or_err!(ctx, copy_bytes_from_user(user_data, &mut scratch[..data_len]));
    }

    // Parse ancillary data for SCM_RIGHTS fds.
    let mut inflight: [(usize, &'static dyn slopos_abi::file_ops::FileOps); SCM_MAX_FDS] =
        [(0, slopos_mm::memfd::dummy_file_ops()); SCM_MAX_FDS];
    let mut fd_count = 0usize;

    if msg.control_len >= core::mem::size_of::<CmsgHdr>() as u64 && msg.control != 0 {
        // Read CmsgHdr
        let cmsg_ptr = try_or_err!(ctx, UserPtr::<CmsgHdr>::try_new(msg.control));
        let cmsg: CmsgHdr = try_or_err!(ctx, copy_from_user(cmsg_ptr));

        if cmsg.cmsg_type == SCM_RIGHTS {
            let hdr_size = core::mem::size_of::<CmsgHdr>();
            let fd_data_len = cmsg.cmsg_len as usize - hdr_size;
            let n_fds = (fd_data_len / 4).min(SCM_MAX_FDS);

            if n_fds > 0 {
                let fd_array_addr = msg.control + hdr_size as u64;
                let mut fd_buf = [0i32; SCM_MAX_FDS];
                let fd_bytes = n_fds * 4;
                let user_fds = try_or_err!(ctx, UserBytes::try_new(fd_array_addr, fd_bytes));
                // Read raw bytes then reinterpret as i32 array.
                let fd_buf_bytes =
                    &mut slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(&mut fd_buf)
                        [..fd_bytes];
                try_or_err!(ctx, copy_bytes_from_user(user_fds, fd_buf_bytes));

                // Resolve and dup each fd
                for j in 0..n_fds {
                    let send_fd = fd_buf[j];
                    let Some((handle, ops)) =
                        slopos_fs::fileio::fileio_get_handle_and_ops(process_id, send_fd)
                    else {
                        // Release already-dup'd fds on error
                        for k in 0..fd_count {
                            inflight[k].1.release(inflight[k].0);
                        }
                        return ctx.err_with(ERRNO_ENOTSOCK); // closest available errno
                    };
                    // Dup to create a new reference
                    let Some(new_handle) = ops.dup(handle) else {
                        for k in 0..fd_count {
                            inflight[k].1.release(inflight[k].0);
                        }
                        return ctx.err_with(ERRNO_ENOMEM);
                    };
                    inflight[fd_count] = (new_handle, ops);
                    fd_count += 1;
                }
            }
        }
    }

    let rc = unix_socket::unix_sendmsg(
        sh,
        &scratch[..data_len],
        &mut inflight[..fd_count],
        fd_count,
    );
    if rc < 0 {
        // On error, release any fds that weren't consumed
        for j in 0..fd_count {
            if inflight[j].0 != 0 {
                inflight[j].1.release(inflight[j].0);
            }
        }
        return ctx.err_with(errno_i32(rc));
    }
    ctx.ok(rc as u64)
});

define_syscall!(syscall_recvmsg(ctx, args) requires(let process_id) {
    use slopos_abi::syscall::{MsgHdr, CmsgHdr, SCM_RIGHTS, SCM_MAX_FDS};

    let fd = args.arg0_i32();
    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };
    let sh = match sock_fd {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return ctx.err_with(ERRNO_ENOTSOCK),
    };

    // Copy MsgHdr from userspace.
    let msg_ptr = try_or_err!(ctx, UserPtr::<MsgHdr>::try_new(args.arg1));
    let msg: MsgHdr = try_or_err!(ctx, copy_from_user(msg_ptr));

    let data_len = (msg.iov_len as usize).min(4096);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(4096) {
        Ok(v) => v,
        Err(_) => return ctx.err_with(ERRNO_ENOMEM),
    };

    // Receive data + fds
    let mut received_fds: [(usize, &'static dyn slopos_abi::file_ops::FileOps); SCM_MAX_FDS] =
        [(0, slopos_mm::memfd::dummy_file_ops()); SCM_MAX_FDS];
    let (bytes_read, n_fds) = unix_socket::unix_recvmsg(
        sh,
        &mut scratch[..data_len],
        &mut received_fds,
        SCM_MAX_FDS,
    );

    if bytes_read < 0 {
        // Release any fds we received despite the error
        for j in 0..n_fds {
            received_fds[j].1.release(received_fds[j].0);
        }
        return ctx.err_with(errno_i32(bytes_read));
    }

    // Copy data to user.
    let copied = bytes_read as usize;
    if copied > 0 && msg.iov_base != 0 {
        let user_out = try_or_err!(ctx, UserBytes::try_new(msg.iov_base, copied));
        try_or_err!(ctx, copy_bytes_to_user(user_out, &scratch[..copied]));
    }

    // Install received fds into the calling process's fd table and build cmsg.
    if n_fds > 0 && msg.control != 0 {
        let hdr_size = core::mem::size_of::<CmsgHdr>();
        let needed = hdr_size + n_fds * 4;
        if msg.control_len as usize >= needed {
            let mut fd_nums = [0i32; SCM_MAX_FDS];
            for j in 0..n_fds {
                let (handle, ops) = received_fds[j];
                let new_fd = slopos_fs::fileio::fileio_open_fd_with_ops(process_id, ops, handle);
                if new_fd < 0 {
                    // Failed to install — release this and remaining
                    ops.release(handle);
                    for k in (j + 1)..n_fds {
                        received_fds[k].1.release(received_fds[k].0);
                    }
                    return ctx.err_with(ERRNO_ENOMEM);
                }
                fd_nums[j] = new_fd;
            }

            // Write CmsgHdr to user
            let cmsg = CmsgHdr {
                cmsg_len: needed as u32,
                cmsg_level: SOL_SOCKET as u32,
                cmsg_type: SCM_RIGHTS,
            };
            let cmsg_ptr = try_or_err!(ctx, UserPtr::<CmsgHdr>::try_new(msg.control));
            try_or_err!(ctx, copy_to_user(cmsg_ptr, &cmsg));

            // Write fd array after header
            let fd_bytes =
                &slopos_ostd::util::byte_view::pod_slice_as_bytes(&fd_nums[..])[..n_fds * 4];
            let fd_out = try_or_err!(ctx, UserBytes::try_new(msg.control + hdr_size as u64, n_fds * 4));
            try_or_err!(ctx, copy_bytes_to_user(fd_out, fd_bytes));

            // Update control_len in the user's MsgHdr
            let updated_msg = MsgHdr {
                iov_base: msg.iov_base,
                iov_len: msg.iov_len,
                control: msg.control,
                control_len: needed as u64,
            };
            try_or_err!(ctx, copy_to_user(msg_ptr, &updated_msg));
        } else {
            // Not enough space — release fds
            for j in 0..n_fds {
                received_fds[j].1.release(received_fds[j].0);
            }
            // Zero out control_len to indicate no ancillary data
            let updated_msg = MsgHdr {
                iov_base: msg.iov_base,
                iov_len: msg.iov_len,
                control: msg.control,
                control_len: 0,
            };
            try_or_err!(ctx, copy_to_user(msg_ptr, &updated_msg));
        }
    } else if n_fds > 0 {
        // No control buffer provided — release received fds
        for j in 0..n_fds {
            received_fds[j].1.release(received_fds[j].0);
        }
    }

    ctx.ok(copied as u64)
});

// ---------------------------------------------------------------------------
// getpeername / getsockname
// ---------------------------------------------------------------------------

define_syscall!(syscall_getpeername(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let addr_buf = args.arg1;
    let addrlen_ptr = args.arg2;

    if addr_buf == 0 || addrlen_ptr == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            // For AF_UNIX, return the peer's bound path as SockAddrUn.
            let Some((path, path_len)) = unix_socket::unix_get_peer_path(sh) else {
                return ctx.err_with(ERRNO_ENOTCONN);
            };
            let mut addr_un = SockAddrUn::default();
            addr_un.family = AF_UNIX;
            if path_len > 0 {
                addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
            }
            let struct_len = 2 + path_len; // family + path bytes

            // Read caller's buffer length.
            let user_len_ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(addrlen_ptr));
            let caller_len = try_or_err!(ctx, copy_from_user(user_len_ptr)) as usize;
            let copy_len = caller_len.min(struct_len);

            if copy_len > 0 {
                let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&addr_un);
                let user_buf = try_or_err!(ctx, UserBytes::try_new(addr_buf, copy_len));
                try_or_err!(ctx, copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]));
            }

            // Write back actual length.
            let actual = struct_len as u32;
            try_or_err!(ctx, copy_to_user(user_len_ptr, &actual));
            ctx.ok(0)
        }
        SocketFd::Inet(sock_idx) => {
            let Some(peer) = socket::socket_get_peer_addr(sock_idx) else {
                return ctx.err_with(ERRNO_ENOTCONN);
            };
            let sock_addr_in = peer.to_user();
            let struct_len = core::mem::size_of::<SockAddrIn>();

            // Read caller's buffer length.
            let user_len_ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(addrlen_ptr));
            let caller_len = try_or_err!(ctx, copy_from_user(user_len_ptr)) as usize;
            let copy_len = caller_len.min(struct_len);

            if copy_len > 0 {
                let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&sock_addr_in);
                let user_buf = try_or_err!(ctx, UserBytes::try_new(addr_buf, copy_len));
                try_or_err!(ctx, copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]));
            }

            // Write back actual length.
            let actual = struct_len as u32;
            try_or_err!(ctx, copy_to_user(user_len_ptr, &actual));
            ctx.ok(0)
        }
    }
});

define_syscall!(syscall_getsockname(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let addr_buf = args.arg1;
    let addrlen_ptr = args.arg2;

    if addr_buf == 0 || addrlen_ptr == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let sock_fd = match socket_fd_for(process_id, fd) {
        Ok(v) => v,
        Err(errno) => return ctx.err_with(errno),
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            // For AF_UNIX, return the socket's own bound path as SockAddrUn.
            let path_len = unix_socket::unix_get_local_path_len(sh);
            let mut addr_un = SockAddrUn::default();
            addr_un.family = AF_UNIX;
            if path_len > 0 {
                if let Some(path) = unix_socket::unix_get_local_path(sh) {
                    addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
                }
            }
            let struct_len = 2 + path_len; // family + path bytes

            // Read caller's buffer length.
            let user_len_ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(addrlen_ptr));
            let caller_len = try_or_err!(ctx, copy_from_user(user_len_ptr)) as usize;
            let copy_len = caller_len.min(struct_len);

            if copy_len > 0 {
                let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&addr_un);
                let user_buf = try_or_err!(ctx, UserBytes::try_new(addr_buf, copy_len));
                try_or_err!(ctx, copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]));
            }

            // Write back actual length.
            let actual = struct_len as u32;
            try_or_err!(ctx, copy_to_user(user_len_ptr, &actual));
            ctx.ok(0)
        }
        SocketFd::Inet(sock_idx) => {
            let Some(local) = socket::socket_get_local_addr(sock_idx) else {
                // Per POSIX, getsockname on an unbound socket returns
                // AF_INET + zeroed addr/port rather than EINVAL.
                let zeroed = SockAddrIn {
                    family: AF_INET,
                    port: 0,
                    addr: [0; 4],
                    _pad: [0; 8],
                };
                let struct_len = core::mem::size_of::<SockAddrIn>();
                let user_len_ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(addrlen_ptr));
                let caller_len = try_or_err!(ctx, copy_from_user(user_len_ptr)) as usize;
                let copy_len = caller_len.min(struct_len);
                if copy_len > 0 {
                    let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&zeroed);
                    let user_buf = try_or_err!(ctx, UserBytes::try_new(addr_buf, copy_len));
                    try_or_err!(ctx, copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]));
                }
                let actual = struct_len as u32;
                try_or_err!(ctx, copy_to_user(user_len_ptr, &actual));
                return ctx.ok(0);
            };
            let sock_addr_in = local.to_user();
            let struct_len = core::mem::size_of::<SockAddrIn>();

            // Read caller's buffer length.
            let user_len_ptr = try_or_err!(ctx, UserPtr::<u32>::try_new(addrlen_ptr));
            let caller_len = try_or_err!(ctx, copy_from_user(user_len_ptr)) as usize;
            let copy_len = caller_len.min(struct_len);

            if copy_len > 0 {
                let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&sock_addr_in);
                let user_buf = try_or_err!(ctx, UserBytes::try_new(addr_buf, copy_len));
                try_or_err!(ctx, copy_bytes_to_user(user_buf, &addr_bytes[..copy_len]));
            }

            // Write back actual length.
            let actual = struct_len as u32;
            try_or_err!(ctx, copy_to_user(user_len_ptr, &actual));
            ctx.ok(0)
        }
    }
});
