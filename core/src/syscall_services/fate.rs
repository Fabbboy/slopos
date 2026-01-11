use slopos_abi::fate::FateResult;
use slopos_lib::ServiceCell;

#[repr(C)]
pub struct FateServices {
    pub notify_outcome: fn(*const FateResult),
}

static FATE: ServiceCell<FateServices> = ServiceCell::new("fate");

pub fn register_fate_services(services: &'static FateServices) {
    FATE.register(services);
}

pub fn is_fate_initialized() -> bool {
    FATE.is_initialized()
}

#[inline(always)]
pub fn fate_services() -> &'static FateServices {
    FATE.get()
}

#[inline(always)]
pub fn fate_notify_outcome(result: *const FateResult) {
    (fate_services().notify_outcome)(result)
}
