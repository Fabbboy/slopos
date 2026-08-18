//! W/L balance ledger: a successful syscall earns a win, a failed one a loss.
//!
//! Mutated only at syscall boundaries (`SyscallContext::ok`/`err`) and by
//! `fate_api::fate_apply_outcome`; adjusting it from internal subsystems would
//! inflate it with per-allocation noise.

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
