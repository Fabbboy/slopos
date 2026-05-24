#![feature(restricted_std)]

pub mod apps;
pub mod gfx;
pub mod net;
pub mod program_registry;
pub mod readiness;
pub mod runtime;
pub mod syscall;
pub mod theme;
pub mod ui_utils;

pub use slopos_slibc as slibc;

pub fn init() {}

/// Process entry. Hands the raw initial stack pointer (`&argc`) to the C
/// runtime, which parses argc/argv/envp, sets up TLS from the program's
/// `PT_TLS` (discovered via `AT_PHDR`), and calls `main`. The standard
/// `_start -> __libc_start_main` contract; nothing here touches TLS.
#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "mov rdi, rsp",
        "and rsp, -16",
        "call {start}",
        "ud2",
        start = sym slopos_slibc::crt::__slibc_start,
    );
}
