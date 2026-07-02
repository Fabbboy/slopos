#![feature(restricted_std)]

//! Panic-recovery syscall smoke: invoke `SYSCALL_TEST_PANIC` so the kernel
//! panics inside this task's syscall context. With `panic.recover_smoke=on`
//! the kernel recovers by killing this task — the process never returns
//! from the syscall, and the boot-log transcript carries the
//! `panic recovery: syscall` line. If the syscall returns (any value), the
//! recovery boundary did not engage; report failure loudly.

// Pull in the userland lib so its `_start` ELF entry point is linked.
use slopos_userland as _;

fn main() {
    let ret = unsafe { slopos_slibc::pal::raw::syscall0(slopos_abi::syscall::SYSCALL_TEST_PANIC) };
    println!("SYSCALL PANIC SMOKE FAIL: syscall returned {ret:#x}");
    std::process::exit(1);
}
