use slopos_abi::fate::FateResult;

slopos_lib::define_service! {
    fate => FateServices {
        notify_outcome(result: *const FateResult);
    }
}

#[inline(always)]
pub fn fate_notify_outcome(result: *const FateResult) {
    notify_outcome(result)
}
