use super::task::{Task, task_find_by_id};
use core::ffi::c_int;
use slopos_abi::fate::FateResult;
use slopos_kernel_services::platform;
use slopos_ostd::wl_currency::{self, WL_DELTA};

/// Run `f` against a live task, or report -1 if the id no longer resolves.
///
/// A shared borrow: the target is very often *not* the calling task — the
/// syscall names an arbitrary id — so an exclusive one would have been a claim
/// about a task that is concurrently running. The fate trio is atomic for
/// exactly that reason.
fn with_task<F, R>(task_id: u32, f: F) -> c_int
where
    F: FnOnce(&Task) -> R,
{
    let Some(task) = task_find_by_id(task_id) else {
        return -1;
    };
    f(&task);
    0
}
pub fn fate_spin() -> FateResult {
    let val = platform::rng_next() as u32;
    FateResult {
        token: val,
        value: val,
    }
}
pub fn fate_set_pending(res: FateResult, task_id: u32) -> c_int {
    with_task(task_id, |t| t.set_fate(res.token, res.value))
}
/// Take `task_id`'s pending fate, if it has one and the task still exists.
pub fn fate_take_pending(task_id: u32) -> Option<FateResult> {
    let mut taken = None;
    let _ = with_task(task_id, |t| {
        taken = t
            .take_fate()
            .map(|(token, value)| FateResult { token, value });
    });
    taken
}
pub fn fate_apply_outcome(res: *const FateResult, _resolution: u32, award: bool) {
    if res.is_null() {
        return;
    }
    if award {
        wl_currency::adjust_balance(WL_DELTA);
    } else {
        wl_currency::adjust_balance(-WL_DELTA);
    }
}
