//! ELF auxiliary vector definitions (kernel-userland ABI).
//!
//! Stack layout after exec:
//!   [argc] [argv0..argvN] [NULL] [env0..envN] [NULL] [auxv entries] [AT_NULL,0]

/// Auxiliary vector entry (two u64 words).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AuxEntry {
    pub a_type: u64,
    pub a_val: u64,
}

// Type numbering follows the ELF spec and the Linux x86-64 ABI.

/// End of auxiliary vector.
pub const AT_NULL: u64 = 0;

/// Entry point of the program (not the interpreter).
pub const AT_ENTRY: u64 = 9;

/// Address of program headers in memory.
pub const AT_PHDR: u64 = 3;

/// Size of each program header entry.
pub const AT_PHENT: u64 = 4;

/// Number of program headers.
pub const AT_PHNUM: u64 = 5;

/// System page size.
pub const AT_PAGESZ: u64 = 6;

/// Base address of the interpreter (0 for static binaries).
pub const AT_BASE: u64 = 7;

/// Flags (unused, set to 0).
pub const AT_FLAGS: u64 = 8;

pub const AT_UID: u64 = 11;

pub const AT_EUID: u64 = 12;

pub const AT_GID: u64 = 13;

pub const AT_EGID: u64 = 14;

/// Secure mode boolean (0 = normal).
pub const AT_SECURE: u64 = 23;

/// Address of 16 kernel-supplied random bytes.
pub const AT_RANDOM: u64 = 25;
