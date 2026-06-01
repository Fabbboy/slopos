//! Opcode dispatch (SLOPRING § 12).
//!
//! Each opcode is a thin adapter that runs the *existing* sync path's
//! **non-blocking probe** and yields one of:
//!   * `Outcome::Inline(res)` — ready now (or a validation/`-EFAULT`
//!     failure): post the CQE immediately;
//!   * `Outcome::WouldBlock`  — `-EAGAIN`: record an in-flight row, let
//!     the harvest phase re-probe (SLOPRING § 7).
//!
//! Because every opcode calls the same code path as its equivalent sync
//! syscall, observable results match (R12 parity). No opcode introduces
//! a new blocking primitive.

use core::ffi::c_int;

use slopos_abi::Errno;
use slopos_abi::ring::{
    OP_ACCEPT, OP_CANCEL, OP_CLOSE, OP_NOP, OP_OPENAT, OP_POLL_ADD, OP_READ, OP_RECVFROM,
    OP_RECVMSG, OP_SEND, OP_SEND_ZC, OP_TIMEOUT, OP_WRITE, SLOPRING_SQE_BUFFER_SELECT,
    SLOPRING_SQE_FIXED_BUFFER, SLOPRING_SQE_MULTISHOT, Sqe,
};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};

use slopos_fs::fileio::{
    file_close_fd, file_open_for_process, file_read_fd_nonblock, file_write_fd_nonblock,
};
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};
use slopos_mm::user_ptr::UserBytes;

use crate::buffers::{BufSel, BufferRegistry};
use crate::ring_obj::InFlight;

/// What a probe yielded.
pub enum Outcome {
    /// Ready (or failed) now — post this `res` as the CQE.
    Inline(i32),
    /// Ready now, and carrying provided-buffer CQE bits to OR into the
    /// completion (`SLOPRING_CQE_F_BUFFER | bid << SHIFT`).
    InlineBuf(i32, u32),
    /// Would block — record an in-flight row and harvest later.
    WouldBlock,
    /// Zero-copy send completed (`OP_SEND_ZC`): post **two** CQEs for this
    /// `user_data` — a result CQE carrying `res` + `SLOPRING_CQE_F_MORE`
    /// ("notification to follow"), then a terminal CQE carrying
    /// `SLOPRING_CQE_F_NOTIF` once the send buffer is reusable (the io_uring
    /// `SEND_ZC` two-CQE model). On the single-direct-copy backend the buffer
    /// is reusable the instant the copy completes, so the notification is
    /// posted immediately after the result (io_uring's `COPIED` fallback).
    InlineNotif(i32),
}

/// Decode the buffer selection an SQE requests (mutually exclusive flags). The
/// fixed-buffer flag takes precedence; `None` is the inline path.
pub fn buf_sel(sqe: &Sqe) -> Option<BufSel> {
    if sqe.flags & SLOPRING_SQE_FIXED_BUFFER != 0 {
        Some(BufSel::Fixed {
            index: sqe.buf_index,
        })
    } else if sqe.flags & SLOPRING_SQE_BUFFER_SELECT != 0 {
        Some(BufSel::Provided {
            group: sqe.buf_group,
        })
    } else {
        None
    }
}

/// The would-block sentinel the fs/net probes return. `Errno::raw()` is
/// *already* negative (`-11`), so this is `-EAGAIN` directly — negating
/// it would yield `+11` and silently misclassify every would-block as an
/// inline completion (the bug that made `OP_READ`/`OP_WRITE` never defer).
const EAGAIN: i32 = Errno::EAGAIN.raw();

/// `true` iff `res` is the `-EAGAIN` would-block sentinel.
fn is_eagain(res: i64) -> bool {
    res == EAGAIN as i64
}

/// Build an [`InFlight`] row from an SQE snapshot.
pub fn inflight_from(sqe: &Sqe, deadline_ms: u64) -> InFlight {
    InFlight {
        user_data: sqe.user_data,
        opcode: sqe.opcode,
        fd: sqe.fd,
        addr: sqe.addr,
        addr2: sqe.addr2,
        len: sqe.len,
        op_flags: sqe.op_flags,
        off: sqe.off,
        deadline_ms,
        is_multishot: sqe.sqe_flags2 & SLOPRING_SQE_MULTISHOT != 0,
        last_revents: 0,
        buf_group: sqe.buf_group,
        buf_index: sqe.buf_index,
        buf_flags: sqe.flags,
    }
}

/// Run the non-blocking probe for one SQE in process `pid`. Pure
/// dispatch — never blocks. `OP_CANCEL` / `OP_TIMEOUT` are handled by
/// the caller (they touch ring state), so this returns `Inline(EINVAL)`
/// for them if it ever sees them (defence in depth).
pub fn probe(pid: u32, sqe: &Sqe, buffers: &mut BufferRegistry) -> Outcome {
    let sel = buf_sel(sqe);
    match sqe.opcode {
        OP_NOP => reject_buf(sel).unwrap_or(Outcome::Inline(0)),
        OP_READ => reject_buf(sel).unwrap_or_else(|| probe_read(pid, sqe)),
        OP_WRITE => reject_buf(sel).unwrap_or_else(|| probe_write(pid, sqe)),
        // OP_SEND / OP_RECVMSG are socket-typed: they route through the
        // socket send/recvmsg paths, not the generic file write/read
        // (SLOPRING § 12). Separate arms from OP_WRITE/OP_READ. With a buffer
        // selection they use the registered/provided buffer (zero staging
        // alloc); `sel == None` keeps the inline path byte-for-byte.
        OP_SEND => match sel {
            None => crate::net_glue::send_nonblock(pid, sqe.fd, sqe.addr, sqe.len, sqe.op_flags),
            Some(BufSel::Fixed { index }) => {
                crate::net_glue::send_fixed(pid, sqe.fd, index, sqe.len, sqe.op_flags, buffers)
            }
            // A provided ring is kernel-picks-on-fill, which only makes sense
            // for recv; send must name its data (a fixed buffer).
            Some(BufSel::Provided { .. }) => Outcome::Inline(Errno::EINVAL.raw()),
        },
        // OP_SEND_ZC: zero-copy send from a registered fixed buffer. Requires
        // the fixed-buffer flag (it must name its pinned data); the inline /
        // provided selections are rejected. On success it posts the two-CQE
        // notification protocol (result + F_NOTIF) — see `send_zc_fixed`.
        OP_SEND_ZC => match sel {
            Some(BufSel::Fixed { index }) => {
                crate::net_glue::send_zc_fixed(pid, sqe.fd, index, sqe.len, sqe.op_flags, buffers)
            }
            None | Some(BufSel::Provided { .. }) => Outcome::Inline(Errno::EINVAL.raw()),
        },
        OP_RECVMSG => match sel {
            None => crate::net_glue::recvmsg_nonblock(pid, sqe.fd, sqe.addr, sqe.op_flags),
            Some(BufSel::Fixed { index }) => {
                crate::net_glue::recvmsg_fixed(pid, sqe.fd, index, sqe.op_flags, buffers)
            }
            Some(BufSel::Provided { group }) => {
                crate::net_glue::recvmsg_provided(pid, sqe.fd, group, sqe.op_flags, buffers)
            }
        },
        OP_RECVFROM => reject_buf(sel).unwrap_or_else(|| {
            crate::net_glue::recvfrom_nonblock(pid, sqe.fd, sqe.addr, sqe.len, sqe.addr2)
        }),
        OP_OPENAT => reject_buf(sel).unwrap_or_else(|| probe_openat(pid, sqe)),
        OP_CLOSE => reject_buf(sel).unwrap_or_else(|| probe_close(pid, sqe)),
        OP_ACCEPT => reject_buf(sel).unwrap_or_else(|| probe_accept(pid, sqe)),
        OP_POLL_ADD => reject_buf(sel).unwrap_or_else(|| probe_poll(pid, sqe)),
        // OP_TIMEOUT / OP_CANCEL are handled in enter.rs (they touch the
        // ring object, not an fd). Reaching here is a logic error.
        OP_TIMEOUT | OP_CANCEL => Outcome::Inline(Errno::EINVAL.raw()),
        _ => Outcome::Inline(Errno::EINVAL.raw()),
    }
}

/// A buffer selection on an opcode that does not support one is `-EINVAL`;
/// `None` lets the caller run the opcode's normal (inline) path.
fn reject_buf(sel: Option<BufSel>) -> Option<Outcome> {
    sel.map(|_| Outcome::Inline(Errno::EINVAL.raw()))
}

/// Re-probe an in-flight row at harvest time. Same dispatch as `probe`,
/// reconstructed from the stored row (re-validating the user buffer via
/// a fresh `UserReadBuf`/`UserWriteBuf` each time — SLOPRING § 9).
pub fn reprobe(pid: u32, row: &InFlight, buffers: &mut BufferRegistry) -> Outcome {
    let sqe = Sqe {
        opcode: row.opcode,
        // Carry the buffer-selection flags + indices so the deferred reprobe
        // re-applies the same registered/provided buffer (dropping them — the
        // original code's `flags: 0, buf_*: 0` — silently lost the selection
        // on the would-block path).
        flags: row.buf_flags,
        _pad0: 0,
        fd: row.fd,
        off: row.off,
        addr: row.addr,
        len: row.len,
        op_flags: row.op_flags,
        user_data: row.user_data,
        addr2: row.addr2,
        sqe_flags2: 0,
        buf_group: row.buf_group,
        buf_index: row.buf_index,
        _resv0: 0,
        _resv1: 0,
    };
    probe(pid, &sqe, buffers)
}

fn probe_read(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let Some(mut buf) = UserWriteBuf::new(sqe.addr, sqe.len as usize) else {
        return Outcome::Inline(Errno::EFAULT.raw());
    };
    let rc = file_read_fd_nonblock(pid, sqe.fd as c_int, &mut buf);
    if is_eagain(rc as i64) {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc as i32)
    }
}

fn probe_write(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let Some(buf) = UserReadBuf::new(sqe.addr, sqe.len as usize) else {
        return Outcome::Inline(Errno::EFAULT.raw());
    };
    let rc = file_write_fd_nonblock(pid, sqe.fd as c_int, &buf);
    if is_eagain(rc as i64) {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc as i32)
    }
}

/// `OP_OPENAT`: non-blocking file open (SLOPRING § 12). SlopOS fs opens
/// are immediate (no disk blocking), so this always completes inline.
/// Mirrors `syscall_fs_open` / `file_open_for_process`: copy the path
/// from `addr`/`len`, open with `op_flags`, return the new fd (`>= 0`) or
/// a negated errno. It is an ownership op (installs an fd), so the caller
/// reserves a CQE slot first. Null `addr` → `-EFAULT` inline.
fn probe_openat(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.addr == 0 {
        return Outcome::Inline(Errno::EFAULT.raw());
    }
    let path_len = (sqe.len as usize).min(slopos_abi::fs::USER_PATH_MAX);
    if path_len == 0 {
        return Outcome::Inline(Errno::EINVAL.raw());
    }
    let user = match UserBytes::try_new(sqe.addr, path_len) {
        Ok(u) => u,
        Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    let mut buf = [0u8; slopos_abi::fs::USER_PATH_MAX];
    let copied = match slopos_mm::user_copy::copy_bytes_from_user(user, &mut buf[..path_len]) {
        Ok(n) => n,
        Err(_) => return Outcome::Inline(Errno::EFAULT.raw()),
    };
    // Trim at the first NUL so a NUL-terminated user path opens the right
    // file (mirrors the syscall layer's `UserCStr` decode).
    let path = match buf[..copied].iter().position(|&b| b == 0) {
        Some(n) => &buf[..n],
        None => &buf[..copied],
    };
    Outcome::Inline(file_open_for_process(pid, path, sqe.op_flags))
}

/// `OP_CLOSE`: close `fd` via the ring (SLOPRING § 12). Inline; mirrors
/// `syscall_fs_close` / `file_close_fd`. Returns `0` or a negated errno
/// (`-EBADF` for a bad fd).
fn probe_close(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    Outcome::Inline(file_close_fd(pid, sqe.fd as c_int))
}

fn probe_accept(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    match crate::net_glue::accept_nonblock(pid, sqe.fd) {
        Ok(Some(new_fd)) => Outcome::Inline(new_fd),
        Ok(None) => Outcome::WouldBlock,
        Err(e) => Outcome::Inline(e.raw()),
    }
}

fn probe_poll(pid: u32, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let want = poll_want(sqe.op_flags);
    let revents = slopos_fs::fileio::file_poll_fd(pid, sqe.fd as c_int, want);
    if revents & POLLNVAL != 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    let ready = revents & (want | POLLERR | POLLHUP);
    if ready != 0 {
        Outcome::Inline(ready as i32)
    } else {
        Outcome::WouldBlock
    }
}

/// The poll mask an `op_flags` word requests (the bits OP_POLL_ADD wants
/// to be told about), error/hangup bits always included.
pub fn poll_want(op_flags: u32) -> u16 {
    (op_flags as u16) & (POLLIN | POLLOUT | POLLERR | POLLHUP)
}

/// Level readiness probe for an OP_POLL_ADD row — the raw `revents` the
/// same `poll(2)` query would report (`file_poll_fd` returns `POLLNVAL`
/// for a bad fd). Factored out of [`probe_poll`] so the multishot harvest
/// can diff the masked-ready set against `InFlight::last_revents` and post
/// only on a transition (the anti-flood edge), and so it can detect
/// `POLLERR`/`POLLHUP` to terminate the armed row. Returns the raw
/// `revents` bitset; the caller applies its own mask.
pub fn probe_poll_revents(pid: u32, row: &InFlight) -> u16 {
    if row.fd < 0 {
        return POLLNVAL;
    }
    let want = poll_want(row.op_flags);
    slopos_fs::fileio::file_poll_fd(pid, row.fd as c_int, want)
}

/// Does this opcode transfer ownership / consume bytes on success, so
/// it must reserve a CQE slot *before* running (SLOPRING § 11)?
pub fn is_ownership_op(opcode: u8) -> bool {
    // OP_ACCEPT / OP_OPENAT install an fd; OP_READ / OP_RECVMSG /
    // OP_RECVFROM consume kernel buffer bytes (a datagram, for RECVFROM).
    // Dropping their CQE would orphan an fd / destroy consumed data.
    matches!(
        opcode,
        OP_ACCEPT | OP_OPENAT | OP_READ | OP_RECVMSG | OP_RECVFROM
    )
}
