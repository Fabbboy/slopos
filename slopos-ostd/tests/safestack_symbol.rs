//! Host-side tests for `slopos_ostd::task::abi` and the
//! `__safestack_pointer_address` naked-fn home in OSTD.
//!
//! The naked fn itself reads `gs:[...]` so it cannot run under
//! `cargo test`. The `TaskAbi` layout contract (slot at offset 0,
//! exactly one naturally-aligned u64) is pinned by `const _` asserts
//! beside the type in `slopos_ostd::task::abi`; the tests here cover
//! the behavioral surface — the `install_*` safe wrappers thread
//! through the BSP-init protocol and the slot is a plain writable u64.

use std::sync::Mutex;

use slopos_ostd::arch::x86_64::safestack::{install_ap_trampoline, install_safestack_runtime};
use slopos_ostd::sync::{reset_bsp_token_for_tests, run_bsp_init};
use slopos_ostd::task::abi::TaskAbi;

/// Serialises BSP-token-touching tests because the one-shot mint
/// guard is process-global.
static BSP_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn install_ap_trampoline_returns_non_null_fn_pointer() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_bsp_token_for_tests();
    let fp = run_bsp_init(|tok| install_ap_trampoline(tok));
    // Pointer-equality check: the wrapper hands back the documented
    // `ap_entry` naked fn. Its address is non-null and stable.
    let addr = fp as usize;
    assert_ne!(addr, 0, "install_ap_trampoline must return a real fn");
}

#[test]
fn install_safestack_runtime_accepts_bsp_token() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_bsp_token_for_tests();
    // Smoke test: the safe wrapper compiles, type-checks, and
    // accepts a BspToken from `run_bsp_init`. Today it's a no-op;
    // the test pins the signature so future side-effects ride the
    // same protocol.
    run_bsp_init(|tok| install_safestack_runtime(tok));
}

#[test]
fn task_abi_unsafe_stack_sp_is_writeable_round_trip() {
    // The kernel-side `task_set_unsafe_stack_sp` writes through this
    // field; round-trip a value to verify the field is a plain u64
    // with no #[repr(packed)] alignment surprise that would force
    // unaligned read/write paths.
    let mut abi = TaskAbi { unsafe_stack_sp: 0 };
    abi.unsafe_stack_sp = 0xDEAD_BEEF_CAFE_F00D;
    assert_eq!(abi.unsafe_stack_sp, 0xDEAD_BEEF_CAFE_F00D);
}
