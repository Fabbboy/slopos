use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::fate::FateResult;

#[repr(C)]
pub struct FateServices {
    pub notify_outcome: fn(*const FateResult),
}

static FATE: AtomicPtr<FateServices> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_fate_services(services: &'static FateServices) {
    let prev = FATE.swap(services as *const _ as *mut _, Ordering::Release);
    assert!(prev.is_null(), "fate services already registered");
}

pub fn is_fate_initialized() -> bool {
    !FATE.load(Ordering::Acquire).is_null()
}

#[inline(always)]
pub fn fate_services() -> &'static FateServices {
    let ptr = FATE.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "fate services not initialized");
    unsafe { &*ptr }
}

#[inline(always)]
pub fn fate_notify_outcome(result: *const FateResult) {
    (fate_services().notify_outcome)(result)
}
