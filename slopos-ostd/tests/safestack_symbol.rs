//! Host-side tests for `slopos_ostd::task::abi` and the
//! `__safestack_pointer_address` naked-fn home in OSTD.
//!
//! The naked fn reads `gs:[...]` so it cannot run under `cargo test`, and the
//! `TaskAbi` layout contract is pinned by `const _` asserts beside the type;
//! what is left to cover here is the behavioural surface.

use std::sync::Mutex;

use slopos_ostd::arch::x86_64::safestack::{install_ap_trampoline, install_safestack_runtime};
use slopos_ostd::sync::{reset_bsp_token_for_tests, run_bsp_init};
use slopos_ostd::task::abi::TaskAbi;

/// The one-shot BSP-token mint guard is process-global.
static BSP_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn install_ap_trampoline_returns_non_null_fn_pointer() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_bsp_token_for_tests();
    let fp = run_bsp_init(|tok| install_ap_trampoline(tok));
    let addr = fp as usize;
    assert_ne!(addr, 0, "install_ap_trampoline must return a real fn");
}

#[test]
fn install_safestack_runtime_accepts_bsp_token() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_bsp_token_for_tests();
    // A no-op today; the test pins the signature to the BSP-init protocol.
    run_bsp_init(|tok| install_safestack_runtime(tok));
}

#[test]
fn task_abi_unsafe_stack_sp_is_writeable_round_trip() {
    // Pins the field as a plain u64: no #[repr(packed)] surprise forcing
    // unaligned read/write paths.
    let mut abi = TaskAbi { unsafe_stack_sp: 0 };
    abi.unsafe_stack_sp = 0xDEAD_BEEF_CAFE_F00D;
    assert_eq!(abi.unsafe_stack_sp, 0xDEAD_BEEF_CAFE_F00D);
}
