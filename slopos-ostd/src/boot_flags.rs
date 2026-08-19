use core::sync::atomic::{AtomicU32, Ordering};

static FLAGS: AtomicU32 = AtomicU32::new(0);

pub const BOOT_FLAG_ROULETTE_SKIP: u32 = 1 << 0;
pub const BOOT_FLAG_TESTS_ENABLED: u32 = 1 << 1;
pub const BOOT_FLAG_PANIC_ON_OOPS: u32 = 1 << 2;
pub const BOOT_FLAG_PANIC_RECOVER_SMOKE: u32 = 1 << 3;

/// The Wheel of Fate's loss arm may reboot the machine.
///
/// A second key on `roulette_result`, in the idiom `syscall_test_panic`
/// already uses: the capability admits a caller to the syscall, this says the
/// *image* is one where losing costs a reboot. Set for an interactive boot,
/// clear under `tests=on` — a test image must not have a user-reachable path
/// that power-cycles the machine mid-run.
pub const BOOT_FLAG_FATE_REBOOT: u32 = 1 << 4;

pub fn set_flag(flag: u32) {
    FLAGS.fetch_or(flag, Ordering::Relaxed);
}

pub fn get_flags() -> u32 {
    FLAGS.load(Ordering::Relaxed)
}

pub fn has_flag(flag: u32) -> bool {
    get_flags() & flag != 0
}
