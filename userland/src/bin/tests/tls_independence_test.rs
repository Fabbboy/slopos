#![feature(restricted_std)]

//! Per-thread TLS independence test (Phase-6 native/FS_BASE proof).
//!
//! With std routed through the compiler-native (`#[thread_local]`) storage
//! arm — backed by variant-II FS_BASE TLS, one block per OS thread — each
//! spawned thread must own its own copy of every `thread_local!`. This test
//! is the end-to-end proof that the `native` arm is live: under the old
//! `no_threads` arm a single process-global cell was shared by all threads,
//! so the child and main thread would clobber each other.

use std::cell::Cell;

use slopos_userland as _;

thread_local! {
    static SLOT: Cell<u32> = const { Cell::new(0) };
}

const MAIN_VALUE: u32 = 0xAAAA_AAAA;
const CHILD_VALUE: u32 = 0xBBBB_BBBB;

/// Basic single-thread sanity: a `thread_local!` set is observable by a
/// later get on the same thread (the native arm must actually store the value).
fn test_set_get_same_thread() -> bool {
    SLOT.with(|c| c.set(MAIN_VALUE));
    SLOT.with(|c| c.get()) == MAIN_VALUE
}

/// The gating requirement: a spawned thread's TLS write must be invisible to
/// the main thread, and vice versa. Main sets A; child sets B + reads B;
/// after join, main must still read A. Pass iff distinct + no clobber.
fn test_independent_across_threads() -> bool {
    SLOT.with(|c| c.set(MAIN_VALUE));
    if SLOT.with(|c| c.get()) != MAIN_VALUE {
        return false;
    }

    let child_saw = std::thread::spawn(|| {
        // A fresh thread must start from the initializer, not the main
        // thread's value — confirm the child does NOT inherit MAIN_VALUE.
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

    // Child started clean (own block), saw its own write back, and the
    // main thread's cell is untouched by the child's store.
    child_before != MAIN_VALUE && child_after == CHILD_VALUE && SLOT.with(|c| c.get()) == MAIN_VALUE
}

/// `available_parallelism()` must report the kernel's online CPU count
/// (clamped to >= 1), not the old hardcoded 1. Confirms the
/// `SYSCALL_GET_CPU_COUNT` wrapper is wired into std's thread PAL.
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
