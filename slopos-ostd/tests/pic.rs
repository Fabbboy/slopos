//! Host-side coverage for the internal legacy 8259 lifecycle.

#[test]
fn legacy_8259_init_disable_is_idempotent() {
    slopos_ostd::sync::run_bsp_init_for_test(|token| {
        slopos_ostd::io::init_and_disable_legacy_8259(token);
        slopos_ostd::io::init_and_disable_legacy_8259(token);
    });
}
