//! W/L Currency System — The Wheel of Fate's Ledger
//!
//! Tracks wins and losses as the kernel gambles with destiny. The balance
//! reflects how well the system is treating userland: every successful
//! syscall earns a win, every failed syscall takes a loss.
//!
//! # Instrumentation Boundary
//!
//! W/L balance mutations are applied at **system call boundaries** only:
//! - `SyscallContext::ok()`  → win
//! - `SyscallContext::err()` → loss
//!
//! The sole exception is `fate_api::fate_apply_outcome`, which directly
//! adjusts the balance as part of the Wheel of Fate roulette mechanic.
//!
//! Internal subsystems (heap allocator, filesystem internals, drivers,
//! boot sequences) must **not** adjust the balance directly. Doing so
//! inflates it with internal noise — a single user operation can trigger
//! hundreds of allocations, and each one should not independently move
//! the needle.
//!
//! # API Design
//!
//! This module deliberately does **not** export named `award_win`/`award_loss`
//! helpers. Those semantics belong to the syscall layer (`slopos_core`), which
//! wraps [`adjust_balance`] with the correct W/L delta. Keeping the mutation
//! API low-level here discourages drivers and other subsystems from reaching
//! in to award wins/losses where they shouldn't.

use core::sync::atomic::{AtomicI64, Ordering};

static BALANCE: AtomicI64 = AtomicI64::new(0);

pub const WL_DELTA: i64 = 10;

pub fn reset() {
    BALANCE.store(0, Ordering::Relaxed);
}

pub fn check_balance() -> i64 {
    BALANCE.load(Ordering::Relaxed)
}

/// Only valid callers: `SyscallContext::ok()`/`err()` and `fate_api::fate_apply_outcome`.
#[inline]
pub fn adjust_balance(delta: i64) {
    BALANCE.fetch_add(delta, Ordering::Relaxed);
}
