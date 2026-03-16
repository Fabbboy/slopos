use crate::syscall::common::SyscallDisposition;
use crate::syscall::context::SyscallContext;
use slopos_abi::net::{
    AF_INET, INVALID_SOCKET_IDX, IPPROTO_ICMP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn,
};
use slopos_abi::syscall::*;
use slopos_lib::kernel_services::syscall_services::socket;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

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
        ctx.err_with((rc as u64) as u64)
    } else {
        ctx.ok(rc as u64)
    }
}

fn socket_idx_for_fd(process_id: u32, fd: i32) -> Result<u32, u64> {
    let idx = slopos_fs::fileio_get_socket_idx(process_id, fd).unwrap_or(INVALID_SOCKET_IDX);
    if idx == INVALID_SOCKET_IDX {
        Err(ERRNO_ENOTSOCK)
    } else {
        Ok(idx)
    }
}

define_syscall!(syscall_socket(ctx, args) requires(let process_id) {
    let domain = args.arg0 as u16;
    let sock_type = args.arg1 as u16;
    let protocol = args.arg2 as u16;

    if domain != AF_INET {
        return ctx.err_with(ERRNO_EAFNOSUPPORT);
    }
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return ctx.err_with(ERRNO_EPROTONOSUPPORT);
    }
    let _icmp_datagram = sock_type == SOCK_DGRAM && protocol == IPPROTO_ICMP;

    let sock_idx = socket::create(domain, sock_type, protocol);
    if sock_idx < 0 {
        return ctx.err_with(errno_i32(sock_idx));
    }

    let fd = slopos_fs::fileio_open_socket_fd(process_id, sock_idx as u32);
    if fd < 0 {
        let _ = socket::close(sock_idx as u32);
        return ctx.err_with(ERRNO_ENOMEM);
    }

    ctx.ok(fd as u64)
});

define_syscall!(syscall_bind(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }
    if args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let user_addr = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg1));
    let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
    let port = u16::from_be(sock_addr.port);
    rc_i32(&ctx, socket::bind(sock_idx, sock_addr.addr, port))
});

define_syscall!(syscall_listen(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let backlog = args.arg1_u32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };
    rc_i32(&ctx, socket::listen(sock_idx, backlog))
});

define_syscall!(syscall_accept(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    let mut peer_ip = [0u8; 4];
    let mut peer_port = 0u16;
    let want_peer = args.arg1 != 0;
    if want_peer && args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let accepted_idx = socket::accept(
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
        let _ = socket::close(accepted_idx as u32);
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
});

define_syscall!(syscall_connect(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }
    if args.arg2_usize() < core::mem::size_of::<SockAddrIn>() {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let user_addr = try_or_err!(ctx, UserPtr::<SockAddrIn>::try_new(args.arg1));
    let sock_addr = try_or_err!(ctx, copy_from_user(user_addr));
    let port = u16::from_be(sock_addr.port);
    rc_i32(&ctx, socket::connect(sock_idx, sock_addr.addr, port))
});

define_syscall!(syscall_send(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = [0u8; 4096];
    if len > 0 {
        let user_data = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, len));
        let copied = try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_data, &mut scratch[..len]));
        return rc_i64(&ctx, socket::send(sock_idx, scratch.as_ptr(), copied));
    }

    rc_i64(&ctx, socket::send(sock_idx, core::ptr::null(), 0))
});

define_syscall!(syscall_recv(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    if args.arg1 == 0 && args.arg2 != 0 {
        return ctx.err_with(ERRNO_EFAULT);
    }

    let len = args.arg2_usize().min(4096);
    let mut scratch = [0u8; 4096];
    let rc = socket::recv(sock_idx, scratch.as_mut_ptr(), len);
    if rc < 0 {
        return ctx.err_with(rc as u64);
    }

    let copied = rc as usize;
    if copied > 0 {
        let user_out = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, copied));
        try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_out, &scratch[..copied]));
    }
    ctx.ok(copied as u64)
});

define_syscall!(syscall_sendto(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
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
    let mut scratch = [0u8; 4096];
    let copied = if len > 0 {
        let user_data = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg1, len));
        try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_data, &mut scratch[..len]))
    } else {
        0
    };

    rc_i64(
        &ctx,
        socket::sendto(
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
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
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
    let mut scratch = [0u8; 4096];
    let mut src_ip = [0u8; 4];
    let mut src_port = 0u16;

    let rc = socket::recvfrom(
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
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
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
        socket::setsockopt(sock_idx, level, optname, scratch[..optlen].as_ptr(), optlen),
    )
});

define_syscall!(syscall_getsockopt(ctx, args) requires(let process_id) {
    let fd = args.arg0_i32();
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
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
    let rc = socket::getsockopt(sock_idx, level, optname, scratch.as_mut_ptr(), optlen);
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
    let sock_idx = match socket_idx_for_fd(process_id, fd) {
        Ok(idx) => idx,
        Err(errno) => return ctx.err_with(errno),
    };

    let how = args.arg1 as i32;
    rc_i32(&ctx, socket::shutdown(sock_idx, how))
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

    // Copy hostname from user memory
    let mut hostname_buf = [0u8; 253];
    let user_hostname = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg0, hostname_len));
    let copied = try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_from_user(user_hostname, &mut hostname_buf[..hostname_len]));
    if copied != hostname_len {
        return ctx.err_with(ERRNO_EFAULT);
    }

    // Call DNS resolver via service
    use slopos_lib::kernel_services::syscall_services::dns;
    let mut result_addr = [0u8; 4];
    let rc = dns::resolve(
        hostname_buf[..hostname_len].as_ptr(),
        hostname_len,
        &mut result_addr as *mut [u8; 4],
    );

    if rc < 0 {
        return ctx.err_with(ERRNO_EHOSTUNREACH);
    }

    // Copy result to user memory
    let user_result = try_or_err!(ctx, slopos_mm::user_ptr::UserBytes::try_new(args.arg2, 4));
    try_or_err!(ctx, slopos_mm::user_copy::copy_bytes_to_user(user_result, &result_addr));

    ctx.ok(0)
});
