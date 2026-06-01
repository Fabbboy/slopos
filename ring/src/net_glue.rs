//! Socket-opcode glue (SLOPRING § 12, cross-cutting reality 2).
//!
//! `OP_ACCEPT`, `OP_SEND`, and `OP_RECVMSG` are socket-typed opcodes that
//! must route through the socket send/recv/accept paths (not the generic
//! `file_read_fd`/`file_write_fd` write/read). AF_INET and AF_UNIX have
//! different ABIs, so each routes per family via `FileOps::is_unix_socket()`,
//! forcing the socket's stored nonblocking flag across the probe (and
//! restoring it). AF_INET entry points take raw user pointers and copy
//! against the current address space; AF_UNIX entry points take kernel
//! slices, so AF_UNIX marshals user↔kernel through a validated `UserBytes`
//! staging copy (SLOPRING § 12 reality 2).

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::net::{AF_INET, SockAddrIn};
use slopos_abi::ring::{SLOPRING_CQE_BUFFER_SHIFT, SLOPRING_CQE_F_BUFFER};
use slopos_abi::syscall::{CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS, SOL_SOCKET};

use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

use slopos_net::socket::ZcSendOutcome;
use slopos_net::unix_socket::SocketHandle;
use slopos_net::{socket, unix_socket, unix_socket_file_ops};

use slopos_ostd::TxReclaimToken;

use crate::buffers::BufferRegistry;
use crate::opcode::Outcome;

/// Staging-buffer cap for AF_UNIX user↔kernel marshalling, matching the
/// 4 KiB bound the `send`/`recv`/`recvmsg` syscall handlers stage through.
const STAGING_CAP: usize = 4096;

/// The would-block sentinel (`Errno::EAGAIN.raw()` is already negative).
const EAGAIN: i32 = Errno::EAGAIN.raw();

/// Run `f` with the AF_UNIX socket forced nonblocking, restoring the
/// listener/socket's original stored flag afterward (SLOPRING § 12
/// reality 1: there is no per-call nonblock argument).
///
/// A concurrent close of the same socket between the set and the restore
/// is the established forced-nonblock pattern's narrow race, bounded by
/// the socket-handle/idx validation each primitive does internally.
fn with_unix_forced_nonblock<T>(h: SocketHandle, f: impl FnOnce() -> T) -> T {
    let was = unix_socket::unix_is_nonblocking(h).unwrap_or(false);
    let _ = unix_socket::unix_set_nonblocking(h, true);
    let out = f();
    let _ = unix_socket::unix_set_nonblocking(h, was);
    out
}

/// Run `f` with the AF_INET socket forced nonblocking, restoring the
/// socket's original stored flag afterward.
fn with_inet_forced_nonblock<T>(idx: u32, f: impl FnOnce() -> T) -> T {
    let was = socket::socket_is_nonblocking(idx).unwrap_or(false);
    let _ = socket::socket_set_nonblocking(idx, true);
    let out = f();
    let _ = socket::socket_set_nonblocking(idx, was);
    out
}

/// Non-blocking accept on `fd` for process `pid`. Returns:
///   * `Ok(Some(new_fd))` — a connection was accepted and installed;
///   * `Ok(None)`         — would block (no pending connection);
///   * `Err(errno)`       — a real error (`ENOTSOCK`, `ENOMEM`, …).
///
/// Reserve-before-side-effect (SLOPRING § 11) is the *caller's*
/// responsibility — by the time we install the fd here the CQE slot is
/// already reserved, so the accepted fd can always be reported.
pub fn accept_nonblock(pid: u32, fd: i32) -> Result<Option<i32>, Errno> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(pid, fd) else {
        return Err(Errno::ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }

    if ops.is_unix_socket() {
        let listener = SocketHandle::from_usize(handle);
        // Force nonblocking across the accept, then restore the listener's
        // *original* stored flag (not hard-coded blocking — that would
        // clobber a listener the caller deliberately set nonblocking).
        let result = with_unix_forced_nonblock(listener, || unix_socket::unix_accept(listener));
        match result {
            Ok(accepted) => {
                let new_fd = slopos_fs::fileio_open_fd_with_ops(
                    pid,
                    &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                    accepted.as_usize(),
                );
                if new_fd < 0 {
                    let _ = unix_socket::unix_close(accepted);
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
        // Restore the listener's *original* stored flag after the probe.
        let accepted = with_inet_forced_nonblock(sock_idx, || {
            socket::socket_accept(sock_idx, core::ptr::null_mut(), core::ptr::null_mut())
        });
        if accepted < 0 {
            let e = Errno::from_raw(accepted).unwrap_or(Errno::EINVAL);
            return if e == Errno::EAGAIN { Ok(None) } else { Err(e) };
        }
        let new_fd = slopos_fs::fileio_open_socket_fd(pid, accepted as u32);
        if new_fd < 0 {
            let _ = socket::socket_close(accepted as u32);
            return Err(Errno::ENOMEM);
        }
        Ok(Some(new_fd))
    }
}

/// Resolve `fd` to a socket, returning `ENOTSOCK` for any non-socket fd.
fn socket_handle_for(pid: u32, fd: i32) -> Result<(usize, &'static dyn FileOps), Errno> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(pid, fd) else {
        return Err(Errno::ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    Ok((handle, ops))
}

/// Map a non-blocking socket-op return code into an [`Outcome`]: the
/// would-block sentinel (`-EAGAIN`) defers, everything else (success or a
/// real negative errno) completes inline.
fn outcome_from_rc(rc: i32) -> Outcome {
    if rc == EAGAIN {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc)
    }
}

/// `OP_SEND`: socket-only send (SLOPRING § 12). Routes per family —
/// Both families stage the user bytes into a kernel scratch buffer first
/// (via `copy_bytes_from_user`) and pass the *scratch* pointer/slice to
/// the send primitive — never a raw user VA. Forwarding the user VA to
/// `socket_send` (which reads it without fault recovery) would be a
/// TOCTOU: a concurrent munmap between validation and the read faults
/// the kernel. Neither primitive takes per-call send flags, so
/// `sqe.op_flags` is accepted and ignored, matching the `send(2)` syscall
/// path (`_flags`). The socket's stored nonblock flag is forced across
/// the probe; `-EAGAIN` defers, every other result completes inline.
pub fn send_nonblock(pid: u32, fd: i32, addr: u64, len: u32, _op_flags: u32) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    let len = (len as usize).min(STAGING_CAP);
    if addr == 0 && len != 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }

    // Stage the user bytes into a kernel buffer once, for both families.
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
        // AF_INET takes a raw pointer — pass the *scratch* pointer (kernel
        // memory the netstack can read safely), not the user VA.
        let rc = with_inet_forced_nonblock(idx, || {
            let ptr = if copied == 0 {
                core::ptr::null()
            } else {
                scratch.as_ptr()
            };
            socket::socket_send(idx, ptr, copied)
        });
        // socket_send returns i64; collapse to the CQE's i32 res.
        if rc == EAGAIN as i64 {
            Outcome::WouldBlock
        } else {
            Outcome::Inline(rc as i32)
        }
    }
}

/// `OP_RECVMSG`: socket-only recvmsg (SLOPRING § 12). Parses the user
/// `MsgHdr` at `sqe.addr`, recvs into a staging buffer, copies the data
/// out to `iov_base`/`iov_len`, and — for AF_UNIX with SCM_RIGHTS fds —
/// installs the received fds into the caller's fd table and writes the
/// `CmsgHdr` back into `control`/`control_len`. Mirrors the kernel
/// recvmsg syscall's marshalling (the same `unix_recvmsg` primitive and
/// the same fd-install + cmsg-writeback shape) so observable results
/// match (R12). AF_INET fills data only (no SCM_RIGHTS), which is
/// correct, not an error. This is an ownership op (it installs fds), so
/// the caller reserves a CQE slot before dispatch (SLOPRING § 11).
pub fn recvmsg_nonblock(pid: u32, fd: i32, addr: u64, _op_flags: u32) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    // Validate + snapshot the user MsgHdr (a null/invalid addr → EFAULT).
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
        let mut received_fds: [(usize, &'static dyn FileOps); SCM_MAX_FDS] =
            [(0, slopos_mm::memfd::dummy_file_ops()); SCM_MAX_FDS];
        let (bytes_read, n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg(sh, &mut scratch[..data_len], &mut received_fds, SCM_MAX_FDS)
        });
        if bytes_read == EAGAIN {
            // No data and no fds drained — defer (the fds drain atomically
            // with the data on the real completion).
            return Outcome::WouldBlock;
        }
        if bytes_read < 0 {
            for slot in received_fds.iter().take(n_fds) {
                slot.1.release(slot.0);
            }
            return Outcome::Inline(bytes_read);
        }
        let copied = bytes_read as usize;
        if copied > 0 && msg.iov_base != 0 {
            let user_out = match UserBytes::try_new(msg.iov_base, copied) {
                Ok(u) => u,
                Err(_) => {
                    for slot in received_fds.iter().take(n_fds) {
                        slot.1.release(slot.0);
                    }
                    return Outcome::Inline(Errno::EFAULT.raw());
                }
            };
            if copy_bytes_to_user(user_out, &scratch[..copied]).is_err() {
                for slot in received_fds.iter().take(n_fds) {
                    slot.1.release(slot.0);
                }
                return Outcome::Inline(Errno::EFAULT.raw());
            }
        }
        if n_fds > 0 {
            if let Err(e) = recvmsg_writeback_cmsg(pid, &msg, &mut received_fds[..n_fds], msg_ptr) {
                return Outcome::Inline(e.raw());
            }
        }
        Outcome::Inline(copied as i32)
    } else {
        // AF_INET stream recv: fill data only, no control fds.
        let idx = handle as u32;
        let rc = with_inet_forced_nonblock(idx, || {
            let ptr = if data_len == 0 {
                core::ptr::null_mut()
            } else {
                scratch.as_mut_ptr()
            };
            socket::socket_recv(idx, ptr, data_len)
        });
        if rc == EAGAIN as i64 {
            return Outcome::WouldBlock;
        }
        if rc < 0 {
            return Outcome::Inline(rc as i32);
        }
        let copied = rc as usize;
        if copied > 0 && msg.iov_base != 0 {
            // The bytes are already consumed from the socket by this
            // point; if the copy to iov_base faults they are lost (TCP
            // has no un-consume). This matches the recv/recvmsg syscall
            // path, which also reports EFAULT after consuming.
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

/// `OP_RECVFROM`: AF_INET datagram recv that returns the source address
/// (SLOPRING § 12, the nc UDP-listen gap). Mirrors the blocking
/// `recvfrom` syscall (`syscall_recvfrom` / `socket_recvfrom`): recv into
/// a kernel scratch (forced nonblocking), copy the data out to the user
/// buffer at `addr`, and write the source `SockAddrIn` to the validated
/// user out-pointer at `addr2`. `-EAGAIN` defers; every other result
/// (success bytes or a negative errno) completes inline. This is a
/// *consuming* op — the caller reserves a CQE slot before dispatch
/// (SLOPRING § 11), so a successful recv never drops its CQE and loses
/// the datagram. Null `addr` (with non-zero `len`) or null `addr2` →
/// `-EFAULT` before any recv (the source addr is mandatory, like
/// `recvfrom(2)`'s `src_addr` when requested).
pub fn recvfrom_nonblock(pid: u32, fd: i32, addr: u64, len: u32, addr2: u64) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    // The source-addr out-pointer is mandatory for OP_RECVFROM (it is the
    // entire point of the op); a null one is a caller error → EFAULT.
    if addr2 == 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // socket_recvfrom is AF_INET (UDP/ICMP) only — an AF_UNIX fd has no
    // datagram source address, matching `syscall_recvfrom`'s ENOTSOCK.
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
    let mut src_ip = [0u8; 4];
    let mut src_port = 0u16;
    let rc = with_inet_forced_nonblock(idx, || {
        socket::socket_recvfrom(
            idx,
            if len == 0 {
                core::ptr::null_mut()
            } else {
                scratch.as_mut_ptr()
            },
            len,
            &mut src_ip as *mut [u8; 4],
            &mut src_port as *mut u16,
        )
    });
    if rc == EAGAIN as i64 {
        return Outcome::WouldBlock;
    }
    if rc < 0 {
        return Outcome::Inline(rc as i32);
    }

    // The datagram is consumed from the socket by this point. If a copy to
    // the user buffer or out-addr faults the bytes are lost (UDP has no
    // un-consume) — this matches `syscall_recvfrom`, which also reports
    // EFAULT after consuming.
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

    // Write the source SockAddrIn to the validated user out-pointer.
    let src = SockAddrIn {
        family: AF_INET,
        port: src_port.to_be(),
        addr: src_ip,
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

/// Install the received SCM_RIGHTS fds into the caller's fd table and
/// write the `CmsgHdr` + fd array back into the user `control` buffer,
/// updating `control_len`. Mirrors the kernel recvmsg syscall's
/// cmsg-writeback exactly. On any failure the still-uninstalled handles
/// are released so no inflight fd leaks; if a copy back to user memory
/// fails *after* fds were installed, every installed fd is closed so the
/// caller (which never learns the fd numbers) cannot orphan them — an
/// fd-table-exhaustion DoS over repeated calls.
fn recvmsg_writeback_cmsg(
    pid: u32,
    msg: &MsgHdr,
    received_fds: &mut [(usize, &'static dyn FileOps)],
    msg_ptr: UserPtr<MsgHdr>,
) -> Result<(), Errno> {
    let n_fds = received_fds.len();

    if msg.control == 0 {
        for slot in received_fds.iter() {
            slot.1.release(slot.0);
        }
        return Ok(());
    }

    let hdr_size = core::mem::size_of::<CmsgHdr>();
    let needed = hdr_size + n_fds * 4;
    if (msg.control_len as usize) < needed {
        for slot in received_fds.iter() {
            slot.1.release(slot.0);
        }
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
    for (j, slot) in received_fds.iter().enumerate() {
        let (h, ops) = *slot;
        let new_fd = slopos_fs::fileio::fileio_open_fd_with_ops(pid, ops, h);
        if new_fd < 0 {
            ops.release(h);
            for later in received_fds.iter().skip(j + 1) {
                later.1.release(later.0);
            }
            // Roll back the fds installed so far (a partial install with
            // no surviving cmsg writeback would orphan them).
            for &fd in fd_nums.iter().take(j) {
                let _ = slopos_fs::fileio::file_close_fd(pid, fd);
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
            let _ = slopos_fs::fileio::file_close_fd(pid, fd);
        }
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Registered / provided buffer fast paths (ABI v2). These never allocate a
// per-op staging `KVec`, never run `copy_*_user` (SMAP), and never re-validate
// a user pointer — the buffer was pinned + validated once at registration. The
// payload is staged through the ring's reusable scratch via one volatile copy
// against the pinned pages. `buf_group == 0` and no fixed-buffer flag keep the
// inline `*_nonblock` paths above byte-for-byte unchanged.
// ---------------------------------------------------------------------------

/// `OP_SEND` from registered fixed buffer `index` (`SLOPRING_SQE_FIXED_BUFFER`).
pub fn send_fixed(
    pid: u32,
    fd: i32,
    index: u16,
    len: u32,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // Single-direct-copy: build a volatile reader over the pinned buffer and
    // have the net leaf pull straight from the pinned pages into the socket
    // buffer — no kernel scratch hop.
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

/// `OP_SEND_ZC` from registered fixed buffer `index`: the io_uring zero-copy
/// send (true NIC-DMA).
///
/// For a connected **AF_INET UDP/ICMP** socket it first attempts the true
/// NIC-DMA path ([`try_send_zc_inet`]): the NIC DMAs the payload straight from
/// the pinned pages (0 CPU copies) and the buffer is **not** reusable until the
/// device reclaims the descriptor, so it returns [`Outcome::DeferredNotif`] —
/// the result CQE (`F_MORE`) posts now and the terminal `F_NOTIF` is deferred
/// until the harvest observes the reclaim token flip. When the destination
/// isn't a zero-copy candidate (cold ARP, no checksum offload, TCP, …) it falls
/// back to the **single-direct-copy** leaf (one volatile copy pinned-pages →
/// socket buffer), whose buffer is reusable the instant the copy returns, so it
/// posts the two CQEs immediately ([`Outcome::InlineNotif`], io_uring's `COPIED`
/// fallback). AF_UNIX always uses the copy leaf. A real error (`rc < 0`) posts a
/// single CQE; a would-block defers.
pub fn send_zc_fixed(
    pid: u32,
    fd: i32,
    index: u16,
    len: u32,
    user_data: u64,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };

    // AF_INET: try true NIC-DMA zero-copy first; `None` means not a candidate,
    // fall through to the single-direct-copy leaf below.
    if !ops.is_unix_socket()
        && let Some(out) = try_send_zc_inet(handle as u32, index, len, user_data, reg)
    {
        return out;
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
/// socket `idx`. Returns:
///   * `Some(Outcome::DeferredNotif(n))` — queued to the NIC; the deferred-notif
///     row is recorded in `reg` (buffer stays checked out until the token flips);
///   * `Some(Outcome::WouldBlock)`      — device TX ring full, defer + re-probe;
///   * `None`                            — not a zero-copy candidate (or the
///     token / keepalive / slices could not be built); caller uses the copy leaf.
///
/// Nothing is submitted on the `None` path, so the copy fallback is sound. The
/// reclaim snapshot is taken **before** the submit (so a fast reclaim is never
/// missed) and `push_deferred` runs **after** a successful submit (and is
/// infallible — the table was pre-grown).
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
    // Scope the reader (it borrows `reg`) so the `&mut reg` for `push_deferred`
    // is free afterward. ICMP uses the reader for its CPU-side checksum; UDP
    // ignores it (hardware checksum offload).
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

/// `OP_RECVMSG` into registered fixed buffer `index`. Data-plane only: any
/// SCM_RIGHTS fds that arrive are released (registered buffers carry bulk data,
/// not descriptors). The recv'd bytes land in the fixed buffer, not `iov_base`.
pub fn recvmsg_fixed(
    pid: u32,
    fd: i32,
    index: u16,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // Single-direct-copy: fill the pinned buffer straight from the socket via a
    // volatile writer — no kernel scratch, no separate publish copy.
    let mut writer = match reg.fixed_writer(index) {
        Ok(w) => w,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let rc: i32 = if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        let mut fds: [(usize, &'static dyn FileOps); SCM_MAX_FDS] =
            [(0, slopos_mm::memfd::dummy_file_ops()); SCM_MAX_FDS];
        let (read, n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg_into(sh, &mut writer, &mut fds, SCM_MAX_FDS)
        });
        for slot in fds.iter().take(n_fds) {
            slot.1.release(slot.0);
        }
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
/// (`SLOPRING_SQE_BUFFER_SELECT`). Peeks the next published buffer, recvs into
/// it, reports the chosen `bid` in the CQE (`SLOPRING_CQE_F_BUFFER`). The buffer
/// is consumed off the ring only once data actually lands (a would-block leaves
/// the ring untouched), and the ring head advances atomically with the fill.
pub fn recvmsg_provided(
    pid: u32,
    fd: i32,
    group: u16,
    _op_flags: u32,
    reg: &mut BufferRegistry,
) -> Outcome {
    if fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let (handle, ops) = match socket_handle_for(pid, fd) {
        Ok(v) => v,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let buf = match reg.peek_provided(group) {
        Ok(Some(b)) => b,
        Ok(None) => return Outcome::Inline(Errno::ENOBUFS.raw()),
        Err(e) => return Outcome::Inline(e.raw()),
    };
    // Single-direct-copy: transiently pin the kernel-picked buffer and fill it
    // straight from the socket via a volatile writer — no kernel scratch. The
    // pin is validated *before* any socket consume, so a bad buffer can't lose
    // data (unlike the old consume-then-publish-fault path).
    let pin = match BufferRegistry::provided_pin(pid, buf.addr, buf.len as usize) {
        Ok(p) => p,
        Err(e) => return Outcome::Inline(e.raw()),
    };
    let mut writer = match pin.writer(0, buf.len as usize) {
        Some(w) => w,
        None => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    let rc: i32 = if ops.is_unix_socket() {
        let sh = SocketHandle::from_usize(handle);
        let mut fds: [(usize, &'static dyn FileOps); SCM_MAX_FDS] =
            [(0, slopos_mm::memfd::dummy_file_ops()); SCM_MAX_FDS];
        let (read, n_fds) = with_unix_forced_nonblock(sh, || {
            unix_socket::unix_recvmsg_into(sh, &mut writer, &mut fds, SCM_MAX_FDS)
        });
        for slot in fds.iter().take(n_fds) {
            slot.1.release(slot.0);
        }
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
    // Data landed directly in the pinned buffer; consume it off the ring.
    reg.commit_provided(group);
    let flags = SLOPRING_CQE_F_BUFFER | ((buf.bid as u32) << SLOPRING_CQE_BUFFER_SHIFT);
    Outcome::InlineBuf(rc, flags)
}
