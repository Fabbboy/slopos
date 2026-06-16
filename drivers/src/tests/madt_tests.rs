//! Regression tests for ACPI MADT flag parsing used by interrupt setup.

use slopos_acpi::madt::{Madt, MadtEntry};
use slopos_acpi::tables::AcpiTable;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;

const MADT_NO_PCAT: [u8; 44] = [
    b'A', b'P', b'I', b'C', 44, 0, 0, 0, 0, 0xB7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const MADT_PCAT: [u8; 44] = [
    b'A', b'P', b'I', b'C', 44, 0, 0, 0, 0, 0xB6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0,
];

const MADT_WITH_IOAPIC: [u8; 56] = [
    b'A', b'P', b'I', b'C', 56, 0, 0, 0, 0, 0xD9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 12, 7, 0, 0, 0, 0xC0, 0xFE, 0, 0, 0,
    0,
];

fn parse(bytes: &'static [u8]) -> Option<Madt> {
    AcpiTable::from_bytes(bytes).and_then(Madt::from_table)
}

pub fn test_madt_pcat_flag_set() -> TestResult {
    let Some(madt) = parse(&MADT_PCAT) else {
        klog_info!("MADT_TEST: failed to parse PCAT MADT fixture");
        return TestResult::Fail;
    };
    if !madt.has_pcat_compat_dual_8259() {
        klog_info!("MADT_TEST: PCAT_COMPAT flag was not detected");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_madt_pcat_flag_clear() -> TestResult {
    let Some(madt) = parse(&MADT_NO_PCAT) else {
        klog_info!("MADT_TEST: failed to parse no-PCAT MADT fixture");
        return TestResult::Fail;
    };
    if madt.has_pcat_compat_dual_8259() {
        klog_info!("MADT_TEST: PCAT_COMPAT flag falsely detected");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_madt_entries_start_after_flags() -> TestResult {
    let Some(madt) = parse(&MADT_WITH_IOAPIC) else {
        klog_info!("MADT_TEST: failed to parse IOAPIC MADT fixture");
        return TestResult::Fail;
    };
    let Some(MadtEntry::Ioapic(info)) = madt.entries().next() else {
        klog_info!("MADT_TEST: first MADT entry was not parsed as IOAPIC");
        return TestResult::Fail;
    };
    if info.id != 7 || info.address != 0xFEC0_0000 || info.gsi_base != 0 {
        klog_info!("MADT_TEST: IOAPIC entry fields parsed incorrectly");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_madt_pcat_flag_set, suite = madt);
slopos_testing::stest!(name = test_madt_pcat_flag_clear, suite = madt);
slopos_testing::stest!(name = test_madt_entries_start_after_flags, suite = madt);
