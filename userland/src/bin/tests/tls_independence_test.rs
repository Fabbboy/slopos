#![feature(restricted_std)]

//! Per-thread TLS independence test (Phase-6 native/FS_BASE proof).
//!
//! std routes through the compiler-native (`#[thread_local]`) arm, backed by
//! variant-II FS_BASE TLS with one block per OS thread.

use std::cell::Cell;

use slopos_userland as _;

thread_local! {
    static SLOT: Cell<u32> = const { Cell::new(0) };
}

const MAIN_VALUE: u32 = 0xAAAA_AAAA;
const CHILD_VALUE: u32 = 0xBBBB_BBBB;

fn test_set_get_same_thread() -> bool {
    SLOT.with(|c| c.set(MAIN_VALUE));
    SLOT.with(|c| c.get()) == MAIN_VALUE
}

fn test_independent_across_threads() -> bool {
    SLOT.with(|c| c.set(MAIN_VALUE));
    if SLOT.with(|c| c.get()) != MAIN_VALUE {
        return false;
    }

    let child_saw = std::thread::spawn(|| {
        let before = SLOT.with(|c| c.get());
        SLOT.with(|c| c.set(CHILD_VALUE));
        let after = SLOT.with(|c| c.get());
        (before, after)
    })
    .join();

    let (child_before, child_after) = match child_saw {
        Ok(v) => v,
        Err(_) => return false,
    };

    child_before != MAIN_VALUE && child_after == CHILD_VALUE && SLOT.with(|c| c.get()) == MAIN_VALUE
}

/// Confirms the `SYSCALL_GET_CPU_COUNT` wrapper is wired into std's thread PAL.
fn test_available_parallelism() -> bool {
    match std::thread::available_parallelism() {
        Ok(n) => {
            eprintln!("tls_independence: available_parallelism = {}", n.get());
            n.get() >= 1
        }
        Err(_) => false,
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("set_get_same_thread", test_set_get_same_thread),
    (
        "independent_across_threads",
        test_independent_across_threads,
    ),
    ("available_parallelism", test_available_parallelism),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
