use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;

use slopos_ostd::cpu::x86_64::interrupts;
use slopos_ostd::sync::lock_graph::{
    LOCK_LEVEL_RESOURCE, enable_lock_tracking, pop_lock, push_lock, reset_for_test,
};
use slopos_ostd::task::drop_off_lock;
use slopos_ostd::task::kernel_task::TaskInner;

static CONTEXT_TEST_LOCK: Mutex<()> = Mutex::new(());

unsafe fn noop_poison(_addr: *const ()) {}

#[test]
fn deferred_drop_runs_with_interrupts_enabled() {
    struct IrqProbe<'a>(&'a mut bool);

    impl Drop for IrqProbe<'_> {
        fn drop(&mut self) {
            *self.0 = interrupts::are_interrupts_enabled();
        }
    }

    let _serial = CONTEXT_TEST_LOCK.lock().unwrap();
    let mut observed_enabled = false;
    drop_off_lock(IrqProbe(&mut observed_enabled));
    assert!(observed_enabled);
}

#[test]
fn task_drop_rejects_interrupts_disabled_context() {
    let _serial = CONTEXT_TEST_LOCK.lock().unwrap();
    let task = TaskInner::<(), ()>::invalid();

    interrupts::disable_interrupts();
    let result = panic::catch_unwind(AssertUnwindSafe(|| drop(task)));
    interrupts::enable_interrupts();

    assert!(result.is_err());
}

#[test]
fn task_drop_rejects_held_lock_context() {
    let _serial = CONTEXT_TEST_LOCK.lock().unwrap();
    reset_for_test();
    enable_lock_tracking();
    let task = TaskInner::<(), ()>::invalid();
    let lock_addr = core::ptr::without_provenance::<()>(0xD04D_C07E);

    unsafe {
        push_lock(lock_addr, noop_poison, LOCK_LEVEL_RESOURCE);
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| drop(task)));
    unsafe {
        pop_lock(lock_addr);
    }

    assert!(result.is_err());
}
