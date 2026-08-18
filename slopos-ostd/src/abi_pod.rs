//! `Pod` impls for `slopos-abi` types.
//!
//! They live here rather than in `slopos-abi` because OSTD owns the trait and
//! all of the kernel's `unsafe`.

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
