//! MCFG / ECAM regression tests.
//!
//! All tests run after PCI enumeration in the test harness.  QEMU q35 exposes
//! a single MCFG entry for segment 0, buses 0–255, at physical address
//! 0xB000_0000 or 0xE000_0000 (depends on QEMU version/config).

use slopos_testing::TestResult;
use slopos_testing::{fail, pass};

use crate::pci::{
    pci_ecam_available, pci_ecam_base, pci_ecam_entry, pci_ecam_entry_count, pci_ecam_find_entry,
    pci_ecam_mapped_region, pci_ecam_primary_virt, pci_ecam_read8, pci_ecam_read16,
    pci_ecam_read32, pci_get_device, pci_get_device_count,
};

pub fn test_ecam_available() -> TestResult {
    if !pci_ecam_available() {
        return fail!("pci_ecam_available() returned false — MCFG discovery failed on q35");
    }
    pass!()
}

pub fn test_ecam_entry_count_nonzero() -> TestResult {
    let count = pci_ecam_entry_count();
    if count == 0 {
        return fail!("pci_ecam_entry_count() is 0 — no MCFG entries cached");
    }
    pass!()
}

pub fn test_ecam_base_nonzero() -> TestResult {
    let base = pci_ecam_base();
    if base == 0 {
        return fail!("pci_ecam_base() returned 0 — no segment 0 ECAM region");
    }
    pass!()
}

pub fn test_ecam_base_page_aligned() -> TestResult {
    let base = pci_ecam_base();
    if base == 0 {
        return fail!("pci_ecam_base() is 0, cannot check alignment");
    }
    if (base & 0xFFF) != 0 {
        return fail!(
            "ECAM base 0x{:x} is not 4K-aligned (bottom 12 bits: 0x{:x})",
            base,
            base & 0xFFF
        );
    }
    pass!()
}

pub fn test_primary_entry_covers_bus_zero() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_start > 0 {
        return fail!(
            "Primary ECAM entry bus_start={}, expected 0",
            entry.bus_start
        );
    }
    pass!()
}

pub fn test_all_entries_base_nonzero() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => return fail!("pci_ecam_entry({}) returned None but count={}", i, count),
        };
        if entry.base_phys == 0 {
            return fail!("ECAM entry {} has zero base_phys", i);
        }
    }
    pass!()
}

pub fn test_all_entries_bus_range_valid() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => continue,
        };
        if entry.bus_start > entry.bus_end {
            return fail!(
                "ECAM entry {} has inverted bus range: start={} > end={}",
                i,
                entry.bus_start,
                entry.bus_end
            );
        }
    }
    pass!()
}

pub fn test_region_size_formula() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => continue,
        };
        let expected = (entry.bus_end as u64 - entry.bus_start as u64 + 1) * 256 * 4096;
        let actual = entry.region_size();
        if actual != expected {
            return fail!(
                "Entry {} region_size()={} expected {} (buses {}..{})",
                i,
                actual,
                expected,
                entry.bus_start,
                entry.bus_end
            );
        }
    }
    pass!()
}

pub fn test_region_size_full_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_start == 0 && entry.bus_end == 255 {
        let size = entry.region_size();
        let expected = 256 * 256 * 4096;
        if size != expected {
            return fail!(
                "Full-range region_size()={} expected {} (256 MiB)",
                size,
                expected
            );
        }
    }
    pass!()
}

pub fn test_ecam_offset_zero_bdf() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    let offset = entry.ecam_offset(entry.bus_start, 0, 0);
    match offset {
        None => return fail!("ecam_offset(bus_start, 0, 0) returned None"),
        Some(0) => {}
        Some(v) => return fail!("ecam_offset(bus_start, 0, 0) = 0x{:x}, expected 0", v),
    }
    pass!()
}

pub fn test_ecam_offset_known_bdf() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    let test_bus = entry.bus_start.saturating_add(1);
    if test_bus > entry.bus_end {
        return pass!();
    }
    let relative_bus = (test_bus - entry.bus_start) as u64;
    let expected = (relative_bus << 20) | (3u64 << 15) | (2u64 << 12);
    let actual = match entry.ecam_offset(test_bus, 3, 2) {
        Some(v) => v,
        None => return fail!("ecam_offset({}, 3, 2) returned None unexpectedly", test_bus),
    };
    if actual != expected {
        return fail!(
            "ecam_offset({}, 3, 2) = 0x{:x} expected 0x{:x}",
            test_bus,
            actual,
            expected
        );
    }
    pass!()
}

pub fn test_ecam_offset_bus_below_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_start == 0 {
        return pass!();
    }
    let below = entry.bus_start - 1;
    if entry.ecam_offset(below, 0, 0).is_some() {
        return fail!(
            "ecam_offset(bus={}, 0, 0) should return None (below bus_start={})",
            below,
            entry.bus_start
        );
    }
    pass!()
}

pub fn test_ecam_offset_bus_above_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_end == 255 {
        return pass!();
    }
    let above = entry.bus_end + 1;
    if entry.ecam_offset(above, 0, 0).is_some() {
        return fail!(
            "ecam_offset(bus={}, 0, 0) should return None (above bus_end={})",
            above,
            entry.bus_end
        );
    }
    pass!()
}

pub fn test_ecam_offset_device_out_of_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.ecam_offset(entry.bus_start, 32, 0).is_some() {
        return fail!("ecam_offset(bus_start, device=32, 0) should return None");
    }
    pass!()
}

pub fn test_ecam_offset_function_out_of_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.ecam_offset(entry.bus_start, 0, 8).is_some() {
        return fail!("ecam_offset(bus_start, 0, function=8) should return None");
    }
    pass!()
}

pub fn test_find_entry_segment0_bus0() -> TestResult {
    let entry = match pci_ecam_find_entry(0, 0) {
        Some(e) => e,
        None => return fail!("pci_ecam_find_entry(0, 0) returned None"),
    };
    if entry.segment != 0 {
        return fail!(
            "pci_ecam_find_entry(0, 0) returned segment {} (expected 0)",
            entry.segment
        );
    }
    pass!()
}

pub fn test_find_entry_nonexistent_segment() -> TestResult {
    if pci_ecam_find_entry(0xFFFF, 0).is_some() {
        return fail!("pci_ecam_find_entry(0xFFFF, 0) unexpectedly returned Some");
    }
    pass!()
}

pub fn test_ecam_base_matches_primary_entry() -> TestResult {
    let base = pci_ecam_base();
    let entry = match pci_ecam_find_entry(0, 0) {
        Some(e) => e,
        None => {
            if base == 0 {
                return pass!();
            }
            return fail!(
                "pci_ecam_base()=0x{:x} but find_entry(0,0) returned None",
                base
            );
        }
    };
    if base != entry.base_phys {
        return fail!(
            "pci_ecam_base()=0x{:x} != find_entry base_phys=0x{:x}",
            base,
            entry.base_phys
        );
    }
    pass!()
}

pub fn test_entry_count_matches_indexable() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        if pci_ecam_entry(i).is_none() {
            return fail!("pci_ecam_entry({}) is None but entry_count={}", i, count);
        }
    }
    if pci_ecam_entry(count as usize).is_some() {
        return fail!(
            "pci_ecam_entry({}) is Some but should be past-the-end",
            count
        );
    }
    pass!()
}

pub fn test_ecam_state_deterministic() -> TestResult {
    let base1 = pci_ecam_base();
    let count1 = pci_ecam_entry_count();
    let avail1 = pci_ecam_available();

    let base2 = pci_ecam_base();
    let count2 = pci_ecam_entry_count();
    let avail2 = pci_ecam_available();

    if base1 != base2 {
        return fail!("ECAM base changed: 0x{:x} vs 0x{:x}", base1, base2);
    }
    if count1 != count2 {
        return fail!("ECAM entry count changed: {} vs {}", count1, count2);
    }
    if avail1 != avail2 {
        return fail!("ECAM availability changed: {} vs {}", avail1, avail2);
    }

    for i in 0..count1 as usize {
        let e1 = pci_ecam_entry(i);
        let e2 = pci_ecam_entry(i);
        match (e1, e2) {
            (Some(a), Some(b)) => {
                if a.base_phys != b.base_phys
                    || a.segment != b.segment
                    || a.bus_start != b.bus_start
                    || a.bus_end != b.bus_end
                {
                    return fail!("ECAM entry {} changed between reads", i);
                }
            }
            (None, None) => {}
            _ => return fail!("ECAM entry {} availability changed between reads", i),
        }
    }
    pass!()
}

pub fn test_ecam_mmio_is_active() -> TestResult {
    if !pci_ecam_available() {
        return fail!("pci_ecam_available() returned false — ECAM MMIO not mapped on q35");
    }
    pass!()
}

pub fn test_ecam_backend_is_ecam() -> TestResult {
    if !pci_ecam_available() {
        return fail!("ECAM not available — expected ECAM-only backend");
    }
    pass!()
}

pub fn test_ecam_primary_virt_nonzero() -> TestResult {
    let virt = pci_ecam_primary_virt();
    if virt == 0 {
        return fail!("pci_ecam_primary_virt() returned 0 — primary MMIO not mapped");
    }
    pass!()
}

pub fn test_ecam_mapped_region_exists() -> TestResult {
    let region = match pci_ecam_mapped_region(0) {
        Some(r) => r,
        None => return fail!("pci_ecam_mapped_region(0) returned None"),
    };
    if !region.is_mapped() {
        return fail!("Primary ECAM region reports is_mapped()=false");
    }
    if region.size() == 0 {
        return fail!("Primary ECAM region has zero size");
    }
    pass!()
}

pub fn test_ecam_read32_vendor_device_id() -> TestResult {
    let dev_count = pci_get_device_count();
    if dev_count == 0 {
        return fail!("No PCI devices enumerated — cannot cross-validate ECAM reads");
    }

    for i in 0..dev_count {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        let ecam_val = match pci_ecam_read32(dev.bus, dev.device, dev.function, 0x00) {
            Some(v) => v,
            None => {
                return fail!(
                    "pci_ecam_read32({},{},{}, 0x00) returned None",
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
        };

        let expected_vendor = dev.vendor_id as u32;
        let expected_device = dev.device_id as u32;
        let expected = expected_vendor | (expected_device << 16);

        if ecam_val != expected {
            return fail!(
                "ECAM read32 BDF {}.{}.{} offset 0x00 = 0x{:08x}, expected 0x{:08x} (VID:DID)",
                dev.bus,
                dev.device,
                dev.function,
                ecam_val,
                expected,
            );
        }
    }
    pass!()
}

pub fn test_ecam_read16_vendor_id() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let vendor = match pci_ecam_read16(dev.bus, dev.device, dev.function, 0x00) {
        Some(v) => v,
        None => return fail!("pci_ecam_read16 returned None for VID"),
    };
    if vendor != dev.vendor_id {
        return fail!(
            "ECAM read16 VID=0x{:04x}, expected 0x{:04x}",
            vendor,
            dev.vendor_id
        );
    }
    pass!()
}

pub fn test_ecam_read16_device_id() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let did = match pci_ecam_read16(dev.bus, dev.device, dev.function, 0x02) {
        Some(v) => v,
        None => return fail!("pci_ecam_read16 returned None for DID"),
    };
    if did != dev.device_id {
        return fail!(
            "ECAM read16 DID=0x{:04x}, expected 0x{:04x}",
            did,
            dev.device_id
        );
    }
    pass!()
}

pub fn test_ecam_read8_class_code() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let class = match pci_ecam_read8(dev.bus, dev.device, dev.function, 0x0B) {
        Some(v) => v,
        None => return fail!("pci_ecam_read8 returned None for class code"),
    };
    if class != dev.class_code {
        return fail!(
            "ECAM read8 class=0x{:02x}, expected 0x{:02x}",
            class,
            dev.class_code
        );
    }
    pass!()
}

pub fn test_ecam_read8_revision() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let rev = match pci_ecam_read8(dev.bus, dev.device, dev.function, 0x08) {
        Some(v) => v,
        None => return fail!("pci_ecam_read8 returned None for revision"),
    };
    if rev != dev.revision {
        return fail!(
            "ECAM read8 revision=0x{:02x}, expected 0x{:02x}",
            rev,
            dev.revision
        );
    }
    pass!()
}

pub fn test_ecam_extended_config_readable() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let val = match pci_ecam_read32(dev.bus, dev.device, dev.function, 0x100) {
        Some(v) => v,
        None => {
            return fail!(
                "pci_ecam_read32 at offset 0x100 returned None — extended config space inaccessible"
            );
        }
    };
    // 0x100 is the PCIe extended capability header; QEMU may legitimately
    // report no ext caps, so only readability is checked.
    let _ = val;
    pass!()
}

pub fn test_ecam_extended_config_end_boundary() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    // Last valid 32-bit read: offset 0xFFC (bytes 0xFFC..0xFFF inclusive)
    let val = match pci_ecam_read32(dev.bus, dev.device, dev.function, 0xFFC) {
        Some(v) => v,
        None => return fail!("pci_ecam_read32 at offset 0xFFC returned None"),
    };
    let _ = val;
    pass!()
}

pub fn test_ecam_read32_misaligned_returns_none() -> TestResult {
    if pci_ecam_read32(0, 0, 0, 0x01).is_some() {
        return fail!("pci_ecam_read32 at misaligned offset 0x01 should return None");
    }
    if pci_ecam_read32(0, 0, 0, 0x03).is_some() {
        return fail!("pci_ecam_read32 at misaligned offset 0x03 should return None");
    }
    pass!()
}

pub fn test_ecam_read16_misaligned_returns_none() -> TestResult {
    if pci_ecam_read16(0, 0, 0, 0x01).is_some() {
        return fail!("pci_ecam_read16 at misaligned offset 0x01 should return None");
    }
    if pci_ecam_read16(0, 0, 0, 0x03).is_some() {
        return fail!("pci_ecam_read16 at misaligned offset 0x03 should return None");
    }
    pass!()
}

pub fn test_ecam_read32_overflow_returns_none() -> TestResult {
    if pci_ecam_read32(0, 0, 0, 0xFFD).is_some() {
        return fail!("pci_ecam_read32 at offset 0xFFD should return None (would overflow 4096)");
    }
    pass!()
}

pub fn test_ecam_read_invalid_device_returns_none() -> TestResult {
    if pci_ecam_read32(0, 32, 0, 0x00).is_some() {
        return fail!("pci_ecam_read32 with device=32 should return None");
    }
    pass!()
}

pub fn test_ecam_read_invalid_function_returns_none() -> TestResult {
    if pci_ecam_read32(0, 0, 8, 0x00).is_some() {
        return fail!("pci_ecam_read32 with function=8 should return None");
    }
    pass!()
}

pub fn test_ecam_reads_deterministic() -> TestResult {
    let dev = match pci_get_device(0) {
        Some(d) => d,
        None => return fail!("No device at index 0"),
    };
    let v1 = pci_ecam_read32(dev.bus, dev.device, dev.function, 0x00);
    let v2 = pci_ecam_read32(dev.bus, dev.device, dev.function, 0x00);
    if v1 != v2 {
        return fail!(
            "ECAM read32 at 0x00 not deterministic: 0x{:x?} vs 0x{:x?}",
            v1,
            v2
        );
    }

    let v1 = pci_ecam_read16(dev.bus, dev.device, dev.function, 0x04);
    let v2 = pci_ecam_read16(dev.bus, dev.device, dev.function, 0x04);
    if v1 != v2 {
        return fail!(
            "ECAM read16 at 0x04 not deterministic: {:x?} vs {:x?}",
            v1,
            v2
        );
    }

    let v1 = pci_ecam_read8(dev.bus, dev.device, dev.function, 0x0B);
    let v2 = pci_ecam_read8(dev.bus, dev.device, dev.function, 0x0B);
    if v1 != v2 {
        return fail!(
            "ECAM read8 at 0x0B not deterministic: {:x?} vs {:x?}",
            v1,
            v2
        );
    }
    pass!()
}

pub fn test_ecam_sweep_all_devices() -> TestResult {
    let count = pci_get_device_count();
    for i in 0..count {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };

        let vid = match pci_ecam_read16(dev.bus, dev.device, dev.function, 0x00) {
            Some(v) => v,
            None => {
                return fail!(
                    "ECAM read16 VID failed for device {} ({}.{}.{})",
                    i,
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
        };

        if vid != dev.vendor_id {
            return fail!(
                "Device {} ECAM VID=0x{:04x} != enumerated VID=0x{:04x}",
                i,
                vid,
                dev.vendor_id
            );
        }

        let class = match pci_ecam_read8(dev.bus, dev.device, dev.function, 0x0B) {
            Some(v) => v,
            None => {
                return fail!(
                    "ECAM read8 class failed for device {} ({}.{}.{})",
                    i,
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
        };

        if class != dev.class_code {
            return fail!(
                "Device {} ECAM class=0x{:02x} != enumerated class=0x{:02x}",
                i,
                class,
                dev.class_code
            );
        }
    }
    pass!()
}

slopos_testing::stest!(name = test_ecam_available, suite = ecam);
slopos_testing::stest!(name = test_ecam_entry_count_nonzero, suite = ecam);
slopos_testing::stest!(name = test_ecam_base_nonzero, suite = ecam);
slopos_testing::stest!(name = test_ecam_base_page_aligned, suite = ecam);
slopos_testing::stest!(name = test_primary_entry_covers_bus_zero, suite = ecam);
slopos_testing::stest!(name = test_all_entries_base_nonzero, suite = ecam);
slopos_testing::stest!(name = test_all_entries_bus_range_valid, suite = ecam);
slopos_testing::stest!(name = test_region_size_formula, suite = ecam);
slopos_testing::stest!(name = test_region_size_full_range, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_zero_bdf, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_known_bdf, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_bus_below_range, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_bus_above_range, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_device_out_of_range, suite = ecam);
slopos_testing::stest!(name = test_ecam_offset_function_out_of_range, suite = ecam);
slopos_testing::stest!(name = test_find_entry_segment0_bus0, suite = ecam);
slopos_testing::stest!(name = test_find_entry_nonexistent_segment, suite = ecam);
slopos_testing::stest!(name = test_ecam_base_matches_primary_entry, suite = ecam);
slopos_testing::stest!(name = test_entry_count_matches_indexable, suite = ecam);
slopos_testing::stest!(name = test_ecam_state_deterministic, suite = ecam);
slopos_testing::stest!(name = test_ecam_mmio_is_active, suite = ecam);
slopos_testing::stest!(name = test_ecam_backend_is_ecam, suite = ecam);
slopos_testing::stest!(name = test_ecam_primary_virt_nonzero, suite = ecam);
slopos_testing::stest!(name = test_ecam_mapped_region_exists, suite = ecam);
slopos_testing::stest!(name = test_ecam_read32_vendor_device_id, suite = ecam);
slopos_testing::stest!(name = test_ecam_read16_vendor_id, suite = ecam);
slopos_testing::stest!(name = test_ecam_read16_device_id, suite = ecam);
slopos_testing::stest!(name = test_ecam_read8_class_code, suite = ecam);
slopos_testing::stest!(name = test_ecam_read8_revision, suite = ecam);
slopos_testing::stest!(name = test_ecam_extended_config_readable, suite = ecam);
slopos_testing::stest!(name = test_ecam_extended_config_end_boundary, suite = ecam);
slopos_testing::stest!(
    name = test_ecam_read32_misaligned_returns_none,
    suite = ecam
);
slopos_testing::stest!(
    name = test_ecam_read16_misaligned_returns_none,
    suite = ecam
);
slopos_testing::stest!(name = test_ecam_read32_overflow_returns_none, suite = ecam);
slopos_testing::stest!(
    name = test_ecam_read_invalid_device_returns_none,
    suite = ecam
);
slopos_testing::stest!(
    name = test_ecam_read_invalid_function_returns_none,
    suite = ecam
);
slopos_testing::stest!(name = test_ecam_reads_deterministic, suite = ecam);
slopos_testing::stest!(name = test_ecam_sweep_all_devices, suite = ecam);
