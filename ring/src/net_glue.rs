//! Socket-opcode glue (SLOPRING § 12, cross-cutting reality 2).
//!
//! Socket-typed opcodes route through the socket send/recv/accept paths, not
//! the generic `file_read_fd`/`file_write_fd`, per family via
//! `FileOps::is_unix_socket()` and with the socket's stored nonblocking flag
//! forced across the probe. AF_INET entry points take raw user pointers;
//! AF_UNIX ones take kernel slices, so AF_UNIX marshals user↔kernel through a
//! validated `UserBytes` staging copy.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::net::{AF_INET, AF_UNIX, SockAddrIn};
use slopos_abi::ring::{SLOPRING_CQE_BUFFER_SHIFT, SLOPRING_CQE_F_BUFFER};
use slopos_abi::syscall::{CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS, SOL_SOCKET};
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};
use slopos_fs::fileio::FdTable;

use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

use slopos_fs::fileio::FileRef;
use slopos_net::socket::ZcSendOutcome;
use slopos_net::types::{Ipv4Addr, Port, SockAddr};
use slopos_net::unix_socket::SocketHandle;
use slopos_net::{socket, unix_socket, unix_socket_file_ops};

use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use crate::buffers::BufferRegistry;
use crate::opcode::Outcome;

/// Staging-buffer cap for AF_UNIX user↔kernel marshalling, matching the
/// 4 KiB bound the `send`/`recv`/`recvmsg` syscall handlers stage through.
const STAGING_CAP: usize = 4096;

/// The would-block sentinel (`Errno::EAGAIN.raw()` is already negative).
const EAGAIN: i32 = Errno::EAGAIN.raw();

/// Run `f` with the AF_UNIX socket forced nonblocking, restoring its original
/// stored flag afterward (SLOPRING § 12 reality 1: there is no per-call
/// nonblock argument). A concurrent close between the set and the restore is
/// bounded by the socket-handle validation each primitive does internally.
fn with_unix_forced_nonblock<T>(h: SocketHandle, f: impl FnOnce() -> T) -> T {
    let was = unix_socket::unix_is_nonblocking(h).unwrap_or(false);
    let _ = unix_socket::unix_set_nonblocking(h, true);
    let out = f();
    let _ = unix_socket::unix_set_nonblocking(h, was);
    out
}

/// Run `f` with the AF_INET socket forced nonblocking, restoring its original
/// stored flag afterward.
fn with_inet_forced_nonblock<T>(idx: u32, f: impl FnOnce() -> T) -> T {
    let was = socket::socket_is_nonblocking(idx).unwrap_or(false);
    let _ = socket::socket_set_nonblocking(idx, true);
    let out = f();
    let _ = socket::socket_set_nonblocking(idx, was);
    out
}

/// Non-blocking accept for process `table`. `Ok(None)` means would block;
/// `Err(errno)` is a real error.
///
/// Reserve-before-side-effect (SLOPRING § 11) is the *caller's*
/// responsibility — by the time we install the fd here the CQE slot is
/// already reserved, so the accepted fd can always be reported.
pub fn accept_nonblock(table: FdTable, file: &FileRef) -> Result<Option<i32>, Errno> {
    let (handle, ops) = socket_handle_from_ref(file)?;

    if ops.is_unix_socket() {
        let listener = SocketHandle::from_usize(handle);
        let result = with_unix_forced_nonblock(listener, || unix_socket::unix_accept(listener));
        match result {
            Ok(accepted) => {
                // The backing owns the accepted endpoint: a failed install
                // closes it. Charged to the *accepting* process — a connection
                // is remote-triggered, so billing the listener's principal
                // would let a peer exhaust that principal's whole budget.
                let Some(backing) = slopos_net::unix_socket_file_ops::unix_socket_backing(
                    accepted,
                    table.account(),
                ) else {
                    return Err(Errno::ENFILE);
                };
                let new_fd = slopos_fs::fileio_open_fd_with_ops(
                    table,
                    &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                    accepted.as_usize(),
                    Some(backing),
                    slopos_fs::FdFlags::NONE,
                );
                if new_fd < 0 {
                    return Err(Errno::ENOMEM);
                }
                Ok(Some(new_fd))
            }
            Err(rc) => {
                let e = Errno::from_raw(rc).unwrap_or(Errno::EINVAL);
                if e == Errno::EAGAIN { Ok(None) } else { Err(e) }
            }
        }
    } else {
        let sock_idx = handle as u32;
        let accepted = with_inet_forced_nonblock(sock_idx, || {
            socket::socket_accept(sock_idx, core::ptr::null_mut(), core::ptr::null_mut())
        });
        if accepted < 0 {
            let e = Errno::from_raw(accepted).unwrap_or(Errno::EINVAL);
            return if e == Errno::EAGAIN { Ok(None) } else { Err(e) };
        }
        let Some(backing) =
            slopos_net::socket_file_ops::socket_backing(accepted as u32, table.account())
        else {
            return Err(Errno::ENFILE);
        };
        let new_fd = slopos_fs::fileio_open_socket_fd(table, accepted as u32, Some(backing));
        if new_fd < 0 {
            return Err(Errno::ENOMEM);
        }
        Ok(Some(new_fd))
    }
}

/// Non-blocking connect to the socket address at user VA `addr_va` — AF_INET
/// (`SockAddrIn`) and AF_UNIX (`SockAddrUn`). Returns the socket-op result
/// code; `Ok(-EAGAIN)` is in progress and the ring defers. Idempotent across
/// re-probes: [`socket::socket_connect_nonblock`] emits the SYN once and then
/// polls. The user address is validated and snapshotted **before** any side
/// effect, so a faulting address fails cleanly and a re-probe re-reads the same
/// (caller-stable) pointer.
pub fn connect_nonblock(file: &FileRef, addr_va: u64, addr_len: u32) -> Result<i32, Errno> {
    let (handle, ops) = socket_handle_from_ref(file)?;
    if ops.is_unix_socket() {
        // The AF_UNIX `-EAGAIN` means the listener backlog is momentarily full,
        // returned before any side effect, so it is legitimately deferrable.
        if (addr_len as usize) < 4 {
            return Err(Errno::EINVAL);
        }
        let ptr = UserPtr::<SockAddrUn>::try_new(addr_va).map_err(|_| Errno::EFAULT)?;
        let sa: SockAddrUn = copy_from_user(ptr).map_err(|_| Errno::EFAULT)?;
        if sa.family != AF_UNIX {
            return Err(Errno::EAFNOSUPPORT);
        }
        let path_len = (addr_len as usize - 2).min(UNIX_PATH_MAX);
        let actual_len = sa.path[..path_len]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(path_len);
        if actual_len == 0 {
            return Err(Errno::EINVAL);
        }
        let sh = SocketHandle::from_usize(handle);
        let rc =
            with_unix_forced_nonblock(sh, || unix_socket::unix_connect(sh, &sa.path[..actual_len]));
        return Ok(rc);
    }
    if (addr_len as usize) < core::mem::size_of::<SockAddrIn>() {
        return Err(Errno::EINVAL);
    }
    let ptr = UserPtr::<SockAddrIn>::try_new(addr_va).map_err(|_| Errno::EFAULT)?;
    let sa: SockAddrIn = copy_from_user(ptr).map_err(|_| Errno::EFAULT)?;
    if sa.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let port = u16::from_be(sa.port);
    let idx = handle as u32;
    // `socket_connect_nonblock` keys off the socket's own state, so the wrapper
    // is a no-op here; kept so a future blocking-aware path stays correct.
    Ok(with_inet_forced_nonblock(idx, || {
        socket::socket_connect_nonblock(idx, sa.addr, port)
    }))
}

/// Resolve a held [`FileRef`] to its socket handle + ops (`ENOTSOCK` for a
/// non-socket). The reference pins one socket identity for the op's duration —
/// no fd number is re-interpreted.
fn socket_handle_from_ref(file: &FileRef) -> Result<(usize, &'static dyn FileOps), Errno> {
    let (handle, ops) = slopos_fs::fileio::fileio_handle_and_ops_from_ref(file);
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    Ok((handle, ops))
}

/// Map a non-blocking socket-op return code into an [`Outcome`]: the
/// would-block sentinel (`-EAGAIN`) defers, everything else completes inline.
pub(crate) fn outcome_from_rc(rc: i32) -> Outcome {
    if rc == EAGAIN {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc)
    }
}

/// `OP_SEND`: socket-only send (SLOPRING § 12). Both families stage the user
/// bytes into a kernel scratch and pass the *scratch* to the send primitive,
/// never a raw user VA: `socket_send` reads without fault recovery, so a
/// concurrent munmap between validation and the read would fault the kernel.
/// Neither primitive takes per-call send flags, so `sqe.op_flags` is accepted
/// and ignored, matching the `send(2)` syscall path.
pub fn send_nonblock(file: &FileRef, addr: u64, len: u32, _op_flags: u32) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    let len = (len as usize).min(STAGING_CAP);
    if addr == 0 && len != 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }

    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(STAGING_CAP) {
        Ok(v) => v,
        Err(_) => return Outcome::Inline(Errno::ENOMEM.raw()),
    };
    let copied = if len > 0 {
        let user = match UserBytes::try_new(addr, len) {
            Ok(u) => u,
            Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
        };
        match copy_bytes_from_user(user, &mut scratch[..len]) {
            Ok(n) => n,
            Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
        }
    } else {
        0
    };

    if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        let rc = with_unix_forced_nonblock(sh, || unix_socket::unix_send(sh, &scratch[..copied]));
        outcome_from_rc(rc)
    } else {
        let idx = handle as u32;
        let rc = with_inet_forced_nonblock(idx, || socket::socket_send(idx, &scratch[..copied]));
        // socket_send returns i64; collapse to the CQE's i32 res.
        if rc == EAGAIN as i64 {
            Outcome::WouldBlock
        } else {
            Outcome::Inline(rc as i32)
        }
    }
}

/// `OP_RECVMSG`: socket-only recvmsg (SLOPRING § 12). Parses the user `MsgHdr`
/// at `sqe.addr`, recvs into a staging buffer, copies the data out to
/// `iov_base`/`iov_len`, and — for AF_UNIX with SCM_RIGHTS fds — installs the
/// received fds into the caller's fd table and writes the `CmsgHdr` back into
/// `control`/`control_len`. AF_INET fills data only (no SCM_RIGHTS), which is
/// correct, not an error. An ownership op (it installs fds), so the caller
/// reserves a CQE slot before dispatch (SLOPRING § 11).
pub fn recvmsg_nonblock(table: FdTable, file: &FileRef, addr: u64, _op_flags: u32) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    let msg_ptr = match UserPtr::<MsgHdr>::try_new(addr) {
        Ok(p) => p,
        Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    let msg: MsgHdr = match copy_from_user(msg_ptr) {
        Ok(m) => m,
        Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
    };

    let data_len = (msg.iov_len as usize).min(STAGING_CAP);
    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(STAGING_CAP) {
        Ok(v) => v,
        Err(_) => return Outcome::Inline(Errno::ENOMEM.raw()),
    };

    if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        let mut received: slopos_ostd::KVec<slopos_fs::FileRef> =
            match slopos_ostd::KVec::with_capacity(SCM_MAX_FDS) {
                Ok(v) => v,
                Err(_) => return Outcome::Inline(Errno::ENOMEM.raw()),
            };
        let (bytes_read, n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg(sh, &mut scratch[..data_len], &mut received, SCM_MAX_FDS)
        });
        if bytes_read == EAGAIN && n_fds == 0 {
            // The fds drain atomically with the data on the real completion.
            return Outcome::WouldBlock;
        }
        if bytes_read < 0 && !(bytes_read == EAGAIN && n_fds > 0) {
            // `received` drops here, closing any drained aliases.
            return Outcome::Inline(bytes_read);
        }
        let copied = if bytes_read > 0 {
            bytes_read as usize
        } else {
            0
        };
        if copied > 0 && msg.iov_base != 0 {
            let user_out = match UserBytes::try_new(msg.iov_base, copied) {
                Ok(u) => u,
                Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
            };
            if copy_bytes_to_user(user_out, &scratch[..copied]).is_err() {
                return Outcome::Inline(Errno::EFAULT.raw());
            }
        }
        if n_fds > 0 {
            if let Err(e) = recvmsg_writeback_cmsg(table, &msg, received, msg_ptr) {
                return Outcome::Inline(e.raw());
            }
        }
        Outcome::Inline(copied as i32)
    } else {
        let idx = handle as u32;
        let rc =
            with_inet_forced_nonblock(idx, || socket::socket_recv(idx, &mut scratch[..data_len]));
        if rc == EAGAIN as i64 {
            return Outcome::WouldBlock;
        }
        if rc < 0 {
            return Outcome::Inline(rc as i32);
        }
        let copied = rc as usize;
        if copied > 0 && msg.iov_base != 0 {
            // The bytes are already consumed; if the copy to iov_base faults
            // they are lost (TCP has no un-consume), as in the recv syscall.
            let user_out = match UserBytes::try_new(msg.iov_base, copied) {
                Ok(u) => u,
                Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
            };
            if copy_bytes_to_user(user_out, &scratch[..copied]).is_err() {
                return Outcome::Inline(Errno::EFAULT.raw());
            }
        }
        Outcome::Inline(copied as i32)
    }
}

/// `OP_RECVFROM`: AF_INET datagram recv that also returns the source address
/// (SLOPRING § 12). Recvs into a kernel scratch, copies the data out to the
/// user buffer at `addr`, and writes the source `SockAddrIn` to the validated
/// user out-pointer at `addr2`. `-EAGAIN` defers; every other result completes
/// inline. A *consuming* op — the caller reserves a CQE slot before dispatch
/// (SLOPRING § 11), so a successful recv never drops its CQE and loses the
/// datagram. Null `addr` (with non-zero `len`) or null `addr2` is `-EFAULT`
/// before any recv: the source addr is mandatory, like `recvfrom(2)`'s
/// `src_addr` when requested.
pub fn recvfrom_nonblock(file: &FileRef, addr: u64, len: u32, addr2: u64) -> Outcome {
    if addr2 == 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // An AF_UNIX fd has no datagram source address.
    if ops.is_unix_socket() {
        return Outcome::Inline(Errno::ENOTSOCK.raw());
    }

    let len = (len as usize).min(STAGING_CAP);
    if addr == 0 && len != 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }

    let mut scratch = match slopos_ostd::KVec::<u8>::zeroed(STAGING_CAP) {
        Ok(v) => v,
        Err(_) => return Outcome::Inline(Errno::ENOMEM.raw()),
    };

    let idx = handle as u32;
    let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
    let rc = with_inet_forced_nonblock(idx, || {
        socket::socket_recvfrom(idx, &mut scratch[..len], Some(&mut peer))
    });
    if rc == EAGAIN as i64 {
        return Outcome::WouldBlock;
    }
    if rc < 0 {
        return Outcome::Inline(rc as i32);
    }

    // The datagram is consumed by this point; if a copy to the user buffer or
    // out-addr faults the bytes are lost (UDP has no un-consume).
    let copied = rc as usize;
    if copied > 0 && addr != 0 {
        let user_out = match UserBytes::try_new(addr, copied) {
            Ok(u) => u,
            Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
        };
        if copy_bytes_to_user(user_out, &scratch[..copied]).is_err() {
            return Outcome::Inline(Errno::EFAULT.raw());
        }
    }

    let src = SockAddrIn {
        family: AF_INET,
        port: peer.port.0.to_be(),
        addr: peer.ip.0,
        _pad: [0; 8],
    };
    let src_ptr = match UserPtr::<SockAddrIn>::try_new(addr2) {
        Ok(p) => p,
        Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    if copy_to_user(src_ptr, &src).is_err() {
        return Outcome::Inline(Errno::EFAULT.raw());
    }

    Outcome::Inline(copied as i32)
}

/// Install the received SCM_RIGHTS files into the caller's fd table and write
/// the `CmsgHdr` + fd array back into the user `control` buffer, updating
/// `control_len`. Consumes `received`: undeliverable aliases drop (close) here,
/// and a copy back to user memory failing *after* fds were installed closes
/// every installed fd — the caller never learns the numbers, so they would
/// otherwise be orphaned into an fd-table-exhaustion DoS.
fn recvmsg_writeback_cmsg(
    table: FdTable,
    msg: &MsgHdr,
    mut received: slopos_ostd::KVec<slopos_fs::FileRef>,
    msg_ptr: UserPtr<MsgHdr>,
) -> Result<(), Errno> {
    let n_fds = received.len();

    if msg.control == 0 {
        // No control buffer to report them in — the aliases drop.
        return Ok(());
    }

    let hdr_size = core::mem::size_of::<CmsgHdr>();
    let needed = hdr_size + n_fds * 4;
    if (msg.control_len as usize) < needed {
        drop(received);
        let updated = MsgHdr {
            iov_base: msg.iov_base,
            iov_len: msg.iov_len,
            control: msg.control,
            control_len: 0,
        };
        copy_to_user(msg_ptr, &updated).map_err(|_| Errno::EFAULT)?;
        return Ok(());
    }

    debug_assert!(n_fds <= SCM_MAX_FDS);
    let mut fd_nums = [0i32; SCM_MAX_FDS];
    for (j, file) in received.drain(..).enumerate() {
        let new_fd = slopos_fs::fileio::fileio_install_file_ref(table, file);
        if new_fd < 0 {
            // The failed install dropped its alias and the drain drops the
            // rest; roll back the installed fds so none are orphaned.
            for &fd in fd_nums.iter().take(j) {
                let _ = slopos_fs::fileio::file_close_fd(table, fd);
            }
            return Err(Errno::ENOMEM);
        }
        fd_nums[j] = new_fd;
    }

    let writeback = || -> Result<(), Errno> {
        let cmsg = CmsgHdr {
            cmsg_len: needed as u32,
            cmsg_level: SOL_SOCKET as u32,
            cmsg_type: SCM_RIGHTS,
        };
        let cmsg_ptr = UserPtr::<CmsgHdr>::try_new(msg.control).map_err(|_| Errno::EFAULT)?;
        copy_to_user(cmsg_ptr, &cmsg).map_err(|_| Errno::EFAULT)?;

        let fd_bytes = &slopos_ostd::util::byte_view::pod_slice_as_bytes(&fd_nums[..])[..n_fds * 4];
        let fd_out = UserBytes::try_new(msg.control + hdr_size as u64, n_fds * 4)
            .map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(fd_out, fd_bytes).map_err(|_| Errno::EFAULT)?;

        let updated = MsgHdr {
            iov_base: msg.iov_base,
            iov_len: msg.iov_len,
            control: msg.control,
            control_len: needed as u64,
        };
        copy_to_user(msg_ptr, &updated).map_err(|_| Errno::EFAULT)?;
        Ok(())
    };

    if let Err(e) = writeback() {
        for &fd in fd_nums.iter().take(n_fds) {
            let _ = slopos_fs::fileio::file_close_fd(table, fd);
        }
        return Err(e);
    }
    Ok(())
}

// The registered / provided buffer fast paths below never re-validate a user
// pointer: the buffer was pinned and validated once at registration.

/// `OP_SEND` from registered fixed buffer `index` (`SLOPRING_SQE_FIXED_BUFFER`).
pub fn send_fixed(
    file: &FileRef,
    index: u16,
    len: u32,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // The net leaf pulls straight from the pinned pages — no kernel scratch hop.
    let mut reader = match reg.fixed_reader(index, len as usize) {
        Ok(r) => r,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        let rc = with_unix_forced_nonblock(sh, || unix_socket::unix_send_from(sh, &mut reader));
        outcome_from_rc(rc)
    } else {
        let idx = handle as u32;
        let rc = with_inet_forced_nonblock(idx, || socket::socket_send_pinned(idx, &mut reader));
        if rc == EAGAIN as i64 {
            Outcome::WouldBlock
        } else {
            Outcome::Inline(rc as i32)
        }
    }
}

/// `OP_SEND_ZC` from registered fixed buffer `index`: the zero-copy send.
///
/// AF_INET first attempts the true NIC-DMA path: the NIC DMAs straight from the
/// pinned pages, and the buffer is **not** reusable until the device reclaims
/// the descriptor, so it returns [`Outcome::DeferredNotif`] — the `F_MORE`
/// result posts now and the terminal `F_NOTIF` waits for the harvest to observe
/// the reclaim. A non-candidate (cold ARP, no checksum offload, …) and AF_UNIX
/// fall back to the single-direct-copy leaf, whose buffer is reusable the
/// instant the copy returns, so both CQEs post immediately
/// ([`Outcome::InlineNotif`]). A real error posts a single CQE.
pub fn send_zc_fixed(
    file: &FileRef,
    index: u16,
    len: u32,
    user_data: u64,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    // `None` means not a zero-copy candidate: fall through to the copy leaf.
    if !ops.is_unix_socket() {
        let idx = handle as u32;
        if socket::socket_is_tcp(idx) {
            if let Some(out) = try_send_zc_tcp(idx, index, len, user_data, reg) {
                return out;
            }
        } else if let Some(out) = try_send_zc_inet(idx, index, len, user_data, reg) {
            return out;
        }
    }

    let mut reader = match reg.fixed_reader(index, len as usize) {
        Ok(r) => r,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let rc: i32 = if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        with_unix_forced_nonblock(sh, || unix_socket::unix_send_from(sh, &mut reader))
    } else {
        let idx = handle as u32;
        with_inet_forced_nonblock(idx, || socket::socket_send_pinned(idx, &mut reader)) as i32
    };
    if rc == EAGAIN {
        Outcome::WouldBlock
    } else if rc < 0 {
        Outcome::Inline(rc) // failed before the buffer was used → single CQE
    } else {
        Outcome::InlineNotif(rc) // sent → result (F_MORE) then F_NOTIF (copied)
    }
}

/// Attempt a true NIC-DMA zero-copy send of fixed buffer `index` on AF_INET
/// socket `idx`. `Some(DeferredNotif)` was queued to the NIC and the buffer
/// stays checked out until the token flips; `Some(WouldBlock)` is a full device
/// TX ring; `None` is not a candidate and submits nothing, so the caller's copy
/// fallback is sound. The reclaim snapshot is taken **before** the submit, so a
/// fast reclaim is never missed.
fn try_send_zc_inet(
    idx: u32,
    index: u16,
    len: u32,
    user_data: u64,
    reg: &mut BufferRegistry,
) -> Option<Outcome> {
    let token = TxReclaimToken::new()?;
    let snapshot = token.snapshot();
    let slices = reg.fixed_io_slices(index, len as usize).ok()?;
    if slices.is_empty() {
        return None; // empty datagram → copy leaf
    }
    let keepalive = reg.fixed_keepalive(index)?;
    // The reader borrows `reg`, so scope it to free the `&mut reg` for
    // `push_deferred`. ICMP uses it for its CPU-side checksum; UDP ignores it.
    let outcome = {
        let mut reader = reg.fixed_reader(index, len as usize).ok()?;
        socket::socket_send_zerocopy(
            idx,
            &slices,
            &mut reader,
            len as usize,
            keepalive,
            token.clone(),
        )
    };
    match outcome {
        ZcSendOutcome::Submitted(n) => {
            reg.push_deferred(user_data, token, snapshot, index);
            Some(Outcome::DeferredNotif(n as i32))
        }
        ZcSendOutcome::WouldBlock => Some(Outcome::WouldBlock),
        ZcSendOutcome::NotEligible => None,
    }
}

/// Attempt a TCP `MSG_ZEROCOPY` send of fixed buffer `index` on connected TCP
/// socket `idx`. Unlike [`try_send_zc_inet`]'s one-shot generation token, this
/// enqueues a send-queue chunk holding the pinned pages across retransmits,
/// keyed on a refcounted [`ZcNotifToken`]; the deferred `F_NOTIF` fires once the
/// bytes are cumulatively ACKed and every in-flight DMA is reclaimed. Nothing is
/// left dangling on `None`: the keepalive and token are dropped inside
/// `socket_send_zerocopy_tcp`.
fn try_send_zc_tcp(
    idx: u32,
    index: u16,
    len: u32,
    user_data: u64,
    reg: &mut BufferRegistry,
) -> Option<Outcome> {
    if len == 0 {
        return None; // empty send → copy leaf
    }
    let token = ZcNotifToken::new()?;
    let keepalive = reg.fixed_keepalive(index)?;
    let base_off = reg.fixed_base_off(index)?;
    match socket::socket_send_zerocopy_tcp(idx, keepalive, base_off, len as usize, token.clone()) {
        ZcSendOutcome::Submitted(n) => {
            reg.push_deferred_notif(user_data, token, index);
            Some(Outcome::DeferredNotif(n as i32))
        }
        ZcSendOutcome::WouldBlock => Some(Outcome::WouldBlock),
        ZcSendOutcome::NotEligible => None,
    }
}

/// `OP_RECVMSG` into registered fixed buffer `index`. Data-plane only: any
/// SCM_RIGHTS fds that arrive are released (registered buffers carry bulk data,
/// not descriptors). The recv'd bytes land in the fixed buffer, not `iov_base`.
pub fn recvmsg_fixed(
    file: &FileRef,
    index: u16,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let mut writer = match reg.fixed_writer(index) {
        Ok(w) => w,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let rc: i32 = if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        // Cap 0 makes the drain drop (close) any arriving SCM_RIGHTS aliases.
        let mut no_files: slopos_ostd::KVec<slopos_fs::FileRef> = slopos_ostd::KVec::new();
        let (read, _n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg_into(sh, &mut writer, &mut no_files, 0)
        });
        read
    } else {
        let idx = handle as u32;
        with_inet_forced_nonblock(idx, || socket::socket_recv_pinned(idx, &mut writer)) as i32
    };
    if rc == EAGAIN {
        return Outcome::WouldBlock;
    }
    if rc < 0 {
        return Outcome::Inline(rc);
    }
    Outcome::Inline(rc)
}

/// `OP_RECVMSG` into a kernel-picked provided buffer from `group`
/// (`SLOPRING_SQE_BUFFER_SELECT`), reporting the chosen `bid` in the CQE
/// (`SLOPRING_CQE_F_BUFFER`). The buffer is consumed off the ring only once data
/// actually lands, and the ring head advances atomically with the fill.
pub fn recvmsg_provided(
    table: FdTable,
    file: &FileRef,
    group: u16,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    let (handle, ops) = match socket_handle_from_ref(file) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let buf = match reg.peek_provided(group) {
        Ok(Some(b)) => b,
        Ok(None) => return Outcome::Inline(Errno::ENOBUFS.raw()),
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // The pin is validated *before* any socket consume, so a bad buffer cannot
    // lose data.
    let Some(vm_process) = table.process() else {
        return Outcome::Inline(Errno::EINVAL.raw());
    };
    let pin = match BufferRegistry::provided_pin(vm_process, buf.addr, buf.len as usize) {
        Ok(p) => p,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let mut writer = match pin.writer(0, buf.len as usize) {
        Some(w) => w,
        None => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    let rc: i32 = if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        // Cap 0 makes the drain drop (close) any arriving SCM_RIGHTS aliases.
        let mut no_files: slopos_ostd::KVec<slopos_fs::FileRef> = slopos_ostd::KVec::new();
        let (read, _n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg_into(sh, &mut writer, &mut no_files, 0)
        });
        read
    } else {
        let idx = handle as u32;
        with_inet_forced_nonblock(idx, || socket::socket_recv_pinned(idx, &mut writer)) as i32
    };
    if rc == EAGAIN {
        return Outcome::WouldBlock; // ring untouched — buffer not consumed
    }
    if rc < 0 {
        return Outcome::Inline(rc); // ring untouched — no data landed
    }
    reg.commit_provided(group);
    let flags = SLOPRING_CQE_F_BUFFER | ((buf.bid as u32) << SLOPRING_CQE_BUFFER_SHIFT);
    Outcome::InlineBuf(rc, flags)
}
