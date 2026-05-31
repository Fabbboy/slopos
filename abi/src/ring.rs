//! SlopRing — io_uring-style submission/completion ring ABI.
//!
//! Single source of truth for the wire format shared between the kernel
//! `ring/` crate and the userland `slibc-ring/` runtime. See
//! `docs/SLOPRING.md` for the full design; this module pins the
//! `#[repr(C)]` layout (§ 4) and the opcode / flag constants (§ 12).
//!
//! Nothing here is `unsafe` or kernel-only: it is plain POD definitions
//! both sides agree on, exactly like [`crate::syscall::numbers`].

// ---------------------------------------------------------------------------
// Limits.
// ---------------------------------------------------------------------------

/// Maximum SQ entries a single `ring_setup` may request (SLOPRING § 6.1).
/// Power of two; the CQ is twice this, so the largest region is a few
/// hundred KiB of `Frame<RingMeta>`.
pub const SLOPRING_MAX_ENTRIES: u32 = 4096;

/// CQ headroom multiplier over the SQ (`cq_entries = SQ_TO_CQ * sq_entries`),
/// matching Linux io_uring's default (SLOPRING § 4.3).
pub const SLOPRING_SQ_TO_CQ: u32 = 2;

// ---------------------------------------------------------------------------
// Opcodes (SLOPRING § 12).
// ---------------------------------------------------------------------------

/// No-op; completes immediately with `res = 0`. Benchmark / fence.
pub const OP_NOP: u8 = 0;
/// `read(fd, buf, len)` — `file_read_fd`.
pub const OP_READ: u8 = 1;
/// `write(fd, buf, len)` — `file_write_fd`.
pub const OP_WRITE: u8 = 2;
/// `recvmsg(fd, msghdr)` — `unix_recvmsg` / `socket_recvfrom`.
pub const OP_RECVMSG: u8 = 3;
/// `send(fd, buf, len)` — `unix_send` / `socket_send`.
pub const OP_SEND: u8 = 4;
/// `accept(fd)` — `unix_accept` / `socket_accept`.
pub const OP_ACCEPT: u8 = 5;
/// `poll(fd, mask)` — `file_poll_register_fd` + readiness probe.
pub const OP_POLL_ADD: u8 = 6;
/// Harvest-wait deadline in nanoseconds (SLOPRING § 12 note).
pub const OP_TIMEOUT: u8 = 7;
/// Cancel an in-flight op by `user_data` — in-flight-table walk.
pub const OP_CANCEL: u8 = 8;
/// `recvfrom(fd, buf, len, src*)` — `socket_recvfrom`. Like `OP_RECVMSG`
/// but returns the datagram's *source* `SockAddrIn` in `addr2` (closes
/// the nc UDP-listen gap). `addr` = data buf VA, `len` = buf len,
/// `addr2` = user VA of a `SockAddrIn` out-struct.
pub const OP_RECVFROM: u8 = 9;
/// `openat(path, flags)` — `file_open_for_process`. Non-blocking file
/// open (SlopOS fs opens are immediate). `addr` = path ptr, `len` = path
/// len, `op_flags` = open flags. Installs an fd (ownership op).
pub const OP_OPENAT: u8 = 10;
/// `close(fd)` — `file_close_fd`. `fd` = fd to close. Completes inline.
pub const OP_CLOSE: u8 = 11;

/// Largest opcode value (inclusive). Used by the kernel to reject
/// out-of-range opcodes with `-EINVAL`.
pub const OP_MAX: u8 = OP_CLOSE;

// ---------------------------------------------------------------------------
// CQE flags (SLOPRING § 4.5).
// ---------------------------------------------------------------------------

/// Multishot continuation bit: this CQE is an *interim* completion of an
/// armed multishot row (OP_ACCEPT/OP_RECVMSG/OP_POLL_ADD) — more CQEs for
/// the same `user_data` are expected. Cleared on the terminal CQE
/// (error / EOF / cancel), which retires the row.
pub const SLOPRING_CQE_F_MORE: u32 = 1 << 0;

/// Provided-buffer id is valid in the high 16 bits of `flags`
/// ([`SLOPRING_CQE_BUFFER_SHIFT`]). Phase 4 (provided buffer rings);
/// never set in Phase 3.
pub const SLOPRING_CQE_F_BUFFER: u32 = 1 << 1;

/// Incremental provided-buffer consumption: the kernel filled part of a
/// provided buffer and will hand back the rest in a later CQE. Phase 4;
/// never set in Phase 3.
pub const SLOPRING_CQE_F_BUF_MORE: u32 = 1 << 2;

/// Bit shift of the provided-buffer id within `Cqe.flags` (the buffer id
/// occupies the high 16 bits, mirroring Linux io_uring's
/// `IORING_CQE_BUFFER_SHIFT`). Phase 4.
pub const SLOPRING_CQE_BUFFER_SHIFT: u32 = 16;

/// Mask selecting the provided-buffer id bits in `Cqe.flags`. Phase 4.
pub const SLOPRING_CQE_BUFFER_MASK: u32 = 0xFFFF_0000;

// ---------------------------------------------------------------------------
// CQ flags word (the shared `cq_off_flags` u32).
// ---------------------------------------------------------------------------

/// Set by the kernel in the shared CQ-flags word when at least one CQE
/// was dropped because the CQ was full (`cq_off_overflow` counts how
/// many). Mirrors Linux io_uring's `IORING_SQ_CQ_OVERFLOW`. Userland
/// reads it to detect a missed completion and recover.
///
/// This is a **sticky one-way latch**: once any CQE is dropped the bit
/// stays set for the ring's lifetime and is cleared only at the next
/// `ring_setup`. The kernel never clears it (the dropped completions are
/// unrecoverable, so re-arming the flag would hide a real data loss).
/// `cq_off_overflow` carries the running drop count.
pub const SLOPRING_CQ_OVERFLOW: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// SQE op_flags (SLOPRING § 10).
// ---------------------------------------------------------------------------

/// `OP_CANCEL`: cancel *every* in-flight op matching the target fd, not
/// just the one whose `user_data` is named.
pub const SLOPRING_ASYNC_CANCEL_ALL: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// SQE behaviour flags — `Sqe.sqe_flags2` (ABI v2, SLOPRING § 10).
// ---------------------------------------------------------------------------

/// `Sqe.sqe_flags2`: arm this op as **multishot** — the kernel keeps the
/// row in flight and posts a CQE (each carrying [`SLOPRING_CQE_F_MORE`])
/// every time the resource yields, until a terminal event (error / EOF /
/// cancel). Honoured for OP_ACCEPT / OP_RECVMSG / OP_POLL_ADD.
pub const SLOPRING_SQE_MULTISHOT: u16 = 1 << 0;

/// `Sqe.flags`: the kernel picks a provided buffer from `Sqe.buf_group`
/// and reports its id in [`SLOPRING_CQE_F_BUFFER`]. Phase 4 (provided
/// buffer rings); reserved (rejected) in Phase 3.
pub const SLOPRING_SQE_BUFFER_SELECT: u8 = 1 << 0;

// ---------------------------------------------------------------------------
// RingParams flags (SLOPRING § 4.3).
// ---------------------------------------------------------------------------

/// The whole ring region is a single mapping (the only mode today).
pub const SLOPRING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

/// The kernel honours [`SLOPRING_SQE_MULTISHOT`] on OP_ACCEPT /
/// OP_RECVMSG / OP_POLL_ADD. Userland gates multishot submission on this
/// bit so a new userland on an old kernel degrades gracefully.
pub const SLOPRING_FEAT_MULTISHOT: u32 = 1 << 1;

/// The kernel implements `ring_register` provided/fixed buffers. Phase 4;
/// off in Phase 3 (the `ring_register` syscall returns `-ENOSYS`).
pub const SLOPRING_FEAT_REG_BUFFERS: u32 = 1 << 2;

// ---------------------------------------------------------------------------
// `ring_enter` flags (SLOPRING § 6.2). Reserved (0) today.
// ---------------------------------------------------------------------------

/// Reserved: wait for events even when nothing was submitted.
pub const SLOPRING_ENTER_GETEVENTS: u32 = 1 << 0;

// ---------------------------------------------------------------------------
// Sqe — Submission Queue Entry (64 bytes, SLOPRING § 4.4).
// ---------------------------------------------------------------------------

/// One submission. 64 bytes, `#[repr(C)]`; SQ slot `i` *is* SQE `i`
/// (direct array, no indirection — SLOPRING § 4.1).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sqe {
    /// `OP_*` opcode.
    pub opcode: u8,
    /// SQE flags (reserved = 0 today).
    pub flags: u8,
    pub _pad0: u16,
    /// Target fd, or -1 for `OP_NOP` / `OP_TIMEOUT`.
    pub fd: i32,
    /// File offset (`OP_READ`/`OP_WRITE`) or timeout ns (`OP_TIMEOUT`).
    pub off: u64,
    /// User VA of the data buffer / msghdr / sockaddr.
    pub addr: u64,
    /// Byte length of the buffer at `addr`.
    pub len: u32,
    /// Per-opcode flags (poll mask, recv/send flags, cancel flags).
    pub op_flags: u32,
    /// Opaque correlation cookie, echoed verbatim into the CQE.
    pub user_data: u64,
    /// Secondary VA (e.g. accept's `socklen*` out-ptr).
    pub addr2: u64,
    /// Op-behaviour flags ([`SLOPRING_SQE_MULTISHOT`], …). Offset 48.
    pub sqe_flags2: u16,
    /// Provided-buffer group id (Phase 4; 0 today). Offset 50.
    pub buf_group: u16,
    /// Registered/fixed-buffer index (Phase 4; 0 today). Offset 52.
    pub buf_index: u16,
    /// Reserved, must be zero. Offset 54.
    pub _resv0: u16,
    /// Reserved, must be zero. Offset 56.
    pub _resv1: u64,
}

const _: () = assert!(core::mem::size_of::<Sqe>() == 64);
const _: () = assert!(core::mem::align_of::<Sqe>() == 8);
// ABI v2 carved named fields out of the former `_resv: [u64; 2]` (offset
// 48–63) without changing the 64-byte size. Pin their offsets so any
// future reorder that shifts them fails the build (the serializer below
// hand-writes each field at these literal offsets).
const _: () = assert!(core::mem::offset_of!(Sqe, sqe_flags2) == 48);
const _: () = assert!(core::mem::offset_of!(Sqe, buf_group) == 50);
const _: () = assert!(core::mem::offset_of!(Sqe, buf_index) == 52);
const _: () = assert!(core::mem::offset_of!(Sqe, _resv0) == 54);
const _: () = assert!(core::mem::offset_of!(Sqe, _resv1) == 56);

impl Sqe {
    /// A zeroed SQE (all fields 0, fd 0). Convenience for userland
    /// builders.
    pub const ZERO: Self = Self {
        opcode: 0,
        flags: 0,
        _pad0: 0,
        fd: 0,
        off: 0,
        addr: 0,
        len: 0,
        op_flags: 0,
        user_data: 0,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };

    /// Decode a 64-byte little-endian record into an `Sqe`. The kernel
    /// snapshots SQE bytes through the volatile `UFrame` accessor, then
    /// calls this to parse the private copy (never a `&Sqe` over ring
    /// memory — AD-3). `bytes.len()` must be at least 64.
    pub fn from_bytes(bytes: &[u8; 64]) -> Self {
        let r16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let r32 =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        let r64 = |o: usize| {
            u64::from_le_bytes([
                bytes[o],
                bytes[o + 1],
                bytes[o + 2],
                bytes[o + 3],
                bytes[o + 4],
                bytes[o + 5],
                bytes[o + 6],
                bytes[o + 7],
            ])
        };
        Self {
            opcode: bytes[0],
            flags: bytes[1],
            _pad0: u16::from_le_bytes([bytes[2], bytes[3]]),
            fd: r32(4) as i32,
            off: r64(8),
            addr: r64(16),
            len: r32(24),
            op_flags: r32(28),
            user_data: r64(32),
            addr2: r64(40),
            sqe_flags2: r16(48),
            buf_group: r16(50),
            buf_index: r16(52),
            _resv0: r16(54),
            _resv1: r64(56),
        }
    }

    /// Encode into a 64-byte little-endian record (userland builder side).
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[0] = self.opcode;
        b[1] = self.flags;
        b[2..4].copy_from_slice(&self._pad0.to_le_bytes());
        b[4..8].copy_from_slice(&self.fd.to_le_bytes());
        b[8..16].copy_from_slice(&self.off.to_le_bytes());
        b[16..24].copy_from_slice(&self.addr.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..32].copy_from_slice(&self.op_flags.to_le_bytes());
        b[32..40].copy_from_slice(&self.user_data.to_le_bytes());
        b[40..48].copy_from_slice(&self.addr2.to_le_bytes());
        b[48..50].copy_from_slice(&self.sqe_flags2.to_le_bytes());
        b[50..52].copy_from_slice(&self.buf_group.to_le_bytes());
        b[52..54].copy_from_slice(&self.buf_index.to_le_bytes());
        b[54..56].copy_from_slice(&self._resv0.to_le_bytes());
        b[56..64].copy_from_slice(&self._resv1.to_le_bytes());
        b
    }
}

// ---------------------------------------------------------------------------
// Cqe — Completion Queue Entry (16 bytes, SLOPRING § 4.5).
// ---------------------------------------------------------------------------

/// One completion. 16 bytes, `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cqe {
    /// Echoed from the originating SQE.
    pub user_data: u64,
    /// Result: `>= 0` success (bytes / fd / readiness), `< 0` negated errno.
    pub res: i32,
    /// CQE flags (e.g. [`SLOPRING_CQE_F_MORE`]).
    pub flags: u32,
}

const _: () = assert!(core::mem::size_of::<Cqe>() == 16);
const _: () = assert!(core::mem::align_of::<Cqe>() == 8);

impl Cqe {
    /// Encode into a 16-byte little-endian record (kernel post side).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.user_data.to_le_bytes());
        b[8..12].copy_from_slice(&self.res.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    /// Decode a 16-byte little-endian record (userland harvest side).
    pub fn from_bytes(bytes: &[u8; 16]) -> Self {
        Self {
            user_data: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            res: i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            flags: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }
    }
}

// ---------------------------------------------------------------------------
// RingParams — header, immutable post-setup (SLOPRING § 4.3).
// ---------------------------------------------------------------------------

/// Ring geometry returned by `ring_setup`, written into the head of the
/// shared region *and* copied to the user's out-pointer so userland
/// learns the layout without first reading the mapping.
///
/// All offsets are byte offsets from the base of the mapped region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingParams {
    /// SQ slot count (power of two).
    pub sq_entries: u32,
    /// CQ slot count (`= SLOPRING_SQ_TO_CQ * sq_entries`).
    pub cq_entries: u32,
    /// Negotiated feature bits ([`SLOPRING_FEAT_SINGLE_MMAP`]).
    pub flags: u32,
    pub _pad0: u32,

    /// Offset of the SQ `head` index (kernel-owned producer cursor read).
    pub sq_off_head: u32,
    /// Offset of the SQ `tail` index (user-owned producer cursor).
    pub sq_off_tail: u32,
    /// Offset of the SQ ring mask (`= sq_entries - 1`).
    pub sq_off_mask: u32,
    /// Offset of the SQ dropped counter.
    pub sq_off_dropped: u32,
    /// Offset of the SQE array.
    pub sq_off_array: u32,

    /// Offset of the CQ `head` index (user-owned consumer cursor).
    pub cq_off_head: u32,
    /// Offset of the CQ `tail` index (kernel-owned producer cursor).
    pub cq_off_tail: u32,
    /// Offset of the CQ ring mask (`= cq_entries - 1`).
    pub cq_off_mask: u32,
    /// Offset of the CQ overflow counter.
    pub cq_off_overflow: u32,
    /// Offset of the CQE array.
    pub cq_off_array: u32,

    /// Offset of the CQ flags word (a u32 holding [`SLOPRING_CQ_OVERFLOW`]
    /// and any future CQ-state bits). Repurposed from the former `_pad1`
    /// reserved field, so the struct offsets below are unchanged.
    pub cq_off_flags: u32,
    /// User VA the region was mapped at (filled by `ring_setup`).
    pub region_addr: u64,
    /// Total mapping length in bytes (so userland mmaps exactly this).
    pub region_bytes: u64,
}

const _: () = assert!(core::mem::size_of::<RingParams>() % 8 == 0);

// The kernel copies `RingParams::to_bytes()` straight into the user's
// `&mut RingParams` out-pointer, and userland reads it back as a
// `#[repr(C)]` struct. For that to be sound the wire image MUST equal
// the in-memory layout — including the 4 bytes of implicit padding the
// compiler inserts before the first `u64` to 8-align it. These offsets
// are asserted so any field reorder that shifts them fails the build
// (a 4-byte skip here once mapped nc's ring to 0x300000000000).
const _: () = assert!(core::mem::size_of::<RingParams>() == 80);
const _: () = assert!(core::mem::offset_of!(RingParams, region_addr) == 64);
const _: () = assert!(core::mem::offset_of!(RingParams, region_bytes) == 72);

impl RingParams {
    /// Length of the canonical little-endian byte image — exactly the
    /// `#[repr(C)]` size, so a `to_bytes()` buffer can be reinterpreted
    /// as a `RingParams` directly.
    pub const SERIALIZED_LEN: usize = core::mem::size_of::<Self>();

    /// Zeroed params (every field 0). The kernel fills this in
    /// `ring_setup`.
    pub const ZERO: Self = Self {
        sq_entries: 0,
        cq_entries: 0,
        flags: 0,
        _pad0: 0,
        sq_off_head: 0,
        sq_off_tail: 0,
        sq_off_mask: 0,
        sq_off_dropped: 0,
        sq_off_array: 0,
        cq_off_head: 0,
        cq_off_tail: 0,
        cq_off_mask: 0,
        cq_off_overflow: 0,
        cq_off_array: 0,
        cq_off_flags: 0,
        region_addr: 0,
        region_bytes: 0,
    };

    /// Encode into the canonical little-endian byte image. Every field
    /// is written at its true `offset_of!` position, so the image is
    /// byte-identical to the `#[repr(C)]` memory layout (padding bytes
    /// stay zero). The kernel copies this verbatim to the user
    /// out-pointer; userland reinterprets the bytes as a `RingParams`,
    /// so the two layouts cannot disagree.
    pub fn to_bytes(&self) -> [u8; Self::SERIALIZED_LEN] {
        let mut b = [0u8; Self::SERIALIZED_LEN];
        macro_rules! put {
            ($field:ident) => {{
                const OFF: usize = core::mem::offset_of!(RingParams, $field);
                let v = self.$field.to_le_bytes();
                b[OFF..OFF + v.len()].copy_from_slice(&v);
            }};
        }
        put!(sq_entries);
        put!(cq_entries);
        put!(flags);
        put!(_pad0);
        put!(sq_off_head);
        put!(sq_off_tail);
        put!(sq_off_mask);
        put!(sq_off_dropped);
        put!(sq_off_array);
        put!(cq_off_head);
        put!(cq_off_tail);
        put!(cq_off_mask);
        put!(cq_off_overflow);
        put!(cq_off_array);
        put!(cq_off_flags);
        put!(region_addr);
        put!(region_bytes);
        b
    }

    /// Decode the canonical little-endian byte image produced by
    /// [`RingParams::to_bytes`]. Reads each field from its true
    /// `offset_of!` position, so it is the exact inverse regardless of
    /// any padding the compiler inserts.
    pub fn from_bytes(b: &[u8; Self::SERIALIZED_LEN]) -> Self {
        macro_rules! get32 {
            ($field:ident) => {{
                const OFF: usize = core::mem::offset_of!(RingParams, $field);
                u32::from_le_bytes([b[OFF], b[OFF + 1], b[OFF + 2], b[OFF + 3]])
            }};
        }
        macro_rules! get64 {
            ($field:ident) => {{
                const OFF: usize = core::mem::offset_of!(RingParams, $field);
                u64::from_le_bytes([
                    b[OFF],
                    b[OFF + 1],
                    b[OFF + 2],
                    b[OFF + 3],
                    b[OFF + 4],
                    b[OFF + 5],
                    b[OFF + 6],
                    b[OFF + 7],
                ])
            }};
        }
        Self {
            sq_entries: get32!(sq_entries),
            cq_entries: get32!(cq_entries),
            flags: get32!(flags),
            _pad0: get32!(_pad0),
            sq_off_head: get32!(sq_off_head),
            sq_off_tail: get32!(sq_off_tail),
            sq_off_mask: get32!(sq_off_mask),
            sq_off_dropped: get32!(sq_off_dropped),
            sq_off_array: get32!(sq_off_array),
            cq_off_head: get32!(cq_off_head),
            cq_off_tail: get32!(cq_off_tail),
            cq_off_mask: get32!(cq_off_mask),
            cq_off_overflow: get32!(cq_off_overflow),
            cq_off_array: get32!(cq_off_array),
            cq_off_flags: get32!(cq_off_flags),
            region_addr: get64!(region_addr),
            region_bytes: get64!(region_bytes),
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical region layout (kernel + userland compute it identically).
// ---------------------------------------------------------------------------

/// Byte size of one `u32` control word.
const U32: u32 = 4;

/// Computed offsets and sizes for a ring with `sq_entries` SQ slots.
///
/// Both the kernel (`ring_setup`) and userland (`slibc-ring`) derive the
/// region layout from `sq_entries` with this single function, so they
/// can never disagree. Layout order matches SLOPRING § 4.1:
/// header → SQ control → CQ control → SQE array → CQE array, each
/// sub-area 64-byte aligned for cache friendliness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingLayout {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub params_bytes: u32,
    pub sq_off_head: u32,
    pub sq_off_tail: u32,
    pub sq_off_mask: u32,
    pub sq_off_dropped: u32,
    pub cq_off_head: u32,
    pub cq_off_tail: u32,
    pub cq_off_mask: u32,
    pub cq_off_overflow: u32,
    pub cq_off_flags: u32,
    pub sqe_array_off: u32,
    pub cqe_array_off: u32,
    pub region_bytes: u32,
}

/// Align `v` up to the next multiple of `a` (a power of two).
const fn align_up(v: u32, a: u32) -> u32 {
    (v + (a - 1)) & !(a - 1)
}

impl RingLayout {
    /// Compute the layout for `sq_entries` (must be a power of two in
    /// `1..=SLOPRING_MAX_ENTRIES`; callers validate first).
    pub const fn new(sq_entries: u32) -> Self {
        let cq_entries = sq_entries * SLOPRING_SQ_TO_CQ;

        // Header: a RingParams worth of bytes, 64-aligned.
        let params_bytes = align_up(core::mem::size_of::<RingParams>() as u32, 64);

        // SQ control block: head, tail, mask, dropped — 4 u32s.
        let sq_ctrl = params_bytes;
        let sq_off_head = sq_ctrl;
        let sq_off_tail = sq_ctrl + U32;
        let sq_off_mask = sq_ctrl + 2 * U32;
        let sq_off_dropped = sq_ctrl + 3 * U32;

        // CQ control block: head, tail, mask, overflow — 4 u32s.
        let cq_ctrl = align_up(sq_ctrl + 4 * U32, 64);
        let cq_off_head = cq_ctrl;
        let cq_off_tail = cq_ctrl + U32;
        let cq_off_mask = cq_ctrl + 2 * U32;
        let cq_off_overflow = cq_ctrl + 3 * U32;
        // CQ flags word: the 5th u32, carved from the currently-unused
        // padding between the CQ control block and the page-aligned SQE
        // array (which begins at `align_up(cq_ctrl + 4 * U32, 4096)`), so
        // this costs no extra region bytes.
        let cq_off_flags = cq_ctrl + 4 * U32;

        // SQE array: sq_entries * 64. Page-aligned so that — since 64
        // divides 4096 — no SQE ever straddles a page boundary. The ring
        // region is backed by separate (possibly non-contiguous) frames,
        // so a straddling entry would split across two frames; aligning
        // the array to a page removes that case entirely.
        let sqe_array_off = align_up(cq_ctrl + 4 * U32, 4096);
        let sqe_bytes = sq_entries * core::mem::size_of::<Sqe>() as u32;

        // CQE array: cq_entries * 16. Page-aligned for the same reason
        // (16 divides 4096).
        let cqe_array_off = align_up(sqe_array_off + sqe_bytes, 4096);
        let cqe_bytes = cq_entries * core::mem::size_of::<Cqe>() as u32;

        let region_bytes = align_up(cqe_array_off + cqe_bytes, 4096);

        Self {
            sq_entries,
            cq_entries,
            params_bytes,
            sq_off_head,
            sq_off_tail,
            sq_off_mask,
            sq_off_dropped,
            cq_off_head,
            cq_off_tail,
            cq_off_mask,
            cq_off_overflow,
            cq_off_flags,
            sqe_array_off,
            cqe_array_off,
            region_bytes,
        }
    }

    /// Byte offset of SQE slot `i` (caller masks `i` first).
    pub const fn sqe_off(&self, i: u32) -> u32 {
        self.sqe_array_off + i * core::mem::size_of::<Sqe>() as u32
    }

    /// Byte offset of CQE slot `i` (caller masks `i` first).
    pub const fn cqe_off(&self, i: u32) -> u32 {
        self.cqe_array_off + i * core::mem::size_of::<Cqe>() as u32
    }

    /// Build the `RingParams` header from this layout.
    pub const fn to_params(&self) -> RingParams {
        RingParams {
            sq_entries: self.sq_entries,
            cq_entries: self.cq_entries,
            flags: SLOPRING_FEAT_SINGLE_MMAP | SLOPRING_FEAT_MULTISHOT,
            _pad0: 0,
            sq_off_head: self.sq_off_head,
            sq_off_tail: self.sq_off_tail,
            sq_off_mask: self.sq_off_mask,
            sq_off_dropped: self.sq_off_dropped,
            sq_off_array: self.sqe_array_off,
            cq_off_head: self.cq_off_head,
            cq_off_tail: self.cq_off_tail,
            cq_off_mask: self.cq_off_mask,
            cq_off_overflow: self.cq_off_overflow,
            cq_off_array: self.cqe_array_off,
            cq_off_flags: self.cq_off_flags,
            region_addr: 0,
            region_bytes: self.region_bytes as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqe_round_trips() {
        let s = Sqe {
            opcode: OP_READ,
            flags: SLOPRING_SQE_BUFFER_SELECT,
            _pad0: 0,
            fd: 7,
            off: 0x1234_5678_9abc_def0,
            addr: 0xdead_beef,
            len: 4096,
            op_flags: 3,
            user_data: 0xcafe_f00d_0bad_b002,
            addr2: 0x5000,
            sqe_flags2: SLOPRING_SQE_MULTISHOT,
            buf_group: 0xabcd,
            buf_index: 0x1234,
            _resv0: 0,
            _resv1: 0,
        };
        assert_eq!(Sqe::from_bytes(&s.to_bytes()), s);
    }

    /// ABI v2: the carved fields live at their pinned offsets and survive
    /// the byte round-trip — `sqe_flags2`@48, `buf_group`@50,
    /// `buf_index`@52 — keeping `Sqe` exactly 64 bytes.
    #[test]
    fn sqe_v2_layout_round_trips() {
        assert_eq!(core::mem::size_of::<Sqe>(), 64);
        assert_eq!(core::mem::offset_of!(Sqe, sqe_flags2), 48);
        assert_eq!(core::mem::offset_of!(Sqe, buf_group), 50);
        assert_eq!(core::mem::offset_of!(Sqe, buf_index), 52);

        let mut s = Sqe::ZERO;
        s.sqe_flags2 = 0x0102;
        s.buf_group = 0x0304;
        s.buf_index = 0x0506;
        let b = s.to_bytes();
        assert_eq!(&b[48..50], &0x0102u16.to_le_bytes());
        assert_eq!(&b[50..52], &0x0304u16.to_le_bytes());
        assert_eq!(&b[52..54], &0x0506u16.to_le_bytes());
        assert_eq!(Sqe::from_bytes(&b), s);
    }

    /// CQE provided-buffer bits: a buffer id packs into the high 16 bits
    /// alongside `F_BUFFER`, and the shift/mask recover it exactly.
    #[test]
    fn cqe_buffer_bits() {
        let bid: u32 = 0xBEEF;
        let flags = SLOPRING_CQE_F_BUFFER | (bid << SLOPRING_CQE_BUFFER_SHIFT);
        let c = Cqe {
            user_data: 0xfeed,
            res: 17,
            flags,
        };
        let round = Cqe::from_bytes(&c.to_bytes());
        assert_eq!(round, c);
        assert_ne!(round.flags & SLOPRING_CQE_F_BUFFER, 0);
        assert_eq!(
            (round.flags & SLOPRING_CQE_BUFFER_MASK) >> SLOPRING_CQE_BUFFER_SHIFT,
            bid
        );
    }

    #[test]
    fn cqe_round_trips() {
        let c = Cqe {
            user_data: 0x1122_3344_5566_7788,
            res: -22,
            flags: SLOPRING_CQE_F_MORE,
        };
        assert_eq!(Cqe::from_bytes(&c.to_bytes()), c);
    }

    #[test]
    fn layout_is_non_overlapping_and_aligned() {
        let l = RingLayout::new(64);
        assert_eq!(l.sq_entries, 64);
        assert_eq!(l.cq_entries, 128);
        // SQE array begins after both control blocks.
        assert!(l.sqe_array_off >= l.cq_off_overflow + 4);
        // CQE array begins after the SQE array.
        assert!(l.cqe_array_off >= l.sqe_array_off + 64 * 64);
        // Everything fits in the region.
        assert!(l.cqe_array_off + 128 * 16 <= l.region_bytes);
        // Region is page-aligned.
        assert_eq!(l.region_bytes % 4096, 0);
        // All sub-areas 64-aligned.
        assert_eq!(l.sqe_array_off % 64, 0);
        assert_eq!(l.cqe_array_off % 64, 0);
    }

    #[test]
    fn params_match_layout() {
        let l = RingLayout::new(32);
        let p = l.to_params();
        assert_eq!(p.sq_entries, 32);
        assert_eq!(p.cq_entries, 64);
        assert_eq!(p.sq_off_array, l.sqe_array_off);
        assert_eq!(p.cq_off_array, l.cqe_array_off);
        assert_eq!(p.flags, SLOPRING_FEAT_SINGLE_MMAP | SLOPRING_FEAT_MULTISHOT);
    }

    /// Regression guard: `to_bytes()` must place `region_addr` at the
    /// struct's true offset (64), not at the naive 15-`u32` offset (60).
    /// The 4-byte skip mapped nc's ring to `region_bytes << 32`
    /// (0x300000000000 for a 16-entry ring) and page-faulted on first
    /// touch.
    #[test]
    fn region_addr_serialized_at_struct_offset() {
        assert_eq!(core::mem::offset_of!(RingParams, region_addr), 64);
        assert_eq!(core::mem::offset_of!(RingParams, region_bytes), 72);

        let mut p = RingParams::ZERO;
        p.region_addr = 0xAABB_CCDD_1122_3344;
        p.region_bytes = 0x3000;
        let b = p.to_bytes();
        assert_eq!(&b[64..72], &0xAABB_CCDD_1122_3344u64.to_le_bytes());
        assert_eq!(&b[72..80], &0x3000u64.to_le_bytes());
        // The buggy serializer wrote region_addr at 60; bytes 60..64 are
        // the implicit padding after `cq_off_flags` (offset 56) and must
        // be zero for ZERO params.
        assert_eq!(&b[60..64], &[0u8; 4]);
    }

    /// `to_bytes()` / `from_bytes()` round-trip every field exactly.
    /// Because `to_bytes()` writes each field at its `offset_of!`
    /// position, the byte image is identical to the `#[repr(C)]` memory
    /// layout — the invariant that lets userland reinterpret the kernel's
    /// out-copy as a `RingParams` struct (verified by the offset asserts
    /// above and `region_addr_serialized_at_struct_offset`).
    #[test]
    fn params_to_from_bytes_round_trip() {
        let mut p = RingLayout::new(16).to_params();
        p.region_addr = 0x4000_0000;
        assert_eq!(RingParams::from_bytes(&p.to_bytes()), p);

        // A fully-populated value (distinct bytes in every field) also
        // round-trips, catching any field that to/from disagree on.
        let p2 = RingParams {
            sq_entries: 0x0102_0304,
            cq_entries: 0x0506_0708,
            flags: 0x090a_0b0c,
            _pad0: 0x0d0e_0f10,
            sq_off_head: 0x1112_1314,
            sq_off_tail: 0x1516_1718,
            sq_off_mask: 0x191a_1b1c,
            sq_off_dropped: 0x1d1e_1f20,
            sq_off_array: 0x2122_2324,
            cq_off_head: 0x2526_2728,
            cq_off_tail: 0x292a_2b2c,
            cq_off_mask: 0x2d2e_2f30,
            cq_off_overflow: 0x3132_3334,
            cq_off_array: 0x3536_3738,
            cq_off_flags: 0x393a_3b3c,
            region_addr: 0x4142_4344_4546_4748,
            region_bytes: 0x494a_4b4c_4d4e_4f50,
        };
        assert_eq!(RingParams::from_bytes(&p2.to_bytes()), p2);
    }

    /// The CQ-flags word lives in the free padding before the
    /// page-aligned SQE array (no region growth), and `cq_off_flags`
    /// round-trips through `to_bytes`/`from_bytes` carrying the
    /// `SLOPRING_CQ_OVERFLOW` bit pattern intact.
    #[test]
    fn cq_off_flags_in_padding_and_round_trips() {
        let l = RingLayout::new(64);
        // Flags word sits just past the four CQ control u32s …
        assert_eq!(l.cq_off_flags, l.cq_off_overflow + 4);
        // … and entirely before the (page-aligned) SQE array, so it
        // consumes only otherwise-unused padding.
        assert!(l.cq_off_flags + 4 <= l.sqe_array_off);
        assert_eq!(l.to_params().cq_off_flags, l.cq_off_flags);

        let mut p = l.to_params();
        p.cq_off_flags = SLOPRING_CQ_OVERFLOW;
        let round = RingParams::from_bytes(&p.to_bytes());
        assert_eq!(round.cq_off_flags, SLOPRING_CQ_OVERFLOW);
        assert_eq!(round, p);
    }
}
