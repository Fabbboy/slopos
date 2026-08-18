//! SlopRing — io_uring-style submission/completion ring ABI: the `#[repr(C)]`
//! wire layouts, opcodes and flag constants shared by the kernel `ring` crate
//! and the userland SlopRing runtime.

/// Maximum SQ entries a single `ring_setup` may request (SLOPRING § 6.1);
/// power of two.
pub const SLOPRING_MAX_ENTRIES: u32 = 4096;

/// CQ headroom multiplier over the SQ (`cq_entries = SQ_TO_CQ * sq_entries`),
/// matching Linux io_uring's default (SLOPRING § 4.3).
pub const SLOPRING_SQ_TO_CQ: u32 = 2;

/// No-op; completes immediately with `res = 0`.
pub const OP_NOP: u8 = 0;
pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
/// `recvmsg(fd, msghdr)` — `addr` is the msghdr.
pub const OP_RECVMSG: u8 = 3;
pub const OP_SEND: u8 = 4;
pub const OP_ACCEPT: u8 = 5;
/// `poll(fd, mask)`.
pub const OP_POLL_ADD: u8 = 6;
/// Harvest-wait deadline in nanoseconds.
pub const OP_TIMEOUT: u8 = 7;
/// Cancel an in-flight op by `user_data`.
pub const OP_CANCEL: u8 = 8;
/// `recvfrom(fd, buf, len, src*)` — like `OP_RECVMSG`, but returns the
/// datagram's *source* `SockAddrIn` in `addr2`.
pub const OP_RECVFROM: u8 = 9;
/// `openat(path, flags)`: `addr` = path ptr, `len` = path len, `op_flags` =
/// open flags. Installs an fd (ownership op).
pub const OP_OPENAT: u8 = 10;
/// `close(fd)`; completes inline.
pub const OP_CLOSE: u8 = 11;

/// Zero-copy send from a registered fixed buffer (`SLOPRING_SQE_FIXED_BUFFER`
/// + `Sqe.buf_index`): the result CQE carries [`SLOPRING_CQE_F_MORE`] and a
/// later terminal [`SLOPRING_CQE_F_NOTIF`] one signals the pin is dropped.
/// Sound only for single-transmit datagrams (UDP/raw); other families fall
/// back to the single-copy `OP_SEND` path (SLOPRING § 13).
pub const OP_SEND_ZC: u8 = 12;

/// `connect(fd, sockaddr)` — `addr` is the user VA of a
/// [`crate::net::SockAddrIn`] (`len` = 16) or a `SockAddrUn`. Single-CQE:
/// `res = 0` or a negated errno. Re-entrant — it initiates the handshake once
/// and then defers (`WouldBlock`) on each harvest re-probe while the
/// connection is in progress.
pub const OP_CONNECT: u8 = 13;

/// Largest opcode value (inclusive); anything above is `-EINVAL`.
pub const OP_MAX: u8 = OP_CONNECT;

/// Interim completion of an armed multishot row; cleared on the terminal CQE
/// (error / EOF / cancel), which retires the row.
pub const SLOPRING_CQE_F_MORE: u32 = 1 << 0;

/// A kernel-picked provided-buffer id is valid in the high 16 bits of `flags`
/// ([`SLOPRING_CQE_BUFFER_SHIFT`]).
pub const SLOPRING_CQE_F_BUFFER: u32 = 1 << 1;

/// Incremental provided-buffer consumption: the rest of the buffer comes back
/// in a later CQE. Reserved; not yet emitted.
pub const SLOPRING_CQE_F_BUF_MORE: u32 = 1 << 2;

/// Terminal CQE of an `OP_SEND_ZC`, posted once the kernel has released its
/// last reference to the pinned send buffer: userland may reuse it.
pub const SLOPRING_CQE_F_NOTIF: u32 = 1 << 3;

/// Bit shift of the provided-buffer id within `Cqe.flags`; matches Linux
/// io_uring's `IORING_CQE_BUFFER_SHIFT`.
pub const SLOPRING_CQE_BUFFER_SHIFT: u32 = 16;

pub const SLOPRING_CQE_BUFFER_MASK: u32 = 0xFFFF_0000;

/// Set in the shared CQ-flags word when at least one CQE was dropped because
/// the CQ was full (`cq_off_overflow` counts them). Sticky: the kernel never
/// clears it, only the next `ring_setup` does.
pub const SLOPRING_CQ_OVERFLOW: u32 = 1 << 1;

/// `OP_CANCEL`: cancel *every* in-flight op matching the target fd, not
/// just the one whose `user_data` is named.
pub const SLOPRING_ASYNC_CANCEL_ALL: u32 = 1 << 0;

/// `Sqe.sqe_flags2`: arm this op as multishot — the row stays in flight and
/// posts a CQE per event until a terminal one. Honoured for OP_ACCEPT /
/// OP_RECVMSG / OP_POLL_ADD.
pub const SLOPRING_SQE_MULTISHOT: u16 = 1 << 0;

/// `Sqe.flags`: the kernel picks a provided buffer from `Sqe.buf_group`'s
/// registered ring and reports its id in [`SLOPRING_CQE_F_BUFFER`].
/// Recv-family ops only.
pub const SLOPRING_SQE_BUFFER_SELECT: u8 = 1 << 0;

/// `Sqe.flags`: the op's data buffer is the registered fixed buffer named by
/// `Sqe.buf_index`, not the user VA in `Sqe.addr`. Mutually exclusive with
/// [`SLOPRING_SQE_BUFFER_SELECT`].
pub const SLOPRING_SQE_FIXED_BUFFER: u8 = 1 << 1;

/// Maximum registered fixed buffers in one `RING_REGISTER_BUFFERS` set.
pub const SLOPRING_MAX_FIXED_BUFFERS: u32 = 1024;

/// Per-registered-buffer byte ceiling (1 GiB, io_uring parity); also the
/// kernel-side pin ceiling.
pub const SLOPRING_MAX_REG_BUF_BYTES: u64 = 1 << 30;

/// Maximum provided-buffer groups per ring; `buf_group` is `1..=this`, and 0
/// means the inline path.
pub const SLOPRING_MAX_BUF_GROUPS: u16 = 64;

/// Maximum entries in one provided buffer ring (power of two; matches Linux
/// io_uring's `IOU_PBUF_RING` cap so a `u16` tail/head never wraps mid-ring).
pub const SLOPRING_PBUF_RING_MAX_ENTRIES: u32 = 32768;

/// `RegisterBufRingCmd.flags`: incremental buffer consumption — the remainder
/// comes back in a later CQE carrying [`SLOPRING_CQE_F_BUF_MORE`].
pub const SLOPRING_PBUF_RING_INC: u16 = 1 << 0;

/// Byte offset of the user-owned producer `tail` (`u16`) within a registered
/// provided buffer ring. Overlaps `bufs[0].resv`, exactly like Linux's
/// `io_uring_buf_ring` union, so `bufs[0]` is still a usable buffer slot.
pub const SLOPRING_PBUF_RING_TAIL_OFFSET: usize = 14;

/// The whole ring region is a single mapping (the only mode today).
pub const SLOPRING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

/// The kernel honours [`SLOPRING_SQE_MULTISHOT`]; userland gates on this bit.
pub const SLOPRING_FEAT_MULTISHOT: u32 = 1 << 1;

/// The kernel implements `ring_register` provided/fixed buffers; userland
/// gates on this bit.
pub const SLOPRING_FEAT_REG_BUFFERS: u32 = 1 << 2;

/// Reserved: wait for events even when nothing was submitted.
pub const SLOPRING_ENTER_GETEVENTS: u32 = 1 << 0;

/// One submission, 64 bytes. SQ slot `i` *is* SQE `i` — a direct array, no
/// indirection (SLOPRING § 4.1).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sqe {
    pub opcode: u8,
    /// [`SLOPRING_SQE_BUFFER_SELECT`] / [`SLOPRING_SQE_FIXED_BUFFER`].
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
    /// Op-behaviour flags ([`SLOPRING_SQE_MULTISHOT`], …).
    pub sqe_flags2: u16,
    /// Provided-buffer group id (with [`SLOPRING_SQE_BUFFER_SELECT`]); 0 for
    /// the inline path.
    pub buf_group: u16,
    /// Registered/fixed-buffer index (with [`SLOPRING_SQE_FIXED_BUFFER`]).
    pub buf_index: u16,
    /// Reserved, must be zero.
    pub _resv0: u16,
    /// Reserved, must be zero.
    pub _resv1: u64,
}

const _: () = assert!(core::mem::size_of::<Sqe>() == 64);
const _: () = assert!(core::mem::align_of::<Sqe>() == 8);
// The serializer hand-writes these fields at literal offsets; a reorder that
// shifts one must fail the build.
const _: () = assert!(core::mem::offset_of!(Sqe, sqe_flags2) == 48);
const _: () = assert!(core::mem::offset_of!(Sqe, buf_group) == 50);
const _: () = assert!(core::mem::offset_of!(Sqe, buf_index) == 52);
const _: () = assert!(core::mem::offset_of!(Sqe, _resv0) == 54);
const _: () = assert!(core::mem::offset_of!(Sqe, _resv1) == 56);

impl Sqe {
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

    /// Decode a 64-byte little-endian record. The kernel parses a private
    /// snapshot of ring memory this way, never a `&Sqe` over it (AD-3).
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

/// One completion, 16 bytes.
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

// Each struct below is hand-serialized at pinned `offset_of!` positions, so the
// wire image equals the `#[repr(C)]` layout and the kernel marshals a private
// snapshot rather than a `&T` over user memory.

/// One registered fixed buffer (a user iovec). `RING_REGISTER_BUFFERS` passes
/// `nr_args` of these at the `arg` user pointer; each is pinned and referenced
/// thereafter by its array index via `Sqe.buf_index`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufIovec {
    /// User VA of the buffer (anonymous memory only).
    pub addr: u64,
    /// Byte length (`<= SLOPRING_MAX_REG_BUF_BYTES`).
    pub len: u32,
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<BufIovec>() == 16);
const _: () = assert!(core::mem::offset_of!(BufIovec, addr) == 0);
const _: () = assert!(core::mem::offset_of!(BufIovec, len) == 8);

impl BufIovec {
    pub const SERIALIZED_LEN: usize = 16;

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.addr.to_le_bytes());
        b[8..12].copy_from_slice(&self.len.to_le_bytes());
        b[12..16].copy_from_slice(&self._pad.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            addr: u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            len: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            _pad: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        }
    }
}

/// `ring_register(RING_REGISTER_PBUF_RING)` argument: register a provided
/// buffer ring for `buf_group`. The ring at `ring_addr` is `ring_entries`
/// [`IouringBuf`] slots whose producer `tail` lives at
/// `ring_addr + SLOPRING_PBUF_RING_TAIL_OFFSET`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterBufRingCmd {
    /// User VA of the provided buffer ring (anonymous memory only).
    pub ring_addr: u64,
    /// Ring slot count (power of two, `<= SLOPRING_PBUF_RING_MAX_ENTRIES`).
    pub ring_entries: u32,
    /// Target group id (`1..=SLOPRING_MAX_BUF_GROUPS`).
    pub buf_group: u16,
    /// [`SLOPRING_PBUF_RING_INC`], or 0.
    pub flags: u16,
}

const _: () = assert!(core::mem::size_of::<RegisterBufRingCmd>() == 16);
const _: () = assert!(core::mem::offset_of!(RegisterBufRingCmd, ring_addr) == 0);
const _: () = assert!(core::mem::offset_of!(RegisterBufRingCmd, ring_entries) == 8);
const _: () = assert!(core::mem::offset_of!(RegisterBufRingCmd, buf_group) == 12);
const _: () = assert!(core::mem::offset_of!(RegisterBufRingCmd, flags) == 14);

impl RegisterBufRingCmd {
    pub const SERIALIZED_LEN: usize = 16;

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.ring_addr.to_le_bytes());
        b[8..12].copy_from_slice(&self.ring_entries.to_le_bytes());
        b[12..14].copy_from_slice(&self.buf_group.to_le_bytes());
        b[14..16].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            ring_addr: u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            ring_entries: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            buf_group: u16::from_le_bytes([b[12], b[13]]),
            flags: u16::from_le_bytes([b[14], b[15]]),
        }
    }
}

/// One provided buffer ring slot, byte-identical to Linux's
/// `struct io_uring_buf`. `resv` of slot 0 doubles as the ring `tail` (see
/// [`SLOPRING_PBUF_RING_TAIL_OFFSET`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IouringBuf {
    pub addr: u64,
    pub len: u32,
    pub bid: u16,
    pub resv: u16,
}

const _: () = assert!(core::mem::size_of::<IouringBuf>() == 16);
const _: () = assert!(core::mem::offset_of!(IouringBuf, addr) == 0);
const _: () = assert!(core::mem::offset_of!(IouringBuf, len) == 8);
const _: () = assert!(core::mem::offset_of!(IouringBuf, bid) == 12);
const _: () = assert!(core::mem::offset_of!(IouringBuf, resv) == 14);
const _: () = assert!(core::mem::offset_of!(IouringBuf, resv) == SLOPRING_PBUF_RING_TAIL_OFFSET);

impl IouringBuf {
    pub const SERIALIZED_LEN: usize = 16;

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.addr.to_le_bytes());
        b[8..12].copy_from_slice(&self.len.to_le_bytes());
        b[12..14].copy_from_slice(&self.bid.to_le_bytes());
        b[14..16].copy_from_slice(&self.resv.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            addr: u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            len: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            bid: u16::from_le_bytes([b[12], b[13]]),
            resv: u16::from_le_bytes([b[14], b[15]]),
        }
    }
}

/// Ring geometry returned by `ring_setup`, written into the head of the shared
/// region *and* copied to the user's out-pointer so userland learns the layout
/// without first reading the mapping. Offsets are bytes from the region base.
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
    pub sq_off_dropped: u32,
    pub sq_off_array: u32,

    /// Offset of the CQ `head` index (user-owned consumer cursor).
    pub cq_off_head: u32,
    /// Offset of the CQ `tail` index (kernel-owned producer cursor).
    pub cq_off_tail: u32,
    /// Offset of the CQ ring mask (`= cq_entries - 1`).
    pub cq_off_mask: u32,
    pub cq_off_overflow: u32,
    pub cq_off_array: u32,

    /// Offset of the CQ flags word (holds [`SLOPRING_CQ_OVERFLOW`]).
    pub cq_off_flags: u32,
    /// User VA the region was mapped at (filled by `ring_setup`).
    pub region_addr: u64,
    /// Total mapping length in bytes (so userland mmaps exactly this).
    pub region_bytes: u64,
}

const _: () = assert!(core::mem::size_of::<RingParams>() % 8 == 0);

// The kernel copies `to_bytes()` straight into the user's `&mut RingParams`,
// which userland reads back as a `#[repr(C)]` struct, so the wire image must
// equal the in-memory layout — including the 4 bytes of implicit padding before
// the first `u64`. Pin the offsets so a field reorder fails the build.
const _: () = assert!(core::mem::size_of::<RingParams>() == 80);
const _: () = assert!(core::mem::offset_of!(RingParams, region_addr) == 64);
const _: () = assert!(core::mem::offset_of!(RingParams, region_bytes) == 72);

impl RingParams {
    /// Exactly the `#[repr(C)]` size, so a `to_bytes()` buffer can be
    /// reinterpreted as a `RingParams` directly.
    pub const SERIALIZED_LEN: usize = core::mem::size_of::<Self>();

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

    /// Encode into the canonical little-endian byte image: every field at its
    /// true `offset_of!` position, so the image is byte-identical to the
    /// `#[repr(C)]` layout and padding bytes stay zero.
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

    /// The exact inverse of [`RingParams::to_bytes`], reading each field from
    /// its true `offset_of!` position regardless of padding.
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

const U32: u32 = 4;

/// Computed offsets and sizes for a ring with `sq_entries` SQ slots. Kernel and
/// userland both derive the region layout from this one function, so they
/// cannot disagree: header → SQ control → CQ control → SQE array → CQE array,
/// each sub-area 64-byte aligned (SLOPRING § 4.1).
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

        let params_bytes = align_up(core::mem::size_of::<RingParams>() as u32, 64);

        let sq_ctrl = params_bytes;
        let sq_off_head = sq_ctrl;
        let sq_off_tail = sq_ctrl + U32;
        let sq_off_mask = sq_ctrl + 2 * U32;
        let sq_off_dropped = sq_ctrl + 3 * U32;

        let cq_ctrl = align_up(sq_ctrl + 4 * U32, 64);
        let cq_off_head = cq_ctrl;
        let cq_off_tail = cq_ctrl + U32;
        let cq_off_mask = cq_ctrl + 2 * U32;
        let cq_off_overflow = cq_ctrl + 3 * U32;
        // Carved from the padding before the page-aligned SQE array, so the
        // flags word costs no extra region bytes.
        let cq_off_flags = cq_ctrl + 4 * U32;

        // Page-aligned: the region is backed by separate, possibly
        // non-contiguous frames, and 64 divides 4096, so no SQE can straddle
        // two of them.
        let sqe_array_off = align_up(cq_ctrl + 4 * U32, 4096);
        let sqe_bytes = sq_entries * core::mem::size_of::<Sqe>() as u32;

        // Page-aligned for the same reason (16 divides 4096).
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

    pub const fn to_params(&self) -> RingParams {
        RingParams {
            sq_entries: self.sq_entries,
            cq_entries: self.cq_entries,
            flags: SLOPRING_FEAT_SINGLE_MMAP | SLOPRING_FEAT_MULTISHOT | SLOPRING_FEAT_REG_BUFFERS,
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

    #[test]
    fn sqe_v2_layout_round_trips() {
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
        assert!(l.sqe_array_off >= l.cq_off_overflow + 4);
        assert!(l.cqe_array_off >= l.sqe_array_off + 64 * 64);
        assert!(l.cqe_array_off + 128 * 16 <= l.region_bytes);
        assert_eq!(l.region_bytes % 4096, 0);
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
        assert_eq!(
            p.flags,
            SLOPRING_FEAT_SINGLE_MMAP | SLOPRING_FEAT_MULTISHOT | SLOPRING_FEAT_REG_BUFFERS
        );
    }

    #[test]
    fn buf_iovec_round_trips() {
        let v = BufIovec {
            addr: 0x1122_3344_5566_7788,
            len: 0x9abc_def0,
            _pad: 0,
        };
        assert_eq!(BufIovec::from_bytes(&v.to_bytes()), v);
    }

    #[test]
    fn register_buf_ring_cmd_round_trips() {
        let c = RegisterBufRingCmd {
            ring_addr: 0xdead_beef_0bad_f00d,
            ring_entries: 4096,
            buf_group: 7,
            flags: SLOPRING_PBUF_RING_INC,
        };
        assert_eq!(RegisterBufRingCmd::from_bytes(&c.to_bytes()), c);
        let b = c.to_bytes();
        assert_eq!(&b[12..14], &7u16.to_le_bytes());
    }

    #[test]
    fn iouring_buf_tail_overlaps_resv() {
        let buf = IouringBuf {
            addr: 0x4000,
            len: 256,
            bid: 3,
            resv: 0x0102,
        };
        let bytes = buf.to_bytes();
        assert_eq!(
            u16::from_le_bytes([
                bytes[SLOPRING_PBUF_RING_TAIL_OFFSET],
                bytes[SLOPRING_PBUF_RING_TAIL_OFFSET + 1]
            ]),
            0x0102
        );
        assert_eq!(IouringBuf::from_bytes(&bytes), buf);
    }

    /// `to_bytes()` must place `region_addr` at the struct's true offset (64),
    /// not at the naive 15-`u32` offset (60).
    #[test]
    fn region_addr_serialized_at_struct_offset() {
        let mut p = RingParams::ZERO;
        p.region_addr = 0xAABB_CCDD_1122_3344;
        p.region_bytes = 0x3000;
        let b = p.to_bytes();
        assert_eq!(&b[64..72], &0xAABB_CCDD_1122_3344u64.to_le_bytes());
        assert_eq!(&b[72..80], &0x3000u64.to_le_bytes());
        // Bytes 60..64 are the implicit padding after `cq_off_flags`.
        assert_eq!(&b[60..64], &[0u8; 4]);
    }

    #[test]
    fn params_to_from_bytes_round_trip() {
        let mut p = RingLayout::new(16).to_params();
        p.region_addr = 0x4000_0000;
        assert_eq!(RingParams::from_bytes(&p.to_bytes()), p);

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

    #[test]
    fn cq_off_flags_in_padding_and_round_trips() {
        let l = RingLayout::new(64);
        assert_eq!(l.cq_off_flags, l.cq_off_overflow + 4);
        assert!(l.cq_off_flags + 4 <= l.sqe_array_off);
        assert_eq!(l.to_params().cq_off_flags, l.cq_off_flags);

        let mut p = l.to_params();
        p.cq_off_flags = SLOPRING_CQ_OVERFLOW;
        let round = RingParams::from_bytes(&p.to_bytes());
        assert_eq!(round.cq_off_flags, SLOPRING_CQ_OVERFLOW);
        assert_eq!(round, p);
    }
}
