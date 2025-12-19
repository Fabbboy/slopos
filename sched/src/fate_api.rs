use crate::task::{task_find_by_id, Task};
use core::ffi::c_int;
use slopos_drivers::{fate::FateResult, random, wl_currency};

fn with_task<F, R>(task_id: u32, f: F) -> c_int
where
    F: FnOnce(&mut Task) -> R,
{
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }
    unsafe {
        f(&mut *task);
    }
    0
}
pub fn fate_spin() -> FateResult {
    let val = random::random_next() as u32;
    FateResult {
        token: val,
        value: val,
    }
}
pub fn fate_set_pending(res: FateResult, task_id: u32) -> c_int {
    with_task(task_id, |t| {
        t.fate_token = res.token;
        t.fate_value = res.value;
        t.fate_pending = 1;
    })
}
pub fn fate_take_pending(task_id: u32, out: *mut FateResult) -> c_int {
    let mut result = -1;
    let _ = with_task(task_id, |t| {
        if t.fate_pending != 0 {
            if !out.is_null() {
                unsafe {
                    *out = FateResult {
                        token: t.fate_token,
                        value: t.fate_value,
                    };
                }
            }
            t.fate_pending = 0;
            result = 0;
        }
    });
    result
}
pub fn fate_apply_outcome(res: *const FateResult, _resolution: u32, award: bool) {
    if res.is_null() {
        return;
    }
    if award {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
    }
}
