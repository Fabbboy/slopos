//! Opcode dispatch (SLOPRING § 12).
//!
//! Each opcode is a thin adapter over the equivalent sync syscall's
//! **non-blocking probe**, so observable results match it (R12 parity) and no
//! opcode introduces a new blocking primitive.

use core::ffi::c_int;
use slopos_fs::fileio::FdTable;

use slopos_abi::Errno;
use slopos_abi::ring::{
    OP_ACCEPT, OP_CANCEL, OP_CLOSE, OP_CONNECT, OP_NOP, OP_OPENAT, OP_POLL_ADD, OP_READ,
    OP_RECVFROM, OP_RECVMSG, OP_SEND, OP_SEND_ZC, OP_TIMEOUT, OP_WRITE, SLOPRING_SQE_BUFFER_SELECT,
    SLOPRING_SQE_FIXED_BUFFER, SLOPRING_SQE_MULTISHOT, Sqe,
};
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};

use slopos_fs::fileio::{
    FileRef, file_close_fd, file_open_for_process, file_poll_ref, file_read_ref_nonblock,
    file_write_ref_nonblock,
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
    /// `user_data` — a result CQE carrying `res` + `SLOPRING_CQE_F_MORE`, then
    /// a terminal `SLOPRING_CQE_F_NOTIF` once the send buffer is reusable,
    /// which on the single-direct-copy backend is immediately.
    InlineNotif(i32),
    /// Zero-copy send queued for true NIC DMA (`OP_SEND_ZC`): post the result
    /// CQE with `SLOPRING_CQE_F_MORE` now and **defer** the terminal
    /// `SLOPRING_CQE_F_NOTIF` until the driver reclaims the NIC TX descriptor.
    /// The fixed buffer stays checked out across the DMA; the harvest checks it
    /// back in from the registry's deferred side table once the token flips.
    DeferredNotif(i32),
}

/// Decode the buffer selection an SQE requests. The fixed-buffer flag takes
/// precedence; `None` is the inline path.
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
/// *already* negative (`-11`), so this is `-EAGAIN` directly — negating it
/// would yield `+11` and misclassify a would-block as an inline completion.
const EAGAIN: i32 = Errno::EAGAIN.raw();

fn is_eagain(res: i64) -> bool {
    res == EAGAIN as i64
}

/// Build an [`InFlight`] row from an SQE snapshot, taking ownership of the
/// resolved file reference (`None` for fd-less rows).
pub fn inflight_from(sqe: &Sqe, deadline_ms: u64, file: Option<FileRef>) -> InFlight {
    InFlight {
        user_data: sqe.user_data,
        opcode: sqe.opcode,
        file,
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

/// Run the non-blocking probe for one SQE in process `table`. Pure dispatch —
/// never blocks. `file` is the reference resolved at submit for fd-driven
/// opcodes (`None` for the path/fd-number/no-fd opcodes, or a closed fd, which
/// the fd-driven arms report as `-EBADF` *after* the static buffer-selection
/// check). `OP_CANCEL` / `OP_TIMEOUT` are handled by the caller (they touch
/// ring state), so reaching here with one yields `Inline(EINVAL)`.
pub fn probe(
    table: FdTable,
    sqe: &Sqe,
    file: Option<&FileRef>,
    buffers: &mut BufferRegistry,
) -> Outcome {
    let sel = buf_sel(sqe);
    match sqe.opcode {
        OP_NOP => reject_buf(sel).unwrap_or(Outcome::Inline(0)),
        OP_READ => reject_buf(sel).unwrap_or_else(|| with_file(file, |f| probe_read(f, sqe))),
        OP_WRITE => reject_buf(sel).unwrap_or_else(|| with_file(file, |f| probe_write(f, sqe))),
        // Socket-typed: these route through the socket send/recvmsg paths, not
        // the generic file write/read (SLOPRING § 12).
        OP_SEND => with_file(file, |f| match sel {
            None => crate::net_glue::send_nonblock(f, sqe.addr, sqe.len, sqe.op_flags),
            Some(BufSel::Fixed { index }) => {
                crate::net_glue::send_fixed(f, index, sqe.len, sqe.op_flags, buffers)
            }
            // A provided ring is kernel-picks-on-fill: send must name its data.
            Some(BufSel::Provided { .. }) => Outcome::Inline(Errno::EINVAL.raw()),
        }),
        // Zero-copy send must name its pinned data, so only a fixed selection.
        OP_SEND_ZC => with_file(file, |f| match sel {
            Some(BufSel::Fixed { index }) => crate::net_glue::send_zc_fixed(
                f,
                index,
                sqe.len,
                sqe.user_data,
                sqe.op_flags,
                buffers,
            ),
            None | Some(BufSel::Provided { .. }) => Outcome::Inline(Errno::EINVAL.raw()),
        }),
        OP_RECVMSG => with_file(file, |f| match sel {
            None => crate::net_glue::recvmsg_nonblock(table, f, sqe.addr, sqe.op_flags),
            Some(BufSel::Fixed { index }) => {
                crate::net_glue::recvmsg_fixed(f, index, sqe.op_flags, buffers)
            }
            Some(BufSel::Provided { group }) => {
                crate::net_glue::recvmsg_provided(table, f, group, sqe.op_flags, buffers)
            }
        }),
        OP_RECVFROM => reject_buf(sel).unwrap_or_else(|| {
            with_file(file, |f| {
                crate::net_glue::recvfrom_nonblock(f, sqe.addr, sqe.len, sqe.addr2)
            })
        }),
        // Path-addressed and fd-number-addressed: no target reference held.
        OP_OPENAT => reject_buf(sel).unwrap_or_else(|| probe_openat(table, sqe)),
        OP_CLOSE => reject_buf(sel).unwrap_or_else(|| probe_close(table, sqe)),
        OP_ACCEPT => reject_buf(sel).unwrap_or_else(|| with_file(file, |f| probe_accept(table, f))),
        OP_CONNECT => reject_buf(sel).unwrap_or_else(|| with_file(file, |f| probe_connect(f, sqe))),
        OP_POLL_ADD => reject_buf(sel).unwrap_or_else(|| with_file(file, |f| probe_poll(f, sqe))),
        OP_TIMEOUT | OP_CANCEL => Outcome::Inline(Errno::EINVAL.raw()),
        _ => Outcome::Inline(Errno::EINVAL.raw()),
    }
}

/// Run `f` with the op's resolved file reference, or post `-EBADF` if the fd
/// was closed / invalid at submit (or absent on a reprobe). Applied *after*
/// `reject_buf`, so a malformed buffer selection still reports `-EINVAL` even
/// when the fd is also bad.
fn with_file(file: Option<&FileRef>, f: impl FnOnce(&FileRef) -> Outcome) -> Outcome {
    match file {
        Some(file) => f(file),
        None => Outcome::Inline(Errno::EBADF.raw()),
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
pub fn reprobe(table: FdTable, row: &InFlight, buffers: &mut BufferRegistry) -> Outcome {
    let sqe = Sqe {
        opcode: row.opcode,
        flags: row.buf_flags,
        _pad0: 0,
        // The target is addressed by the held reference, not this field.
        fd: -1,
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
    probe(table, &sqe, row.file.as_ref(), buffers)
}

fn probe_read(file: &FileRef, sqe: &Sqe) -> Outcome {
    let Some(mut buf) = UserWriteBuf::new(sqe.addr, sqe.len as usize) else {
        return Outcome::Inline(Errno::EFAULT.raw());
    };
    let rc = file_read_ref_nonblock(file, &mut buf);
    if is_eagain(rc as i64) {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc as i32)
    }
}

fn probe_write(file: &FileRef, sqe: &Sqe) -> Outcome {
    let Some(buf) = UserReadBuf::new(sqe.addr, sqe.len as usize) else {
        return Outcome::Inline(Errno::EFAULT.raw());
    };
    let rc = file_write_ref_nonblock(file, &buf);
    if is_eagain(rc as i64) {
        Outcome::WouldBlock
    } else {
        Outcome::Inline(rc as i32)
    }
}

/// `OP_OPENAT`: non-blocking file open (SLOPRING § 12). SlopOS fs opens are
/// immediate, so this always completes inline with the new fd (`>= 0`) or a
/// negated errno. An ownership op (installs an fd), so the caller reserves a
/// CQE slot first.
fn probe_openat(table: FdTable, sqe: &Sqe) -> Outcome {
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
    // Trim at the first NUL: a user path is NUL-terminated within `len`.
    let path = match buf[..copied].iter().position(|&b| b == 0) {
        Some(n) => &buf[..n],
        None => &buf[..copied],
    };
    Outcome::Inline(file_open_for_process(table, path, sqe.op_flags))
}

/// `OP_CLOSE`: close `fd` via the ring (SLOPRING § 12). Always inline; returns
/// `0` or a negated errno.
fn probe_close(table: FdTable, sqe: &Sqe) -> Outcome {
    if sqe.fd < 0 {
        return Outcome::Inline(Errno::EBADF.raw());
    }
    Outcome::Inline(file_close_fd(table, sqe.fd as c_int))
}

fn probe_accept(table: FdTable, file: &FileRef) -> Outcome {
    match crate::net_glue::accept_nonblock(table, file) {
        Ok(Some(new_fd)) => Outcome::Inline(new_fd),
        Ok(None) => Outcome::WouldBlock,
        Err(e) => Outcome::Inline(e.raw()),
    }
}

/// `OP_CONNECT`: async non-blocking connect (SLOPRING § 12). `addr` is the user
/// VA of a `SockAddrIn`; [`crate::net_glue::connect_nonblock`] initiates the
/// handshake once and polls it on each re-probe. Not an ownership op — it
/// installs no fd and consumes no bytes, so no CQE slot is pre-reserved.
fn probe_connect(file: &FileRef, sqe: &Sqe) -> Outcome {
    match crate::net_glue::connect_nonblock(file, sqe.addr, sqe.len) {
        Ok(rc) => crate::net_glue::outcome_from_rc(rc),
        Err(e) => Outcome::Inline(e.raw()),
    }
}

fn probe_poll(file: &FileRef, sqe: &Sqe) -> Outcome {
    let want = poll_want(sqe.op_flags);
    let revents = file_poll_ref(file, want);
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

/// The poll mask an `op_flags` word requests, error/hangup bits always
/// included.
pub fn poll_want(op_flags: u32) -> u16 {
    (op_flags as u16) & (POLLIN | POLLOUT | POLLERR | POLLHUP)
}

/// The raw `revents` the same `poll(2)` query would report for an OP_POLL_ADD
/// row (`POLLNVAL` for a bad fd); the caller applies its own mask. The
/// multishot harvest diffs it against `InFlight::last_revents` to post only on
/// a transition, and watches `POLLERR`/`POLLHUP` to terminate the armed row.
pub fn probe_poll_revents(row: &InFlight) -> u16 {
    let Some(file) = row.file.as_ref() else {
        return POLLNVAL;
    };
    let want = poll_want(row.op_flags);
    file_poll_ref(file, want)
}

/// Does this opcode operate on a target open file (so submit resolves its
/// fd once into a held [`FileRef`])? `OP_OPENAT` (opens a path), `OP_CLOSE`
/// (closes an fd number), `OP_NOP`, `OP_TIMEOUT`, and `OP_CANCEL` do not.
pub fn needs_file_ref(opcode: u8) -> bool {
    matches!(
        opcode,
        OP_READ
            | OP_WRITE
            | OP_SEND
            | OP_SEND_ZC
            | OP_RECVMSG
            | OP_RECVFROM
            | OP_ACCEPT
            | OP_CONNECT
            | OP_POLL_ADD
    )
}

/// Does this opcode transfer ownership / consume bytes on success, so
/// it must reserve a CQE slot *before* running (SLOPRING § 11)?
pub fn is_ownership_op(opcode: u8) -> bool {
    // Dropping their CQE would orphan an installed fd or destroy bytes already
    // consumed out of a kernel buffer.
    matches!(
        opcode,
        OP_ACCEPT | OP_OPENAT | OP_READ | OP_RECVMSG | OP_RECVFROM
    )
}

// ---------------------------------------------------------------------------
// Authority classification (SLOPRING § 12 + the authority model)
//
// The ring must not be a capability bypass: an entry point that runs work
// under a credential other than its creator's is a confused deputy by
// construction. All fourteen opcodes are classified below, and the assert
// holds the whole set to the property that makes the design sound -- no opcode
// reaches a gated capability.
//
// That is what licenses the ring carrying no `Cred` snapshot. It is not a
// claim about today's code that could quietly stop being true: an opcode that
// ever reaches a capability class has to change `opcode_capability`, and the
// assert below fails until somebody states what it now needs.
// ---------------------------------------------------------------------------

use slopos_ostd::authority::Capability;

/// What `opcode` requires of the ring's owner.
///
/// Total over the opcode space, including the invalid range: an unrecognised
/// opcode is rejected before dispatch, and classifying it as `Unimplemented`
/// keeps this function total rather than needing a fallible return.
pub const fn opcode_capability(opcode: u8) -> Capability {
    match opcode {
        // Names nothing outside the ring itself.
        OP_NOP | OP_TIMEOUT | OP_CANCEL => Capability::NoneSelf,
        // Names a descriptor the owner already holds. The fd is resolved
        // against the *creator's* table (`Ring::owner`), never the caller's,
        // which is what keeps a shared ring from laundering designation.
        OP_READ | OP_WRITE | OP_SEND | OP_SEND_ZC | OP_RECVMSG | OP_RECVFROM | OP_ACCEPT
        | OP_CONNECT | OP_POLL_ADD | OP_CLOSE => Capability::NoneFd,
        // The one opcode naming a path rather than a descriptor. Routed
        // through the same path rule as `fs_open`, so the ring is not a second
        // namespace with its own policy.
        OP_OPENAT => Capability::NoneFd,
        _ => Capability::Unimplemented,
    }
}

/// Every valid opcode, for the classification assert and the allow-list.
pub const ALL_OPCODES: [u8; 14] = [
    OP_NOP,
    OP_READ,
    OP_WRITE,
    OP_RECVMSG,
    OP_SEND,
    OP_ACCEPT,
    OP_POLL_ADD,
    OP_TIMEOUT,
    OP_CANCEL,
    OP_RECVFROM,
    OP_OPENAT,
    OP_CLOSE,
    OP_SEND_ZC,
    OP_CONNECT,
];

const _: () = {
    // The opcode set is exactly the range `probe` accepts, so a new opcode
    // added without a classification fails here rather than defaulting.
    assert!(
        ALL_OPCODES.len() == (slopos_abi::ring::OP_MAX as usize) + 1,
        "every opcode up to OP_MAX must be classified",
    );

    let mut i = 0;
    while i < ALL_OPCODES.len() {
        let cap = opcode_capability(ALL_OPCODES[i]);
        assert!(
            cap as u8 != Capability::Unimplemented as u8,
            "a valid opcode must carry a classification",
        );
        // The load-bearing property: no opcode reaches a gated capability, so
        // the ring needs no credential snapshot and cannot be a bypass.
        assert!(
            cap.bit() == 0,
            "a ring opcode reached a gated capability -- the ring would need a \
             Cred snapshot taken at ring_setup, because running an opcode under \
             a credential other than its creator's is a confused deputy",
        );
        i += 1;
    }
};

/// The opcode allow-list a ring is fixed to at `ring_setup`.
///
/// Monotone: set once at creation and never widened, so a ring cannot grow new
/// powers after the fact.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct OpcodeSet(u32);

impl OpcodeSet {
    const _FITS: () = assert!(
        slopos_abi::ring::OP_MAX < 32,
        "the opcode allow-list is a u32 bitmap",
    );

    /// Every classified opcode. What `ring_setup` installs today.
    pub const fn all() -> Self {
        let mut bits = 0u32;
        let mut i = 0;
        while i < ALL_OPCODES.len() {
            bits |= 1u32 << ALL_OPCODES[i];
            i += 1;
        }
        Self(bits)
    }

    #[inline]
    pub const fn permits(self, opcode: u8) -> bool {
        opcode <= slopos_abi::ring::OP_MAX && (self.0 & (1u32 << opcode)) != 0
    }
}
