//! Host-side integration tests for `slopos_ostd::sync::kernel_sync`.

use core::cell::{Cell, RefCell, UnsafeCell};
use std::sync::{Arc, Mutex};
use std::thread;

use slopos_ostd::sync::{BspToken, KernelSync, reset_bsp_token_for_tests, run_bsp_init};

/// Serialises every test touching the process-global `BSP_TOKEN_MINTED` flag,
/// which `cargo test`'s default parallelism would otherwise race.
static BSP_LOCK: Mutex<()> = Mutex::new(());

// Miri flags the RefCell borrow counter's unsynchronized non-atomic store as
// UB; in the kernel `KernelSync` presumes external invariants (IRQs disabled,
// exclusive CPU access) that the host test cannot model.
#[cfg_attr(miri, ignore)]
#[test]
fn refcell_u64_round_trips_across_threads() {
    let shared = Arc::new(KernelSync::new(RefCell::new(0xDEAD_BEEF_u64)));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let s = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            let r = s.get().borrow();
            assert_eq!(*r, 0xDEAD_BEEF_u64);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(*shared.get().borrow(), 0xDEAD_BEEF_u64);
}

// Models the dominant consumer shape: a `KernelSync<*mut T>` field.
#[test]
fn raw_pointer_round_trips_across_threads() {
    let mut backing: Vec<u32> = vec![1, 2, 3, 4];
    let ptr: *mut u32 = backing.as_mut_ptr();
    let shared = Arc::new(KernelSync::new(ptr));
    let mut handles = Vec::new();
    for offset in 0..4 {
        let s = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            // SAFETY: backing is alive for the duration of every spawned
            // thread because the main thread `join`s them before
            // dropping `backing`. Each thread reads a distinct index,
            // so there is no data race.
            let value = unsafe { *s.get().offset(offset) };
            assert_eq!(value, (offset as u32) + 1);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    drop(shared);
    drop(backing);
}

// Models the bootstrap-task pattern: `KernelSync<UnsafeCell<T>>` shared
// read-only across all CPUs.
#[test]
fn unsafe_cell_round_trips_across_threads() {
    let shared = Arc::new(KernelSync::new(UnsafeCell::new(99_u64)));
    let mut handles = Vec::new();
    for _ in 0..3 {
        let s = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            // SAFETY: read-only access, no concurrent writer.
            let value = unsafe { *s.get().get() };
            assert_eq!(value, 99);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: writers joined; we are the sole accessor again.
    assert_eq!(unsafe { *shared.get().get() }, 99);
}

#[test]
fn cell_round_trips_across_threads() {
    let shared = Arc::new(KernelSync::new(Cell::new(7_u64)));
    let s2 = Arc::clone(&shared);
    let h = thread::spawn(move || {
        let value = s2.get().get();
        assert_eq!(value, 7);
    });
    h.join().unwrap();
    assert_eq!(shared.get().get(), 7);
}

#[test]
fn clone_and_into_inner_preserve_value() {
    let original = 0xCAFE_F00D_u64;
    let k = KernelSync::new(original);
    let k2 = k.clone();
    assert_eq!(k.into_inner(), original);
    assert_eq!(k2.into_inner(), original);
}

#[test]
fn default_constructs_inner_default() {
    let k: KernelSync<u64> = KernelSync::default();
    assert_eq!(*k.get(), 0);
    assert_eq!(k.into_inner(), 0);
}

#[test]
fn get_mut_yields_exclusive_borrow() {
    let mut k = KernelSync::new(0_u64);
    *k.get_mut() = 123;
    assert_eq!(*k.get(), 123);
}

#[test]
fn deref_and_deref_mut_round_trip() {
    let mut k = KernelSync::new(0_u64);
    *k = 17;
    assert_eq!(*k, 17);
}

#[test]
fn kernel_sync_debug_format_round_trips_inner() {
    let k = KernelSync::new(7_u64);
    let s = format!("{:?}", k);
    assert!(
        s.contains('7'),
        "Debug output `{}` should embed inner value",
        s
    );
}

#[test]
fn send_sync_flags_present_for_not_sync_inner() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<KernelSync<RefCell<u64>>>();
    assert_sync::<KernelSync<RefCell<u64>>>();
    assert_send::<KernelSync<*mut u32>>();
    assert_sync::<KernelSync<*mut u32>>();
    assert_send::<KernelSync<UnsafeCell<u64>>>();
    assert_sync::<KernelSync<UnsafeCell<u64>>>();
}

#[test]
fn bsp_token_passes_inside_callback() {
    let _guard = BSP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_bsp_token_for_tests();

    let mut call_count = 0_usize;
    let token_size = run_bsp_init(|token: &BspToken| {
        call_count += 1;
        core::mem::size_of_val(token)
    });
    assert_eq!(call_count, 1);
    assert_eq!(token_size, 0);
}

#[test]
fn bsp_token_reset_allows_remint() {
    let _guard = BSP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_bsp_token_for_tests();
    run_bsp_init(|_| {});

    reset_bsp_token_for_tests();
    run_bsp_init(|_| {});
}

#[test]
#[should_panic(expected = "run_bsp_init: BSP token already minted")]
fn bsp_token_single_shot_panics_on_double_call() {
    let _guard = BSP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_bsp_token_for_tests();
    run_bsp_init(|_| {});
    run_bsp_init(|_| {});
}

#[test]
fn bsp_token_is_zero_sized() {
    let _guard = BSP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_bsp_token_for_tests();
    run_bsp_init(|token: &BspToken| {
        assert_eq!(core::mem::size_of_val(token), 0);
    });
}
