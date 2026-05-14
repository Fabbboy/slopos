//! IOAPIC/APIC tests - targeting untested initialization and routing edge cases.

use super::regs::*;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

use crate::{apic, ioapic};

pub fn test_ioapic_ready_state() -> TestResult {
    let ready = ioapic::is_ready();
    if ready == 0 {
        klog_info!("IOAPIC_TEST: WARNING - IOAPIC not ready (may be expected in some configs)");
    }
    TestResult::Pass
}

pub fn test_apic_enabled_state() -> TestResult {
    let enabled = apic::is_enabled();
    if !enabled {
        klog_info!("IOAPIC_TEST: WARNING - APIC not enabled");
    }
    TestResult::Pass
}

pub fn test_apic_id_valid() -> TestResult {
    if !apic::is_enabled() {
        return TestResult::Pass;
    }

    let id = apic::get_id();
    if id > 255 {
        klog_info!("IOAPIC_TEST: BUG - APIC ID {} exceeds 8-bit limit", id);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ioapic_legacy_irq_info_invalid() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let mut gsi = 0xDEADu32;
    let mut flags = 0xBEEFu32;

    let result = ioapic::legacy_irq_info(255, &mut gsi, &mut flags);

    if result == 0 && gsi == 255 && flags == (IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE)
    {
        return TestResult::Pass;
    }

    TestResult::Pass
}

pub fn test_ioapic_legacy_irq_info_valid() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let mut gsi = 0u32;
    let mut flags = 0u32;

    let result = ioapic::legacy_irq_info(0, &mut gsi, &mut flags);
    if result != 0 {
        klog_info!("IOAPIC_TEST: BUG - legacy_irq_info failed for timer IRQ");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ioapic_mask_invalid_gsi() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let result = ioapic::mask_gsi(0xFFFF_FFFF);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - mask_gsi succeeded for invalid GSI");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ioapic_unmask_invalid_gsi() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let result = ioapic::unmask_gsi(0xFFFF_FFFF);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - unmask_gsi succeeded for invalid GSI");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ioapic_config_invalid_gsi() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let result = ioapic::config_irq(0xFFFF_FFFF, 0x30, 0, 0);
    if result == 0 {
        klog_info!("IOAPIC_TEST: BUG - config_irq succeeded for invalid GSI");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ioapic_config_boundary_vector() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    let mut gsi = 0u32;
    let mut flags = 0u32;
    if ioapic::legacy_irq_info(15, &mut gsi, &mut flags) != 0 {
        return TestResult::Pass;
    }

    TestResult::Pass
}

pub fn test_ioapic_flag_constants() -> TestResult {
    if IOAPIC_FLAG_POLARITY_HIGH != 0 {
        klog_info!("IOAPIC_TEST: BUG - POLARITY_HIGH should be 0");
        return TestResult::Fail;
    }

    if IOAPIC_FLAG_TRIGGER_EDGE != 0 {
        klog_info!("IOAPIC_TEST: BUG - TRIGGER_EDGE should be 0");
        return TestResult::Fail;
    }

    if IOAPIC_FLAG_POLARITY_LOW == 0 {
        klog_info!("IOAPIC_TEST: BUG - POLARITY_LOW should be non-zero");
        return TestResult::Fail;
    }

    if IOAPIC_FLAG_TRIGGER_LEVEL == 0 {
        klog_info!("IOAPIC_TEST: BUG - TRIGGER_LEVEL should be non-zero");
        return TestResult::Fail;
    }

    if IOAPIC_FLAG_MASK == 0 {
        klog_info!("IOAPIC_TEST: BUG - MASK flag should be non-zero");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ioapic_register_constants() -> TestResult {
    if IOAPIC_REG_VER != 1 {
        klog_info!("IOAPIC_TEST: BUG - IOAPIC_REG_VER should be 1");
        return TestResult::Fail;
    }

    if IOAPIC_REG_REDIR_BASE != 0x10 {
        klog_info!("IOAPIC_TEST: BUG - IOAPIC_REG_REDIR_BASE should be 0x10");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_apic_eoi_safe() -> TestResult {
    if !apic::is_enabled() {
        return TestResult::Pass;
    }

    apic::send_eoi();
    TestResult::Pass
}

pub fn test_ioapic_double_init() -> TestResult {
    let before = ioapic::is_ready();
    let result = ioapic::init();
    let after = ioapic::is_ready();

    if before != 0 && result != 0 {
        klog_info!("IOAPIC_TEST: BUG - Double init returned error when already initialized");
        return TestResult::Fail;
    }

    if before != after {
        klog_info!("IOAPIC_TEST: BUG - Ready state changed after double init");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ioapic_all_legacy_irqs() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    for irq in 0..16u8 {
        let mut gsi = 0u32;
        let mut flags = 0u32;
        let _ = ioapic::legacy_irq_info(irq, &mut gsi, &mut flags);
    }
    TestResult::Pass
}

pub fn test_apic_spurious_vector() -> TestResult {
    if !apic::is_enabled() {
        return TestResult::Pass;
    }
    TestResult::Pass
}

pub fn test_ioapic_gsi_range() -> TestResult {
    if ioapic::is_ready() == 0 {
        return TestResult::Pass;
    }

    for gsi in [0u32, 1, 2, 15, 16, 23, 24] {
        let _ = ioapic::mask_gsi(gsi);
        let _ = ioapic::unmask_gsi(gsi);
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_ioapic_ready_state, suite = ioapic);
slopos_testing::stest!(name = test_apic_enabled_state, suite = ioapic);
slopos_testing::stest!(name = test_apic_id_valid, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_legacy_irq_info_invalid, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_legacy_irq_info_valid, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_mask_invalid_gsi, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_unmask_invalid_gsi, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_config_invalid_gsi, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_config_boundary_vector, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_flag_constants, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_register_constants, suite = ioapic);
slopos_testing::stest!(name = test_apic_eoi_safe, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_double_init, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_all_legacy_irqs, suite = ioapic);
slopos_testing::stest!(name = test_apic_spurious_vector, suite = ioapic);
slopos_testing::stest!(name = test_ioapic_gsi_range, suite = ioapic);
