//! Host-side integration tests for `slopos_ostd::irq::line`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use slopos_ostd::irq::line::{
    self, ALLOC_VECTOR_BASE, ALLOC_VECTOR_END, IrqAllocator, IrqContext, IrqError, dispatch,
    register_irq_reserved, reset_for_test, shutdown,
};

static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

fn serial() -> MutexGuard<'static, ()> {
    let m = SERIAL.get_or_init(|| Mutex::new(()));
    let g = m.lock().unwrap_or_else(|p| p.into_inner());
    reset_for_test();
    g
}

#[test]
fn alloc_returns_distinct_vectors_in_range() {
    let _g = serial();
    let a = IrqAllocator::alloc().expect("alloc a");
    let b = IrqAllocator::alloc().expect("alloc b");
    assert_ne!(a.vector(), b.vector());
    for v in [a.vector(), b.vector()] {
        assert!((ALLOC_VECTOR_BASE..ALLOC_VECTOR_END).contains(&v));
    }
}

#[test]
fn reserved_vectors_are_excluded() {
    let _g = serial();
    let reserved: &[u8] = &[ALLOC_VECTOR_BASE, ALLOC_VECTOR_BASE + 5, 0xEC, 0x80];
    slopos_ostd::sync::run_bsp_init_for_test(|t| {
        register_irq_reserved(t, reserved);
    });
    for _ in 0..50 {
        let line = IrqAllocator::alloc().expect("alloc");
        let v = line.vector();
        assert!(!reserved.contains(&v), "alloc returned reserved {}", v);
    }
}

#[test]
fn callback_runs_on_dispatch() {
    let _g = serial();
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    let counter = Arc::new(AtomicUsize::new(0));
    let last_err = Arc::new(AtomicUsize::new(0));

    let c2 = counter.clone();
    let e2 = last_err.clone();
    let _h = line
        .register_callback(move |ctx: &IrqContext<'_>| {
            assert_eq!(ctx.vector(), v);
            c2.fetch_add(1, Ordering::Relaxed);
            e2.store(ctx.error_code() as usize, Ordering::Relaxed);
        })
        .expect("register");

    dispatch(v, 0xAB);
    dispatch(v, 0xCD);
    assert_eq!(counter.load(Ordering::Relaxed), 2);
    assert_eq!(last_err.load(Ordering::Relaxed), 0xCD);
}

#[test]
fn dispatch_on_unregistered_vector_is_noop() {
    let _g = serial();
    dispatch(50, 0);
    dispatch(100, 0xDEADBEEF);
}

#[test]
fn double_register_returns_already_registered() {
    let _g = serial();
    let line = IrqAllocator::alloc().expect("alloc");
    let _h = line.register_callback(|_| {}).expect("first");
    let r = line.register_callback(|_| {});
    assert_eq!(r.err(), Some(IrqError::AlreadyRegistered));
}

#[test]
fn dropping_handle_clears_dispatch_slot() {
    let _g = serial();
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let c2 = counter.clone();
        let _h = line
            .register_callback(move |_| {
                c2.fetch_add(1, Ordering::Relaxed);
            })
            .expect("register");
        dispatch(v, 0);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1, "no dispatch after drop");
}

#[test]
fn dropping_line_returns_vector_to_pool() {
    let _g = serial();
    let v_first = IrqAllocator::alloc().expect("first").vector();
    let mut held = std::vec::Vec::new();
    let mut saw_v_first_again = false;
    loop {
        match IrqAllocator::alloc() {
            Ok(line) => {
                if line.vector() == v_first {
                    saw_v_first_again = true;
                    break;
                }
                held.push(line);
            }
            Err(IrqError::Exhausted) => break,
            Err(other) => panic!("unexpected {:?}", other),
        }
    }
    assert!(saw_v_first_again);
}

#[test]
fn shutdown_suppresses_subsequent_dispatch() {
    let _g = serial();
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let _h = line
        .register_callback(move |_| {
            c2.fetch_add(1, Ordering::Relaxed);
        })
        .expect("register");
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    shutdown();
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}

#[test]
fn handle_borrows_line_and_clears_on_drop() {
    let _g = serial();
    let counter = Arc::new(AtomicUsize::new(0));
    let v;
    {
        let line = IrqAllocator::alloc().expect("alloc");
        v = line.vector();
        let c2 = counter.clone();
        let _h = line
            .register_callback(move |_| {
                c2.fetch_add(1, Ordering::Relaxed);
            })
            .expect("register");
        dispatch(v, 0);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        // _h drops before line; both clear the slot, so line's drop must
        // tolerate an already-cleared one.
    }
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}

#[test]
fn reset_for_test_drains_dispatch() {
    let _g = serial();
    let counter = Arc::new(AtomicUsize::new(0));
    let v = {
        let line = IrqAllocator::alloc().expect("alloc");
        let v = line.vector();
        let c2 = counter.clone();
        // Both are forgotten, so nothing but reset_for_test clears the slot.
        let h = line
            .register_callback(move |_| {
                c2.fetch_add(1, Ordering::Relaxed);
            })
            .expect("register");
        std::mem::forget(h);
        std::mem::forget(line);
        v
    };
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    line::reset_for_test();
    dispatch(v, 0);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}
