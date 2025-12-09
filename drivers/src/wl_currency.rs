use core::sync::atomic::{AtomicI64, Ordering};
use crate::serial_println;

static BALANCE: AtomicI64 = AtomicI64::new(0);

pub fn reset() {
    BALANCE.store(0, Ordering::Relaxed);
}

pub fn award_win() {
    let new = BALANCE.fetch_add(10, Ordering::Relaxed) + 10;
    serial_println!("Awarded +10 W. Balance now {}", new);
}

pub fn award_loss() {
    let new = BALANCE.fetch_sub(10, Ordering::Relaxed) - 10;
    serial_println!("Took an L (-10 W). Balance now {}", new);
}

pub fn check_balance() -> i64 {
    BALANCE.load(Ordering::Relaxed)
}

