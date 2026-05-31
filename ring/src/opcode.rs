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
    OP_ACCEPT, OP_CANCEL, OP_NOP, OP_POLL_ADD, OP_READ, OP_RECVMSG, OP_SEND, OP_TIMEOUT, OP_WRITE,
    Sqe,
};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};

use slopos_fs::fileio::{file_read_fd_nonblock, file_write_fd_nonblock};
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};

use crate::ring_obj::InFlight;

/// What a probe yielded.
pub enum Outcome {
    /// Ready (or failed) now — post this `res` as the CQE.
    Inline(i32),
    /// Would block — record an in-flight row and harvest later.
    WouldBlock,
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
    }
}

/// Run the non-blocking probe for one SQE in process `pid`. Pure
/// dispatch — never blocks. `OP_CANCEL` / `OP_TIMEOUT` are handled by
/// the caller (they touch ring state), so this returns `Inline(EINVAL)`
/// for them if it ever sees them (defence in depth).
pub fn probe(pid: u32, sqe: &Sqe) -> Outcome {
    match sqe.opcode {
        OP_NOP => Outcome::Inline(0),
        OP_READ => probe_read(pid, sqe),
        OP_WRITE => probe_write(pid, sqe),
        // OP_SEND / OP_RECVMSG are socket-typed: they route through the
        // socket send/recvmsg paths, not the generic file write/read
        // (SLOPRING § 12). Separate arms from OP_WRITE/OP_READ.
        OP_SEND => crate::net_glue::send_nonblock(pid, sqe.fd, sqe.addr, sqe.len, sqe.op_flags),
        OP_RECVMSG => crate::net_glue::recvmsg_nonblock(pid, sqe.fd, sqe.addr, sqe.op_flags),
        OP_ACCEPT => probe_accept(pid, sqe),
        OP_POLL_ADD => probe_poll(pid, sqe),
        // OP_TIMEOUT / OP_CANCEL are handled in enter.rs (they touch the
        // ring object, not an fd). Reaching here is a logic error.
        OP_TIMEOUT | OP_CANCEL => Outcome::Inline(Errno::EINVAL.raw()),
        _ => Outcome::Inline(Errno::EINVAL.raw()),
    }
}

/// Re-probe an in-flight row at harvest time. Same dispatch as `probe`,
/// reconstructed from the stored row (re-validating the user buffer via
/// a fresh `UserReadBuf`/`UserWriteBuf` each time — SLOPRING § 9).
pub fn reprobe(pid: u32, row: &InFlight) -> Outcome {
    let sqe = Sqe {
        opcode: row.opcode,
        flags: 0,
        _pad0: 0,
        fd: row.fd,
        off: row.off,
        addr: row.addr,
        len: row.len,
        op_flags: row.op_flags,
        user_data: row.user_data,
        addr2: row.addr2,
        _resv: [0, 0],
    };
    probe(pid, &sqe)
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
    // Non-registering readiness probe — the same bits poll(2) would
    // report. `file_poll_fd` returns POLLNVAL for a bad fd.
    let want = (sqe.op_flags as u16) & (POLLIN | POLLOUT | POLLERR | POLLHUP);
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

/// Does this opcode transfer ownership / consume bytes on success, so
/// it must reserve a CQE slot *before* running (SLOPRING § 11)?
pub fn is_ownership_op(opcode: u8) -> bool {
    // OP_ACCEPT installs an fd; OP_READ/OP_RECVMSG consume kernel buffer
    // bytes. Dropping their CQE would orphan an fd / destroy data.
    matches!(opcode, OP_ACCEPT | OP_READ | OP_RECVMSG)
}
