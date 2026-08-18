#![feature(restricted_std)]

//! Panic-recovery smoke: `SYSCALL_TEST_PANIC` panics the kernel inside this
//! task's syscall context. Under `panic.recover_smoke=on` the kernel kills the
//! task, so the syscall must never return — a return means the recovery
//! boundary did not engage.

// Linked for its `_start` ELF entry point.
use slopos_userland as _;

fn main() {
    let ret = unsafe { slopos_slibc::pal::raw::syscall0(slopos_abi::syscall::SYSCALL_TEST_PANIC) };
    println!("SYSCALL PANIC SMOKE FAIL: syscall returned {ret:#x}");
    std::process::exit(1);
}
