use core::ffi::c_int;

// Keep extern "C" for drivers functions to break circular dependency
unsafe extern "C" {
    fn random_u64() -> u64;
    fn wl_award_win();
    fn wl_award_loss();
}

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

#[unsafe(no_mangle)]
pub fn fate_spin() -> fate_result {
    let val = unsafe { random_u64() } as u32;
    fate_result {
        token: val,
        value: val,
    }
}

#[unsafe(no_mangle)]
pub fn fate_set_pending(res: fate_result, task_id: u32) -> c_int {
    with_task(task_id, |t| {
        t.fate_token = res.token;
        t.fate_value = res.value;
        t.fate_pending = 1;
    })
}

#[unsafe(no_mangle)]
pub fn fate_take_pending(task_id: u32, out: *mut fate_result) -> c_int {
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

#[unsafe(no_mangle)]
pub fn fate_apply_outcome(res: *const fate_result, _resolution: u32, award: bool) {
    if res.is_null() {
        return;
    }
    if award {
        unsafe { wl_award_win() };
    } else {
        unsafe { wl_award_loss() };
    }
}
