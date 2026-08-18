#![feature(restricted_std)]

//! SlopRing end-to-end userland test: opcode dispatch, deferred completion and
//! the `slopfut` executor, driven against real fds from a process fd table.

use slopos_userland as _;

use slopos_abi::net::{AF_INET, AF_UNIX, IPPROTO_ICMP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn};
use slopos_abi::ring::{
    BufIovec, OP_CLOSE, OP_CONNECT, OP_NOP, OP_OPENAT, OP_READ, OP_RECVFROM, OP_RECVMSG, OP_SEND,
    OP_SEND_ZC, OP_WRITE, RegisterBufRingCmd, SLOPRING_CQE_F_MORE, SLOPRING_CQE_F_NOTIF,
    SLOPRING_SQE_BUFFER_SELECT, SLOPRING_SQE_FIXED_BUFFER, Sqe,
};
use slopos_abi::unix::SockAddrUn;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::{fs, net};

/// The process's first ring lands at the mmap-window base, so touching its
/// mapping here catches a base-page mapping fault.
fn test_setup() -> bool {
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    if ring.sq_entries() != 8 || ring.fd() < 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_NOP;
    sqe.user_data = 0x5e7;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    matches!(ring.poll_completion(), Some(cqe) if cqe.user_data == 0x5e7 && cqe.res == 0)
}

fn test_nop() -> bool {
    let Ok(mut ring) = Ring::setup(4) else {
        return false;
    };
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_NOP;
    sqe.user_data = 0xa1;
    if ring.push_sqe(&sqe).is_err() {
        return false;
    }
    if ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xa1 && cqe.res == 0,
        None => false,
    }
}

fn test_write_read_pipe() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let payload = b"slopring-parity";
    let mut wsqe = Sqe::ZERO;
    wsqe.opcode = OP_WRITE;
    wsqe.fd = wr.raw();
    wsqe.addr = payload.as_ptr() as u64;
    wsqe.len = payload.len() as u32;
    wsqe.user_data = 0x770;
    if ring.push_sqe(&wsqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let wcqe = match ring.poll_completion() {
        Some(c) => c,
        None => return false,
    };
    if wcqe.res != payload.len() as i32 {
        return false;
    }

    let mut buf = [0u8; 32];
    let mut rsqe = Sqe::ZERO;
    rsqe.opcode = OP_READ;
    rsqe.fd = rd.raw();
    rsqe.addr = buf.as_mut_ptr() as u64;
    rsqe.len = buf.len() as u32;
    rsqe.user_data = 0x4ead;
    if ring.push_sqe(&rsqe).is_err() {
        return false;
    }
    if ring.submit_and_wait(1).is_err() {
        return false;
    }
    let rcqe = match ring.wait_completion() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if rcqe.res != payload.len() as i32 {
        return false;
    }
    &buf[..payload.len()] == payload
}

/// The read on the empty pipe defers (-EAGAIN, recorded in-flight) while the
/// raced write lands inline, so the deferred read completes in the same
/// blocking harvest (SLOPRING § 7.1).
fn test_deferred_read() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();
    let wr_fd = wr.raw();
    let msg = b"deferred!";

    let result = slopfut::block_on(ring, async move {
        match slopfut::select2(
            slopfut::read(rd_fd, vec![0u8; 16], 16),
            slopfut::write(wr_fd, msg.to_vec()),
        )
        .await
        {
            slopfut::Either2::A(rd) => rd,
            // Writer won: sentinel so the test fails loudly.
            slopfut::Either2::B(_) => slopfut::BufResult {
                res: -1,
                buf: Vec::new(),
            },
        }
    });
    // Keep both pipe ends alive until after the ring teardown.
    drop(wr);
    drop(rd);
    result.res == msg.len() as i32 && &result.buf[..msg.len()] == msg
}

/// Dropping an unresolved op future fires `OP_CANCEL` for it, so the timer
/// winning the race lets `block_on` return rather than hang on the read.
fn test_cancel() -> bool {
    let Ok((rd, _wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();

    let timer_won = slopfut::block_on(ring, async move {
        match slopfut::select2(
            slopfut::read(rd_fd, vec![0u8; 16], 16),
            slopfut::timeout(50_000_000), // 50 ms
        )
        .await
        {
            slopfut::Either2::A(_) => false,
            slopfut::Either2::B(_) => true,
        }
    });
    drop(_wr);
    drop(rd);
    timer_won
}

/// OP_WRITE on an unconnected TCP socket must complete inline with a
/// socket-layer errno, which no pipe/regular fd yields — proving the ring took
/// the `FileKind::Socket` path. In-process TCP loopback never completes its
/// handshake here, so a real data round-trip is not available.
fn test_socket_dispatch() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let payload = b"slopring-sock";
    let mut wsqe = Sqe::ZERO;
    wsqe.opcode = OP_WRITE;
    wsqe.fd = sock.raw();
    wsqe.addr = payload.as_ptr() as u64;
    wsqe.len = payload.len() as u32;
    wsqe.user_data = 0x5e;
    if ring.push_sqe(&wsqe).is_err() || ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0x5e && cqe.res < 0,
        None => false,
    }
}

/// OP_SEND on an unconnected TCP socket completes inline with a socket-layer
/// errno, proving the socket-send routing runs distinctly from OP_WRITE's
/// generic `file_write_fd`.
fn test_send_socket_dispatch() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let payload = b"slopring-send";
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND;
    sqe.fd = sock.raw();
    sqe.addr = payload.as_ptr() as u64;
    sqe.len = payload.len() as u32;
    sqe.user_data = 0x5e4d;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0x5e4d && cqe.res < 0,
        None => false,
    }
}

/// OP_RECVMSG treats `sqe.addr` as a user `MsgHdr*` and validates it, unlike
/// OP_READ which treats it as a raw data buffer.
fn test_recvmsg_efault() -> bool {
    // Valid socket so the op reaches msghdr parsing rather than ENOTSOCK.
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_RECVMSG;
    sqe.fd = sock.raw();
    sqe.addr = 0; // null MsgHdr* → EFAULT
    sqe.user_data = 0xfa01;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    // -EFAULT == -14.
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xfa01 && cqe.res == -14,
        None => false,
    }
}

/// OP_RECVFROM's source-addr out-pointer `addr2` is mandatory, unlike
/// OP_READ/OP_RECVMSG which have none.
fn test_recvfrom_null_addr2_efault() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let mut data = [0u8; 32];
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_RECVFROM;
    sqe.fd = sock.raw();
    sqe.addr = data.as_mut_ptr() as u64;
    sqe.len = data.len() as u32;
    sqe.addr2 = 0; // null source-addr out-ptr → EFAULT
    sqe.user_data = 0xc0fe;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    // -EFAULT == -14.
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xc0fe && cqe.res == -14,
        None => false,
    }
}

/// With no datagram queued the recv probe returns -EAGAIN and the op is
/// recorded in-flight, so a pure poll observes no CQE: deferred completions
/// land only inside a blocking `ring_enter` (SLOPRING § 7.1).
fn test_recvfrom_eagain_deferred() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    // Bind so the UDP socket is a valid receiver (no peer ever sends).
    if net::bind_any(sock.raw(), 0).is_err() {
        return false;
    }
    if net::set_nonblocking(sock.raw()).is_err() {
        return false;
    }
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let mut data = [0u8; 32];
    let mut src = slopos_abi::net::SockAddrIn::default();
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_RECVFROM;
    sqe.fd = sock.raw();
    sqe.addr = data.as_mut_ptr() as u64;
    sqe.len = data.len() as u32;
    sqe.addr2 = &mut src as *mut _ as u64;
    sqe.user_data = 0xbeef;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    ring.poll_completion().is_none()
}

/// fs opens never block, so OP_OPENAT and OP_CLOSE both complete inline —
/// exercising the fd install + reserve-before-side-effect ownership path.
fn test_openat_close() -> bool {
    use slopos_abi::fs::{O_CREAT, O_RDWR};

    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let path = b"/tmp_ring_openat_test\0";
    let mut osqe = Sqe::ZERO;
    osqe.opcode = OP_OPENAT;
    osqe.fd = -1;
    osqe.addr = path.as_ptr() as u64;
    osqe.len = path.len() as u32;
    osqe.op_flags = O_CREAT | O_RDWR;
    osqe.user_data = 0x0bad0e0;
    if ring.push_sqe(&osqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let new_fd = match ring.poll_completion() {
        Some(cqe) if cqe.user_data == 0x0bad0e0 => cqe.res,
        _ => return false,
    };
    if new_fd < 0 {
        return false;
    }

    let mut csqe = Sqe::ZERO;
    csqe.opcode = OP_CLOSE;
    csqe.fd = new_fd;
    csqe.user_data = 0xc105e;
    if ring.push_sqe(&csqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let close_ok = matches!(
        ring.poll_completion(),
        Some(cqe) if cqe.user_data == 0xc105e && cqe.res == 0
    );
    if !close_ok {
        return false;
    }

    let missing = b"/no_such_ring_openat_file\0";
    let mut msqe = Sqe::ZERO;
    msqe.opcode = OP_OPENAT;
    msqe.fd = -1;
    msqe.addr = missing.as_ptr() as u64;
    msqe.len = missing.len() as u32;
    msqe.op_flags = O_RDWR;
    msqe.user_data = 0x4044;
    if ring.push_sqe(&msqe).is_err() || ring.submit().is_err() {
        return false;
    }
    matches!(
        ring.poll_completion(),
        Some(cqe) if cqe.user_data == 0x4044 && cqe.res < 0
    )
}

/// The `slopfut` openat/close wrappers and their ownership-passing path buffer.
fn test_slopfut_openat_close() -> bool {
    use slopos_abi::fs::{O_CREAT, O_RDWR};

    let Ok(ring) = Ring::setup(8) else {
        return false;
    };

    slopfut::block_on(ring, async {
        let fd = slopfut::openat(b"/tmp_ring_slopfut_test", O_CREAT | O_RDWR).await;
        if fd < 0 {
            return false;
        }
        let close_res = slopfut::close(fd).await;
        close_res == 0
    })
}

/// `select3` over two reads + a timer with ownership-passing buffers — nc's
/// loop shape: the winner's buffer must come back carrying the data.
fn test_select3_pingpong() -> bool {
    let Ok((rd_a, wr_a)) = fs::pipe() else {
        return false;
    };
    let Ok((rd_b, _wr_b)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let (a, b, wa) = (rd_a.raw(), rd_b.raw(), wr_a.raw());
    let msg = b"abc";

    let ok = slopfut::block_on(ring, async move {
        let w = slopfut::write(wa, msg.to_vec()).await;
        if w.res != msg.len() as i32 {
            return false;
        }
        match slopfut::select3(
            slopfut::read(a, vec![0u8; 8], 8),
            slopfut::read(b, vec![0u8; 8], 8),
            slopfut::timeout(2_000_000_000),
        )
        .await
        {
            slopfut::Either3::A(br) => br.res == msg.len() as i32 && &br.buf[..msg.len()] == msg,
            _ => false,
        }
    });
    drop((rd_a, wr_a, rd_b, _wr_b));
    ok
}

/// `RING_REGISTER_BUFFERS` / `RING_UNREGISTER_BUFFERS`: a double-register is
/// -EEXIST and a zero-count register is -EINVAL.
fn test_register_fixed_buffers() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    // Touch the anonymous buffer so its page is faulted in before the kernel
    // pins it.
    let mut buf = [0u8; 256];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8;
    }
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    if ring.register_buffers(&iovecs) >= 0 {
        return false;
    }
    if ring.unregister_buffers() != 0 {
        return false;
    }
    if ring.register_buffers(&[]) >= 0 {
        return false;
    }
    true
}

/// No user `addr` is supplied, so reaching the socket-layer errno at all proves
/// `buf_index` resolved to the registered fixed buffer.
fn test_fixed_send_dispatch() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let payload = b"slopring-fixed-send";
    let mut buf = [0u8; 64];
    buf[..payload.len()].copy_from_slice(payload);
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 0;
    sqe.len = payload.len() as u32;
    sqe.user_data = 0xF1;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xF1 && cqe.res < 0,
        None => false,
    }
}

/// Zero-copy send must name its pinned data via a registered fixed buffer, so
/// without the flag it is rejected inline with a single error CQE.
fn test_send_zc_requires_fixed_buffer() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND_ZC;
    sqe.fd = sock.raw();
    sqe.flags = 0; // no SLOPRING_SQE_FIXED_BUFFER → -EINVAL
    sqe.len = 0;
    sqe.user_data = 0x5C0;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let Some(cqe) = ring.poll_completion() else {
        return false;
    };
    if cqe.user_data != 0x5C0
        || cqe.res >= 0
        || (cqe.flags & SLOPRING_CQE_F_MORE) != 0
        || (cqe.flags & SLOPRING_CQE_F_NOTIF) != 0
    {
        return false;
    }
    ring.poll_completion().is_none()
}

/// The `OP_SEND_ZC` two-CQE protocol: a result CQE carrying
/// `SLOPRING_CQE_F_MORE` ("notification to follow"), then a terminal CQE
/// carrying `SLOPRING_CQE_F_NOTIF` (registered buffer reusable). The datagram
/// only needs to be queued to the NIC; delivery is irrelevant.
fn test_udp_send_zc_two_cqe() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    // UDP connect only records the default dest, so the send resolves one
    // without a handshake.
    let dst = SockAddrIn {
        family: AF_INET,
        port: 9999u16.to_be(),
        addr: [10, 0, 2, 2],
        _pad: [0; 8],
    };
    if net::connect(sock.raw(), &dst).is_err() {
        return false;
    }
    let payload = b"slopring-zc";
    let mut buf = [0u8; 64];
    buf[..payload.len()].copy_from_slice(payload);
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND_ZC;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 0;
    sqe.len = payload.len() as u32;
    sqe.user_data = 0x5C1;
    // Block for both CQEs: on the deferred NIC-DMA path the F_NOTIF arrives only
    // after the device reclaims the TX descriptor (a harvest re-poll), so a
    // bare `submit()` + `poll_completion()` would miss it. `submit_and_wait(2)`
    // is correct for both the deferred and the copy-fallback paths.
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(2).is_err() {
        return false;
    }
    // First CQE: the send result, carrying F_MORE and not yet F_NOTIF.
    let Some(cqe1) = ring.poll_completion() else {
        return false;
    };
    if cqe1.user_data != 0x5C1 || cqe1.res < 0 {
        return false; // a failed send would be a single error CQE — regression
    }
    if (cqe1.flags & SLOPRING_CQE_F_MORE) == 0 || (cqe1.flags & SLOPRING_CQE_F_NOTIF) != 0 {
        return false;
    }
    // Terminal CQE: F_NOTIF set, F_MORE clear (buffer reusable).
    let Some(cqe2) = ring.poll_completion() else {
        return false;
    };
    cqe2.user_data == 0x5C1
        && (cqe2.flags & SLOPRING_CQE_F_NOTIF) != 0
        && (cqe2.flags & SLOPRING_CQE_F_MORE) == 0
}

/// `OP_SEND_ZC` over a **warmed** connection takes the true NIC-DMA zero-copy
/// path: the payload is DMA'd straight from the pinned buffer and the
/// terminal `SLOPRING_CQE_F_NOTIF` is **deferred** until the device reclaims the
/// TX descriptor (QEMU SLIRP returns it), observed by a harvest re-poll. Warms
/// the gateway neighbor first so the send resolves a MAC (else it falls back to
/// the copy path, which still yields the two-CQE protocol). Either way the
/// blocking `submit_and_wait(2)` collects both CQEs.
fn test_udp_send_zc_two_cqe_deferred() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    let dst = SockAddrIn {
        family: AF_INET,
        port: 9999u16.to_be(),
        addr: [10, 0, 2, 2],
        _pad: [0; 8],
    };
    if net::connect(sock.raw(), &dst).is_err() {
        return false;
    }
    // Warm the neighbor cache: a normal datagram drives the ARP request; the
    // reply lands via NAPI so the zero-copy send can resolve the gateway MAC.
    let _ = net::send(sock.raw(), b"warm", 0);

    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let payload = b"slopring-zc-deferred";
    let mut buf = [0u8; 64];
    buf[..payload.len()].copy_from_slice(payload);
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND_ZC;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 0;
    sqe.len = payload.len() as u32;
    sqe.user_data = 0x5C2;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(2).is_err() {
        return false;
    }
    let Some(cqe1) = ring.poll_completion() else {
        return false;
    };
    if cqe1.user_data != 0x5C2 || cqe1.res < 0 {
        return false;
    }
    if (cqe1.flags & SLOPRING_CQE_F_MORE) == 0 || (cqe1.flags & SLOPRING_CQE_F_NOTIF) != 0 {
        return false;
    }
    let Some(cqe2) = ring.poll_completion() else {
        return false;
    };
    cqe2.user_data == 0x5C2
        && (cqe2.flags & SLOPRING_CQE_F_NOTIF) != 0
        && (cqe2.flags & SLOPRING_CQE_F_MORE) == 0
}

/// `OP_SEND_ZC` on a connected ICMP socket exercises the ICMP zero-copy leaf:
/// the echo payload is DMA'd from the pinned buffer while the kernel
/// computes only the ICMP checksum (no hardware offload for ICMP). The two-CQE
/// protocol (result `F_MORE` then deferred `F_NOTIF`) must hold.
fn test_icmp_send_zc_two_cqe() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP) else {
        return false;
    };
    let dst = SockAddrIn {
        family: AF_INET,
        port: 0,
        addr: [10, 0, 2, 2],
        _pad: [0; 8],
    };
    if net::connect(sock.raw(), &dst).is_err() {
        return false;
    }
    let _ = net::send(sock.raw(), b"warm-icmp", 0); // warm the gateway neighbor

    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let payload = b"slopring-icmp-zc";
    let mut buf = [0u8; 64];
    buf[..payload.len()].copy_from_slice(payload);
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND_ZC;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 0;
    sqe.len = payload.len() as u32;
    sqe.user_data = 0x1C3;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(2).is_err() {
        return false;
    }
    let Some(cqe1) = ring.poll_completion() else {
        return false;
    };
    if cqe1.user_data != 0x1C3 || cqe1.res < 0 {
        return false;
    }
    if (cqe1.flags & SLOPRING_CQE_F_MORE) == 0 || (cqe1.flags & SLOPRING_CQE_F_NOTIF) != 0 {
        return false;
    }
    let Some(cqe2) = ring.poll_completion() else {
        return false;
    };
    cqe2.user_data == 0x1C3
        && (cqe2.flags & SLOPRING_CQE_F_NOTIF) != 0
        && (cqe2.flags & SLOPRING_CQE_F_MORE) == 0
}

/// A fixed-buffer op naming an out-of-range `buf_index` is rejected inline with
/// -EINVAL (the kernel's `check_out_fixed` bounds check — the property the
/// Verus `ring_bufpool` obligation pins) before any socket side effect.
fn test_fixed_buffer_oob() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let mut buf = [0u8; 64];
    buf[0] = 1;
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 5; // only one buffer registered
    sqe.len = 8;
    sqe.user_data = 0x00B;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0x00B && cqe.res == -22, // -EINVAL
        None => false,
    }
}

/// Registering a provided buffer ring succeeds; a recv with `BUFFER_SELECT`
/// over an *empty* ring completes inline with -ENOBUFS (the kernel peeked the
/// group, found no published buffer), and the ring then unregisters. Proves the
/// `RING_REGISTER_PBUF_RING` plumbing + the provided-ring selection path.
fn test_pbuf_ring_select_enobufs() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    // A 4-slot provided ring (4 × 16 bytes), empty (producer tail == 0).
    let ringbuf = [0u8; 4 * 16];
    let cmd = RegisterBufRingCmd {
        ring_addr: ringbuf.as_ptr() as u64,
        ring_entries: 4,
        buf_group: 1,
        flags: 0,
    };
    if ring.register_buf_ring(&cmd) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_RECVMSG;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_BUFFER_SELECT;
    sqe.buf_group = 1;
    sqe.user_data = 0xB0;
    if ring.push_sqe(&sqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let recv_ok = match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xB0 && cqe.res == -105, // -ENOBUFS (empty ring)
        None => false,
    };
    recv_ok && ring.unregister_buf_ring(1) == 0
}

/// `OP_CONNECT` validates the user `SockAddrIn` pointer: a null `addr`
/// completes inline with `-EFAULT` (before any handshake), a single CQE with no
/// `F_MORE`/`F_NOTIF`. Proves the opcode is wired and validates its input.
fn test_connect_efault() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_CONNECT;
    sqe.fd = sock.raw();
    sqe.addr = 0; // null SockAddrIn* → EFAULT
    sqe.len = core::mem::size_of::<SockAddrIn>() as u32;
    sqe.user_data = 0xC0FE;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(1).is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => {
            cqe.user_data == 0xC0FE
                && cqe.res == -14
                && (cqe.flags & SLOPRING_CQE_F_MORE) == 0
                && (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        }
        None => false,
    }
}

/// `OP_CONNECT` on a UDP socket completes **inline** with `res == 0`: a UDP
/// "connect" just records the default peer + `Connected` state (no handshake),
/// so the async opcode resolves immediately with a single, notification-free
/// CQE — exercising OP_CONNECT's success path deterministically (no network).
fn test_connect_udp_inline_success() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_DGRAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let dst = SockAddrIn {
        family: AF_INET,
        port: 9999u16.to_be(),
        addr: [10, 0, 2, 2],
        _pad: [0; 8],
    };
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_CONNECT;
    sqe.fd = sock.raw();
    sqe.addr = &dst as *const SockAddrIn as u64;
    sqe.len = core::mem::size_of::<SockAddrIn>() as u32;
    sqe.user_data = 0xC0DC;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(1).is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => {
            cqe.user_data == 0xC0DC
                && cqe.res == 0
                && (cqe.flags & SLOPRING_CQE_F_MORE) == 0
                && (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        }
        None => false,
    }
}

/// `OP_SEND_ZC` with a fixed buffer on an **unconnected** TCP socket yields a
/// single error CQE (the send fails before the buffer is used: not connected),
/// with no `F_MORE`/`F_NOTIF`. The TCP `MSG_ZEROCOPY` two-CQE / pin-lifetime
/// correctness over a real handshake is covered by the kernel stests + the
/// `tcp_zc_pin` proof — SLIRP has no deterministic outbound TCP peer, so the
/// userland test asserts only the deterministic shape.
fn test_tcp_send_zc_shape() -> bool {
    let Ok(sock) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let buf = [0u8; 64];
    let iovecs = [BufIovec {
        addr: buf.as_ptr() as u64,
        len: buf.len() as u32,
        _pad: 0,
    }];
    if ring.register_buffers(&iovecs) != 0 {
        return false;
    }
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_SEND_ZC;
    sqe.fd = sock.raw();
    sqe.flags = SLOPRING_SQE_FIXED_BUFFER;
    sqe.buf_index = 0;
    sqe.len = 16;
    sqe.user_data = 0x7C90;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(1).is_err() {
        return false;
    }
    match ring.poll_completion() {
        // Unconnected TCP: the send fails before the buffer is used → one error
        // CQE, never a two-CQE notification. (A blocked second CQE would hang;
        // this asserts the single-CQE error shape.)
        Some(cqe) => {
            cqe.user_data == 0x7C90 && cqe.res < 0 && (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        }
        None => false,
    }
}

/// `OP_CONNECT` on an **AF_UNIX** socket, end-to-end: bind + listen a unix
/// listener, then async-connect a client to its path through the ring. Unix
/// connect allocates the pair immediately when the backlog has room, so the op
/// completes inline with `res == 0` and a single notification-free CQE — a fully
/// deterministic (in-process, no network) exercise of the AF_UNIX OP_CONNECT path.
fn test_connect_unix_inline_success() -> bool {
    let path = b"/ring-op-connect-unix";
    let Ok(listener) = net::socket(AF_UNIX, SOCK_STREAM, 0) else {
        return false;
    };
    if net::bind_unix(listener.raw(), path).is_err() {
        return false;
    }
    if net::listen(listener.raw(), 4).is_err() {
        return false;
    }
    let Ok(client) = net::socket(AF_UNIX, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let mut sa = SockAddrUn::default();
    sa.family = AF_UNIX;
    sa.path[..path.len()].copy_from_slice(path);
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_CONNECT;
    sqe.fd = client.raw();
    sqe.addr = &sa as *const SockAddrUn as u64;
    sqe.len = (2 + path.len()) as u32;
    sqe.user_data = 0xC0DB;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(1).is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => {
            cqe.user_data == 0xC0DB
                && cqe.res == 0
                && (cqe.flags & SLOPRING_CQE_F_MORE) == 0
                && (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        }
        None => false,
    }
}

/// `OP_CONNECT` on an AF_UNIX socket to a path with **no listener** completes
/// inline with `-ECONNREFUSED`, a single notification-free CQE — proving the
/// unix connect routing + errno mapping through the ring.
fn test_connect_unix_refused() -> bool {
    let Ok(sock) = net::socket(AF_UNIX, SOCK_STREAM, 0) else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };
    let path = b"/ring-op-connect-no-listener";
    let mut sa = SockAddrUn::default();
    sa.family = AF_UNIX;
    sa.path[..path.len()].copy_from_slice(path);
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_CONNECT;
    sqe.fd = sock.raw();
    sqe.addr = &sa as *const SockAddrUn as u64;
    sqe.len = (2 + path.len()) as u32;
    sqe.user_data = 0xC0DD;
    if ring.push_sqe(&sqe).is_err() || ring.submit_and_wait(1).is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => {
            cqe.user_data == 0xC0DD
                && cqe.res == -111
                && (cqe.flags & SLOPRING_CQE_F_MORE) == 0
                && (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        }
        None => false,
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("setup", test_setup),
    ("nop", test_nop),
    ("write_read_pipe", test_write_read_pipe),
    ("deferred_read", test_deferred_read),
    ("cancel", test_cancel),
    ("select3_pingpong", test_select3_pingpong),
    ("socket_dispatch", test_socket_dispatch),
    ("send_socket_dispatch", test_send_socket_dispatch),
    ("recvmsg_efault", test_recvmsg_efault),
    (
        "recvfrom_null_addr2_efault",
        test_recvfrom_null_addr2_efault,
    ),
    ("recvfrom_eagain_deferred", test_recvfrom_eagain_deferred),
    ("openat_close", test_openat_close),
    ("slopfut_openat_close", test_slopfut_openat_close),
    ("register_fixed_buffers", test_register_fixed_buffers),
    ("fixed_send_dispatch", test_fixed_send_dispatch),
    (
        "send_zc_requires_fixed_buffer",
        test_send_zc_requires_fixed_buffer,
    ),
    ("udp_send_zc_two_cqe", test_udp_send_zc_two_cqe),
    (
        "udp_send_zc_two_cqe_deferred",
        test_udp_send_zc_two_cqe_deferred,
    ),
    ("icmp_send_zc_two_cqe", test_icmp_send_zc_two_cqe),
    ("fixed_buffer_oob", test_fixed_buffer_oob),
    ("pbuf_ring_select_enobufs", test_pbuf_ring_select_enobufs),
    ("connect_efault", test_connect_efault),
    (
        "connect_udp_inline_success",
        test_connect_udp_inline_success,
    ),
    ("tcp_send_zc_shape", test_tcp_send_zc_shape),
    (
        "connect_unix_inline_success",
        test_connect_unix_inline_success,
    ),
    ("connect_unix_refused", test_connect_unix_refused),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
