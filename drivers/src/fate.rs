use crate::random::Lfsr64;
use slopos_abi::fate::FateResult;
use slopos_core::wl_currency;
use slopos_lib::{cpu, klog_info};

static OUTCOME_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn fate_register_outcome_hook(cb: fn(*const FateResult)) {
    OUTCOME_HOOK.store(cb as usize, core::sync::atomic::Ordering::SeqCst);
}

pub fn fate_notify_outcome(res: *const FateResult) {
    if res.is_null() {
        return;
    }
    let hook = OUTCOME_HOOK.load(core::sync::atomic::Ordering::SeqCst);
    if hook != 0 {
        unsafe {
            let cb: fn(*const FateResult) = core::mem::transmute(hook);
            cb(res);
        }
    }
}

pub enum RouletteOutcome {
    Survive,
    Panic,
}

pub struct Wheel {
    rng: Lfsr64,
}

impl Wheel {
    pub fn new() -> Self {
        Self {
            rng: Lfsr64::from_tsc(),
        }
    }

    pub fn spin(&mut self) -> RouletteOutcome {
        let roll = self.rng.next();
        klog_info!("=== KERNEL ROULETTE: Spinning the Wheel of Fate ===");
        klog_info!("Random number: 0x{:016x}", roll);
        let hook = OUTCOME_HOOK.load(core::sync::atomic::Ordering::SeqCst);
        if hook != 0 {
            unsafe {
                let cb: fn(*const FateResult) = core::mem::transmute(hook);
                let result = FateResult {
                    token: 0xC0DE_CAFE,
                    value: (roll & 0xFFFF_FFFF) as u32,
                };
                cb(&result as *const FateResult);
            }
        }
        if roll & 1 == 0 {
            wl_currency::award_loss();
            klog_info!("Even number. The wheel has spoken. Destiny awaits in the abyss.");
            RouletteOutcome::Panic
        } else {
            wl_currency::award_win();
            klog_info!("Odd number. The wizards live to gamble another boot.");
            RouletteOutcome::Survive
        }
    }
}

pub fn detonate() -> ! {
    klog_info!("=== INITIATING KERNEL PANIC (ROULETTE RESULT) ===");
    loop {
        cpu::hlt();
    }
}
