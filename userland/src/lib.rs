#![feature(restricted_std)]

pub mod appkit;
pub mod apps;
pub mod gfx;
pub mod program_registry;
pub mod readiness;
pub mod runtime;
pub mod syscall;
pub mod theme;
pub mod ui_utils;

pub use slopos_slibc as slibc;

pub fn init() {}

unsafe extern "C" {
    fn main(argc: isize, argv: *const *const u8) -> isize;
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "mov rdi, [rsp]",
        "lea rsi, [rsp + 8]",
        "and rsp, -16",
        "call {entry}",
        "mov rdi, rax",
        "mov rax, 1",
        "syscall",
        "ud2",
        entry = sym main,
    );
}
