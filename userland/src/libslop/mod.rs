//! libslop - Minimal C runtime library for SlopOS userland
//!
//! This module provides the minimal runtime support needed for userland programs:
//! - CRT0 (_start entry point with argc/argv/envp parsing)
//! - C-compatible syscall wrappers (read, write, open, close, exit)
//! - Memory allocation (malloc, free, realloc)
//!
//! # Architecture
//!
//! The kernel sets up the user stack with the standard System V AMD64 layout:
//! ```text
//! +------------------+ <- stack_top (high address)
//! | padding          |
//! +------------------+
//! | auxv[n] = 0      |  (null terminator)
//! | auxv[n-1]        |
//! | ...              |
//! | auxv[0]          |
//! +------------------+
//! | envp[n] = NULL   |  (null terminator)
//! | envp[n-1]        |
//! | ...              |
//! | envp[0]          |
//! +------------------+
//! | argv[argc] = NULL|  (null terminator)
//! | argv[argc-1]     |
//! | ...              |
//! | argv[0]          |
//! +------------------+
//! | argc             |  <- RSP points here at _start
//! +------------------+
//! ```

pub mod crt0;
pub mod ffi;
pub mod malloc;
pub mod syscall;

pub use crt0::{argc, argv, crt0_start, envp, get_arg, get_env, set_main};
pub use malloc::{alloc, calloc, dealloc, realloc};
pub use syscall::{sys_brk, sys_close, sys_exit, sys_open, sys_read, sys_sbrk, sys_write};
