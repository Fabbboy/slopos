use core::sync::atomic::{AtomicU32, Ordering};

static FLAGS: AtomicU32 = AtomicU32::new(0);

pub const BOOT_FLAG_ROULETTE_SKIP: u32 = 1 << 0;

pub fn set_flag(flag: u32) {
    FLAGS.fetch_or(flag, Ordering::Relaxed);
}

pub fn get_flags() -> u32 {
    FLAGS.load(Ordering::Relaxed)
}

pub fn has_flag(flag: u32) -> bool {
    get_flags() & flag != 0
}
