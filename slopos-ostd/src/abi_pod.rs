//! `Pod` impls for `slopos-abi` types.
//!
//! `Pod` is OSTD's trait and `slopos-abi` is the upstream ABI crate;
//! the orphan rule permits these impls to live here even though the
//! types themselves are defined in `slopos-abi`. Centralising the
//! impls in OSTD keeps the unsafe surface inside the trusted core and
//! lets downstream kernel crates obtain byte views of these aggregates
//! through `util::byte_view::pod_*` without writing their own
//! `unsafe { from_raw_parts(... as *const u8, ...) }` blocks.
//!
//! Every impl below carries a SAFETY block naming the bit-pattern
//! justification: every byte sequence of length `size_of::<Self>()`
//! must be a valid representation of `Self`. See `slopos_ostd::Pod`
//! for the trait contract.

use slopos_abi::damage::DamageRect;
use slopos_abi::fs::UserFsEntry;
use slopos_abi::net::SockAddrIn;
use slopos_abi::unix::SockAddrUn;

use crate::Pod;

// SAFETY: `SockAddrIn` is `#[repr(C)]` over `u16 + u16 + [u8; 4]
// + [u8; 8]` with no padding (16 bytes total, asserted in the abi
// crate). All field types are primitive integers / byte arrays; every
// byte pattern represents a valid value (`Copy` already derived).
unsafe impl Pod for SockAddrIn {}

// SAFETY: `SockAddrUn` is `#[repr(C)]` over `u16 + [u8; UNIX_PATH_MAX]`
// with no padding (110 bytes total, asserted in the abi crate). All
// field types are primitive integers / byte arrays; every byte pattern
// represents a valid value (`Copy` already derived).
unsafe impl Pod for SockAddrUn {}

// SAFETY: `UserFsEntry` is `#[repr(C)]` over `[u8; 64] + u8 + u32`.
// Layout: bytes 0..64 = name, byte 64 = type_, bytes 65..68 = auto-pad,
// bytes 68..72 = size. Padding bytes 65..68 are populated by the
// kernel-side zeroed allocation (`KVec::<UserFsEntry>::zeroed`) so
// observing them does not leak sensitive memory. Every byte pattern
// otherwise represents a valid value (`Copy` already derived).
unsafe impl Pod for UserFsEntry {}

// SAFETY: `DamageRect` is `#[repr(C)]` over `i32 × 4` with no padding.
// All field types are primitive integers; every byte pattern represents
// a valid value (`Copy` already derived).
unsafe impl Pod for DamageRect {}
