//! Shared memory constants and types
//!
//! This module contains shared memory configuration constants and access flags.
//! For syscall numbers, see [`crate::syscall`] which is the single source of truth
//! for all syscall number definitions.

/// Read-only shared memory access flag.
pub const SHM_ACCESS_RO: u32 = 0;

/// Read-write shared memory access flag.
pub const SHM_ACCESS_RW: u32 = 1;

/// Maximum number of shared memory buffers system-wide.
pub const MAX_SHM_BUFFERS: usize = 256;

/// Maximum mappings per shared memory buffer.
pub const MAX_SHM_MAPPINGS: usize = 8;
