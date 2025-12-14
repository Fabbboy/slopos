use core::sync::atomic::{AtomicI64, Ordering};
use slopos_lib::klog::{klog_is_enabled, KlogLevel};
use crate::serial_println;

static BALANCE: AtomicI64 = AtomicI64::new(0);

pub fn reset() {
    BALANCE.store(0, Ordering::Relaxed);
}

pub fn award_win() {
    let new = BALANCE.fetch_add(10, Ordering::Relaxed) + 10;
    if klog_is_enabled(KlogLevel::Debug) != 0 {
        serial_println!("Awarded +10 W. Balance now {}", new);
    }
}

pub fn award_loss() {
    let new = BALANCE.fetch_sub(10, Ordering::Relaxed) - 10;
    if klog_is_enabled(KlogLevel::Debug) != 0 {
        serial_println!("Took an L (-10 W). Balance now {}", new);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wl_award_loss() {
    award_loss();
}

#[unsafe(no_mangle)]
pub extern "C" fn wl_award_win() {
    award_win();
}

pub fn check_balance() -> i64 {
    BALANCE.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "C" fn wl_check_balance() -> i64 {
    check_balance()
}
