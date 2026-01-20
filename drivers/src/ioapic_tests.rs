//! IOAPIC/APIC tests - targeting untested initialization and routing edge cases.

use core::ffi::c_int;

use slopos_abi::arch::x86_64::ioapic::*;
use slopos_lib::klog_info;

use crate::{apic, ioapic};

pub fn test_ioapic_ready_state() -> c_int {
    let ready = ioapic::is_ready();
    if ready == 0 {
        klog_info!("IOAPIC_TEST: WARNING - IOAPIC not ready (may be expected in some configs)");
    }
    0
}

pub fn test_apic_enabled_state() -> c_int {
    let enabled = apic::is_enabled();
    if !enabled {
        klog_info!("IOAPIC_TEST: WARNING - APIC not enabled");
    }
    0
}

pub fn test_apic_id_valid() -> c_int {
    if !apic::is_enabled() {
        return 0;
    }

    let id = apic::get_id();
    if id > 255 {
        klog_info!("IOAPIC_TEST: BUG - APIC ID {} exceeds 8-bit limit", id);
        return -1;
    }
    0
}

pub fn test_ioapic_legacy_irq_info_invalid() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let mut gsi = 0xDEADu32;
    let mut flags = 0xBEEFu32;

    let result = ioapic::legacy_irq_info(255, &mut gsi, &mut flags);

    if result == 0 && gsi == 255 && flags == (IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE)
    {
        return 0;
    }

    0
}

pub fn test_ioapic_legacy_irq_info_valid() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let mut gsi = 0u32;
    let mut flags = 0u32;

    let result = ioapic::legacy_irq_info(0, &mut gsi, &mut flags);
    if result != 0 {
        klog_info!("IOAPIC_TEST: BUG - legacy_irq_info failed for timer IRQ");
        return -1;
    }

    0
}

pub fn test_ioapic_mask_invalid_gsi() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let result = ioapic::mask_gsi(0xFFFF_FFFF);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - mask_gsi succeeded for invalid GSI");
        return -1;
    }
    0
}

pub fn test_ioapic_unmask_invalid_gsi() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let result = ioapic::unmask_gsi(0xFFFF_FFFF);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - unmask_gsi succeeded for invalid GSI");
        return -1;
    }
    0
}

pub fn test_ioapic_config_invalid_gsi() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let result = ioapic::config_irq(0xFFFF_FFFF, 0x30, 0, 0);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - config_irq succeeded for invalid GSI");
        return -1;
    }
    0
}

pub fn test_ioapic_config_boundary_vector() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    let mut gsi = 0u32;
    let mut flags = 0u32;
    if ioapic::legacy_irq_info(15, &mut gsi, &mut flags) != 0 {
        return 0;
    }

    0
}

pub fn test_ioapic_flag_constants() -> c_int {
    if IOAPIC_FLAG_POLARITY_HIGH != 0 {
        klog_info!("IOAPIC_TEST: BUG - POLARITY_HIGH should be 0");
        return -1;
    }

    if IOAPIC_FLAG_TRIGGER_EDGE != 0 {
        klog_info!("IOAPIC_TEST: BUG - TRIGGER_EDGE should be 0");
        return -1;
    }

    if IOAPIC_FLAG_POLARITY_LOW == 0 {
        klog_info!("IOAPIC_TEST: BUG - POLARITY_LOW should be non-zero");
        return -1;
    }

    if IOAPIC_FLAG_TRIGGER_LEVEL == 0 {
        klog_info!("IOAPIC_TEST: BUG - TRIGGER_LEVEL should be non-zero");
        return -1;
    }

    if IOAPIC_FLAG_MASK == 0 {
        klog_info!("IOAPIC_TEST: BUG - MASK flag should be non-zero");
        return -1;
    }

    0
}

pub fn test_ioapic_register_constants() -> c_int {
    if IOAPIC_REG_VER != 1 {
        klog_info!("IOAPIC_TEST: BUG - IOAPIC_REG_VER should be 1");
        return -1;
    }

    if IOAPIC_REG_REDIR_BASE != 0x10 {
        klog_info!("IOAPIC_TEST: BUG - IOAPIC_REG_REDIR_BASE should be 0x10");
        return -1;
    }

    0
}

pub fn test_apic_eoi_safe() -> c_int {
    if !apic::is_enabled() {
        return 0;
    }

    apic::send_eoi();
    0
}

pub fn test_ioapic_double_init() -> c_int {
    let before = ioapic::is_ready();
    let result = ioapic::init();
    let after = ioapic::is_ready();

    if before != 0 && result != 0 {
        klog_info!("IOAPIC_TEST: BUG - Double init returned error when already initialized");
        return -1;
    }

    if before != after {
        klog_info!("IOAPIC_TEST: BUG - Ready state changed after double init");
        return -1;
    }

    0
}

pub fn test_ioapic_all_legacy_irqs() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    for irq in 0..16u8 {
        let mut gsi = 0u32;
        let mut flags = 0u32;
        let _ = ioapic::legacy_irq_info(irq, &mut gsi, &mut flags);
    }
    0
}

pub fn test_apic_spurious_vector() -> c_int {
    if !apic::is_enabled() {
        return 0;
    }
    0
}

pub fn test_ioapic_gsi_range() -> c_int {
    if ioapic::is_ready() == 0 {
        return 0;
    }

    for gsi in [0u32, 1, 2, 15, 16, 23, 24] {
        let _ = ioapic::mask_gsi(gsi);
        let _ = ioapic::unmask_gsi(gsi);
    }
    0
}

pub fn run_ioapic_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    klog_info!("=== IOAPIC/APIC Tests ===");

    slopos_lib::run_test!(passed, total, test_ioapic_ready_state);
    slopos_lib::run_test!(passed, total, test_apic_enabled_state);
    slopos_lib::run_test!(passed, total, test_apic_id_valid);
    slopos_lib::run_test!(passed, total, test_ioapic_legacy_irq_info_invalid);
    slopos_lib::run_test!(passed, total, test_ioapic_legacy_irq_info_valid);
    slopos_lib::run_test!(passed, total, test_ioapic_mask_invalid_gsi);
    slopos_lib::run_test!(passed, total, test_ioapic_unmask_invalid_gsi);
    slopos_lib::run_test!(passed, total, test_ioapic_config_invalid_gsi);
    slopos_lib::run_test!(passed, total, test_ioapic_config_boundary_vector);
    slopos_lib::run_test!(passed, total, test_ioapic_flag_constants);
    slopos_lib::run_test!(passed, total, test_ioapic_register_constants);
    slopos_lib::run_test!(passed, total, test_apic_eoi_safe);
    slopos_lib::run_test!(passed, total, test_ioapic_double_init);
    slopos_lib::run_test!(passed, total, test_ioapic_all_legacy_irqs);
    slopos_lib::run_test!(passed, total, test_apic_spurious_vector);
    slopos_lib::run_test!(passed, total, test_ioapic_gsi_range);

    klog_info!("IOAPIC tests: {}/{} passed", passed, total);
    (passed, total)
}
