//! PCI capability list parsing regression tests.
//!
//! Run after PCI enumeration; assertions rely on QEMU q35's deterministic
//! device and capability topology.

use slopos_testing::TestResult;
use slopos_testing::{fail, pass};

use crate::pci::{
    PciCapabilityIter, PciDeviceInfo, pci_find_capability, pci_get_device, pci_get_device_count,
};
use crate::pci_defs::*;

pub fn test_pci_enumeration_nonempty() -> TestResult {
    let count = pci_get_device_count();
    if count == 0 {
        return fail!("PCI device count is 0 — enumeration did not run or q35 is misconfigured");
    }
    pass!()
}

/// The q35 host bridge (8086:29c0) exposes no capabilities on QEMU.
pub fn test_cap_iter_empty_for_no_caps_device() -> TestResult {
    let dev = match find_device_by_class(0x06, 0x00) {
        Some(d) => d,
        None => return fail!("No host bridge (class 06:00) found — unexpected q35 topology"),
    };

    let cap_count = PciCapabilityIter::for_device(&dev).count();
    if cap_count != 0 {
        return fail!(
            "Host bridge {:04x}:{:04x} should have 0 capabilities, got {}",
            dev.vendor_id,
            dev.device_id,
            cap_count
        );
    }
    pass!()
}

pub fn test_cap_iter_deterministic() -> TestResult {
    let dev = match find_first_device_with_caps() {
        Some(d) => d,
        None => return fail!("No PCI device with capabilities found"),
    };

    let mut first = [PciCapability { offset: 0, id: 0 }; 48];
    let mut first_len = 0usize;
    for cap in PciCapabilityIter::for_device(&dev) {
        if first_len < 48 {
            first[first_len] = cap;
            first_len += 1;
        }
    }
    let mut second = [PciCapability { offset: 0, id: 0 }; 48];
    let mut second_len = 0usize;
    for cap in PciCapabilityIter::for_device(&dev) {
        if second_len < 48 {
            second[second_len] = cap;
            second_len += 1;
        }
    }

    if first_len != second_len {
        return fail!(
            "Capability count changed between iterations: {} vs {}",
            first_len,
            second_len
        );
    }
    for i in 0..first_len {
        let a = first[i];
        let b = second[i];
        if a != b {
            return fail!(
                "Capability mismatch at index {}: ({:02x}@{:02x}) vs ({:02x}@{:02x})",
                i,
                a.id,
                a.offset,
                b.id,
                b.offset
            );
        }
    }
    pass!()
}

pub fn test_cap_offsets_valid() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        for cap in PciCapabilityIter::for_device(&dev) {
            if (cap.offset & 0x03) != 0 {
                return fail!(
                    "Cap 0x{:02x} at offset 0x{:02x} on {:02x}:{:02x}.{} is not DWORD-aligned",
                    cap.id,
                    cap.offset,
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
            if cap.offset < 0x40 {
                return fail!(
                    "Cap 0x{:02x} at offset 0x{:02x} on {:02x}:{:02x}.{} is below 0x40 (overlaps standard header)",
                    cap.id,
                    cap.offset,
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
        }
    }
    pass!()
}

pub fn test_virtio_blk_has_msix() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device (1af4:1042) not found"),
    };

    if !dev.has_msix() {
        return fail!("VirtIO block device has_msix() returned false");
    }
    if dev.msix_cap_offset.is_none() {
        return fail!("VirtIO block device msix_cap_offset is None");
    }
    pass!()
}

pub fn test_virtio_net_has_msix() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1041) {
        Some(d) => d,
        None => return fail!("VirtIO net device (1af4:1041) not found"),
    };

    if !dev.has_msix() {
        return fail!("VirtIO net device has_msix() returned false");
    }
    if dev.msix_cap_offset.is_none() {
        return fail!("VirtIO net device msix_cap_offset is None");
    }
    pass!()
}

pub fn test_virtio_has_vendor_caps() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device (1af4:1042) not found"),
    };

    let vendor_count = PciCapabilityIter::for_device(&dev)
        .filter(|c| c.id == PCI_CAP_ID_VNDR)
        .count();

    // Modern virtio needs four: common_cfg, notify_cfg, isr_cfg, device_cfg.
    if vendor_count < 4 {
        return fail!(
            "VirtIO block device has only {} vendor caps (need >= 4)",
            vendor_count
        );
    }
    pass!()
}

pub fn test_sata_has_msi() -> TestResult {
    let dev = match find_device_by_vendor_device(0x8086, 0x2922) {
        Some(d) => d,
        None => return fail!("SATA controller (8086:2922) not found"),
    };

    if !dev.has_msi() {
        return fail!("SATA controller has_msi() returned false");
    }
    if dev.msi_cap_offset.is_none() {
        return fail!("SATA controller msi_cap_offset is None");
    }
    if dev.has_msix() {
        return fail!("SATA controller unexpectedly has MSI-X");
    }
    pass!()
}

pub fn test_has_msi_matches_offset() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        if dev.has_msi() != dev.msi_cap_offset.is_some() {
            return fail!(
                "has_msi() disagrees with msi_cap_offset on {:02x}:{:02x}.{}",
                dev.bus,
                dev.device,
                dev.function
            );
        }
    }
    pass!()
}

pub fn test_has_msix_matches_offset() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        if dev.has_msix() != dev.msix_cap_offset.is_some() {
            return fail!(
                "has_msix() disagrees with msix_cap_offset on {:02x}:{:02x}.{}",
                dev.bus,
                dev.device,
                dev.function
            );
        }
    }
    pass!()
}

pub fn test_stored_msi_offset_matches_live_walk() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        let live = pci_find_capability(dev.bus, dev.device, dev.function, PCI_CAP_ID_MSI);
        if dev.msi_cap_offset != live {
            return fail!(
                "msi_cap_offset {:?} != live walk {:?} on {:02x}:{:02x}.{}",
                dev.msi_cap_offset,
                live,
                dev.bus,
                dev.device,
                dev.function
            );
        }
    }
    pass!()
}

pub fn test_stored_msix_offset_matches_live_walk() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        let live = pci_find_capability(dev.bus, dev.device, dev.function, PCI_CAP_ID_MSIX);
        if dev.msix_cap_offset != live {
            return fail!(
                "msix_cap_offset {:?} != live walk {:?} on {:02x}:{:02x}.{}",
                dev.msix_cap_offset,
                live,
                dev.bus,
                dev.device,
                dev.function
            );
        }
    }
    pass!()
}

pub fn test_find_capability_method_matches_free_fn() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        for cap_id in [
            PCI_CAP_ID_MSI,
            PCI_CAP_ID_MSIX,
            PCI_CAP_ID_VNDR,
            PCI_CAP_ID_PCIE,
        ] {
            let via_method = dev.find_capability(cap_id);
            let via_fn = pci_find_capability(dev.bus, dev.device, dev.function, cap_id);
            if via_method != via_fn {
                return fail!(
                    "find_capability(0x{:02x}) method={:?} fn={:?} on {:02x}:{:02x}.{}",
                    cap_id,
                    via_method,
                    via_fn,
                    dev.bus,
                    dev.device,
                    dev.function
                );
            }
        }
    }
    pass!()
}

pub fn test_find_nonexistent_cap_returns_none() -> TestResult {
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        if let Some(off) = dev.find_capability(0xFF) {
            return fail!(
                "find_capability(0xFF) returned Some(0x{:02x}) on {:02x}:{:02x}.{}",
                off,
                dev.bus,
                dev.device,
                dev.function
            );
        }
    }
    pass!()
}

/// A nonexistent BDF reads all-ones, so Status claims capabilities are present
/// and every capability ID reads 0xFF: a standard-ID search must still miss.
pub fn test_find_cap_on_nonexistent_device_returns_none() -> TestResult {
    let result = pci_find_capability(255, 31, 7, PCI_CAP_ID_MSI);
    if result.is_some() {
        return fail!(
            "pci_find_capability on nonexistent device returned Some(0x{:02x})",
            result.unwrap()
        );
    }
    pass!()
}

fn find_device_by_class(class: u8, subclass: u8) -> Option<PciDeviceInfo> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if dev.class_code == class && dev.subclass == subclass {
                return Some(dev);
            }
        }
    }
    None
}

fn find_device_by_vendor_device(vendor: u16, device: u16) -> Option<PciDeviceInfo> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if dev.vendor_id == vendor && dev.device_id == device {
                return Some(dev);
            }
        }
    }
    None
}

fn find_first_device_with_caps() -> Option<PciDeviceInfo> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if PciCapabilityIter::for_device(&dev).next().is_some() {
                return Some(dev);
            }
        }
    }
    None
}

slopos_testing::stest!(name = test_pci_enumeration_nonempty, suite = pci_cap);
slopos_testing::stest!(
    name = test_cap_iter_empty_for_no_caps_device,
    suite = pci_cap
);
slopos_testing::stest!(name = test_cap_iter_deterministic, suite = pci_cap);
slopos_testing::stest!(name = test_cap_offsets_valid, suite = pci_cap);
slopos_testing::stest!(name = test_virtio_blk_has_msix, suite = pci_cap);
slopos_testing::stest!(name = test_virtio_net_has_msix, suite = pci_cap);
slopos_testing::stest!(name = test_virtio_has_vendor_caps, suite = pci_cap);
slopos_testing::stest!(name = test_sata_has_msi, suite = pci_cap);
slopos_testing::stest!(name = test_has_msi_matches_offset, suite = pci_cap);
slopos_testing::stest!(name = test_has_msix_matches_offset, suite = pci_cap);
slopos_testing::stest!(
    name = test_stored_msi_offset_matches_live_walk,
    suite = pci_cap
);
slopos_testing::stest!(
    name = test_stored_msix_offset_matches_live_walk,
    suite = pci_cap
);
slopos_testing::stest!(
    name = test_find_capability_method_matches_free_fn,
    suite = pci_cap
);
slopos_testing::stest!(
    name = test_find_nonexistent_cap_returns_none,
    suite = pci_cap
);
slopos_testing::stest!(
    name = test_find_cap_on_nonexistent_device_returns_none,
    suite = pci_cap
);
