use core::ffi::c_int;

use slopos_drivers::{random, wl_currency};

use crate::task::{task_find_by_id, Task};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct fate_result {
    pub token: u32,
    pub value: u32,
}

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

#[no_mangle]
pub extern "C" fn fate_spin() -> fate_result {
    let val = random::random_next() as u32;
    fate_result {
        token: val,
        value: val,
    }
}

#[no_mangle]
pub extern "C" fn fate_set_pending(res: fate_result, task_id: u32) -> c_int {
    with_task(task_id, |t| {
        t.fate_token = res.token;
        t.fate_value = res.value;
        t.fate_pending = 1;
    })
}

#[no_mangle]
pub extern "C" fn fate_take_pending(task_id: u32, out: *mut fate_result) -> c_int {
    let mut result = -1;
    let _ = with_task(task_id, |t| {
        if t.fate_pending != 0 {
            if !out.is_null() {
                unsafe {
                    *out = fate_result {
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

#[no_mangle]
pub extern "C" fn fate_apply_outcome(res: *const fate_result, _resolution: u32, award: bool) {
    if res.is_null() {
        return;
    }
    if award {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
    }
}
