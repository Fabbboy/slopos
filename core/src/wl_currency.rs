use core::sync::atomic::{AtomicI64, Ordering};

static BALANCE: AtomicI64 = AtomicI64::new(0);

pub fn reset() {
    BALANCE.store(0, Ordering::Relaxed);
}

pub fn award_win() {
    BALANCE.fetch_add(10, Ordering::Relaxed);
}

pub fn award_loss() {
    BALANCE.fetch_sub(10, Ordering::Relaxed);
}

pub fn check_balance() -> i64 {
    BALANCE.load(Ordering::Relaxed)
}
