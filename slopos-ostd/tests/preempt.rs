//! Host-side integration tests for `slopos_ostd::cpu::preempt`.
//!
//! Exercises the [`PreemptBackend`] one-shot registration path, the
//! count-tracking surface, and [`DisabledPreemptGuard`] +
//! [`IrqEntryGuard`]'s interaction with the active backend.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering as StdOrd};
use std::sync::{Mutex, MutexGuard, OnceLock};

use slopos_ostd::cpu::preempt::{
    self, DisabledPreemptGuard, PreemptBackend, default_backend, is_preempt_disabled,
    preempt_count, register_preempt_backend, reset_for_test,
};
use slopos_ostd::irq::idt::{IrqEntryGuard, IstPreemptHold};

static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

fn serial() -> MutexGuard<'static, ()> {
    let m = SERIAL.get_or_init(|| Mutex::new(()));
    let g = m.lock().unwrap_or_else(|p| p.into_inner());
    reset_for_test();
    g
}

#[test]
fn default_noop_backend_starts_at_zero() {
    let _g = serial();
    assert_eq!(preempt_count(), 0);
    assert!(!is_preempt_disabled());
}

#[test]
fn disabled_preempt_guard_uses_default_backend() {
    let _g = serial();
    assert_eq!(default_backend().count(), 0);
    let _h = DisabledPreemptGuard::new();
    assert_eq!(default_backend().count(), 1);
    assert!(is_preempt_disabled());
}

#[test]
fn nested_guards_track_count() {
    let _g = serial();
    let a = DisabledPreemptGuard::new();
    let b = DisabledPreemptGuard::new();
    let c = DisabledPreemptGuard::new();
    assert_eq!(preempt_count(), 3);
    drop(b);
    assert_eq!(preempt_count(), 2);
    drop(a);
    drop(c);
    assert_eq!(preempt_count(), 0);
}

// ---------------------------------------------------------------------------
// Recording backend that counts each entrypoint separately.
// ---------------------------------------------------------------------------

struct RecordingBackend {
    enter: AtomicUsize,
    leave: AtomicUsize,
    leave_quiet: AtomicUsize,
    count: AtomicU32,
}

impl RecordingBackend {
    const fn new() -> Self {
        Self {
            enter: AtomicUsize::new(0),
            leave: AtomicUsize::new(0),
            leave_quiet: AtomicUsize::new(0),
            count: AtomicU32::new(0),
        }
    }
    fn reset(&self) {
        self.enter.store(0, StdOrd::Release);
        self.leave.store(0, StdOrd::Release);
        self.leave_quiet.store(0, StdOrd::Release);
        self.count.store(0, StdOrd::Release);
    }
}

impl PreemptBackend for RecordingBackend {
    fn enter(&self) {
        self.enter.fetch_add(1, StdOrd::Relaxed);
        self.count.fetch_add(1, StdOrd::Relaxed);
    }
    fn leave(&self) {
        self.leave.fetch_add(1, StdOrd::Relaxed);
        self.count.fetch_sub(1, StdOrd::Relaxed);
    }
    fn leave_quiet(&self) {
        self.leave_quiet.fetch_add(1, StdOrd::Relaxed);
        self.count.fetch_sub(1, StdOrd::Relaxed);
    }
    fn count(&self) -> u32 {
        self.count.load(StdOrd::Relaxed)
    }
}

static RECORDING: RecordingBackend = RecordingBackend::new();

fn install_recording() {
    RECORDING.reset();
    // `reset_for_test` cleared any prior installation; the swap inside
    // `register_preempt_backend` asserts !was_installed.
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_preempt_backend(t, &RECORDING);
    });
}

#[test]
fn registered_backend_redirects_disabled_guard() {
    let _g = serial();
    install_recording();
    let _h = DisabledPreemptGuard::new();
    assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 1);
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 0);
    drop(_h);
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 1);
    // DisabledPreemptGuard uses `leave`, NOT `leave_quiet`.
    assert_eq!(RECORDING.leave_quiet.load(StdOrd::Relaxed), 0);
}

#[test]
fn irq_entry_guard_uses_leave_quiet_for_ist_vector() {
    let _g = serial();
    install_recording();
    {
        let _h = IrqEntryGuard::<14>::enter(); // page fault, IST vector
        assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 1);
    }
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 0);
    assert_eq!(RECORDING.leave_quiet.load(StdOrd::Relaxed), 1);
}

#[test]
fn irq_entry_guard_non_ist_vector_does_not_touch_backend() {
    let _g = serial();
    install_recording();
    {
        let _h = IrqEntryGuard::<32>::enter(); // hardware IRQ — not IST
    }
    assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 0);
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 0);
    assert_eq!(RECORDING.leave_quiet.load(StdOrd::Relaxed), 0);
}

#[test]
fn ist_preempt_hold_active_uses_leave_quiet() {
    let _g = serial();
    install_recording();
    {
        let _h = IstPreemptHold::new(true);
        assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 1);
    }
    assert_eq!(RECORDING.leave_quiet.load(StdOrd::Relaxed), 1);
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 0);
}

#[test]
fn ist_preempt_hold_inactive_is_noop() {
    let _g = serial();
    install_recording();
    {
        let _h = IstPreemptHold::new(false);
    }
    assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 0);
    assert_eq!(RECORDING.leave.load(StdOrd::Relaxed), 0);
    assert_eq!(RECORDING.leave_quiet.load(StdOrd::Relaxed), 0);
}

#[test]
fn double_register_preempt_backend_panics() {
    let _g = serial();
    install_recording();
    let result = std::panic::catch_unwind(|| {
        // Intentionally trigger the double-registration panic.
        slopos_ostd::sync::run_bsp_init_for_test(|t| {
            register_preempt_backend(t, &RECORDING);
        });
    });
    assert!(result.is_err(), "second register should panic");
}

// Exercises a fresh `reset_for_test` cycle so the next test starts
// clean — leaves no global state behind by relying on the `serial`
// reset at the *start* of each test. The explicit name ensures the
// helper is exercised in CI even if all real assertions are tail-only.
#[test]
fn reset_for_test_unregisters_backend() {
    let _g = serial();
    install_recording();
    let h = DisabledPreemptGuard::new();
    assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 1);
    drop(h);
    preempt::reset_for_test();
    // After reset, the no-op default backend takes over again.
    let h = DisabledPreemptGuard::new();
    assert_eq!(preempt_count(), 1);
    // Recording backend should not have been called again.
    assert_eq!(RECORDING.enter.load(StdOrd::Relaxed), 1);
    drop(h);
}
