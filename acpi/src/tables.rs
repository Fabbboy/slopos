//! Kernel-side ACPI table lookup: takes the bootloader-published RSDP physical
//! address, slices the HHDM-mapped ACPI region, and returns checksum-validated
//! `AcpiTable<'static>` views over the OSTD primitives.

use core::mem;

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm;
use slopos_ostd::boot::handoff::acpi_region_bytes as ostd_acpi_region_bytes;
use slopos_ostd::klog_info;
use slopos_ostd::util::packed_view::read_packed;

pub use slopos_ostd::acpi::{AcpiTable, RSDP_SIGNATURE, RSDP_V1_SIZE, Rsdp, SdtHeader};

/// `None` when HHDM is unavailable or the address translates to null.
fn acpi_region_bytes(phys: u64, len: usize) -> Option<&'static [u8]> {
    if !hhdm::is_available() {
        return None;
    }
    ostd_acpi_region_bytes(PhysAddr::new(phys), len)
}

pub struct AcpiTables {
    rsdt_phys: u32,
    xsdt_phys: u64,
    revision: u8,
}

impl AcpiTables {
    pub fn from_phys(rsdp_phys: u64) -> Option<Self> {
        if !hhdm::is_available() {
            klog_info!("ACPI: HHDM unavailable, cannot parse tables");
            return None;
        }
        let probe = acpi_region_bytes(rsdp_phys, RSDP_V1_SIZE)?;
        if probe.len() < RSDP_V1_SIZE {
            return None;
        }
        let revision = probe[15];
        let rsdp = if revision >= 2 {
            // The v2 checksum covers the full structure, not just the v1 prefix.
            let full = acpi_region_bytes(rsdp_phys, mem::size_of::<Rsdp>())?;
            Rsdp::validate(full)?
        } else {
            Rsdp::validate(probe)?
        };

        Some(Self {
            rsdt_phys: rsdp.rsdt_address,
            xsdt_phys: rsdp.xsdt_address,
            revision: rsdp.revision,
        })
    }

    /// Searches the XSDT when the RSDP is v2+, else the RSDT.
    pub fn find_table(&self, signature: &[u8; 4]) -> Option<AcpiTable<'static>> {
        if self.revision >= 2 && self.xsdt_phys != 0 {
            if let Some(hit) = self.scan_root(self.xsdt_phys, mem::size_of::<u64>(), signature) {
                return Some(hit);
            }
        }
        if self.rsdt_phys != 0 {
            return self.scan_root(self.rsdt_phys as u64, mem::size_of::<u32>(), signature);
        }
        None
    }

    /// Raw bytes, checksum **not** validated. Visits *every* match rather than
    /// the first: a platform can ship several SSDTs and `\_S5` may be in any.
    pub fn find_map_raw<T>(
        &self,
        signature: &[u8; 4],
        mut f: impl FnMut(&'static [u8]) -> Option<T>,
    ) -> Option<T> {
        let (root_phys, entry_size) = if self.revision >= 2 && self.xsdt_phys != 0 {
            (self.xsdt_phys, mem::size_of::<u64>())
        } else if self.rsdt_phys != 0 {
            (self.rsdt_phys as u64, mem::size_of::<u32>())
        } else {
            return None;
        };
        let root = load_table(root_phys)?;
        let payload = root.payload();
        let entry_count = payload.len() / entry_size;
        for i in 0..entry_count {
            let off = i * entry_size;
            let phys = if entry_size == 8 {
                read_packed::<u64>(payload, off)?
            } else {
                read_packed::<u32>(payload, off)? as u64
            };
            let Some(bytes) = table_bytes_at(phys) else {
                continue;
            };
            if bytes.len() >= 4 && &bytes[0..4] == signature {
                if let Some(result) = f(bytes) {
                    return Some(result);
                }
            }
        }
        None
    }

    fn scan_root(
        &self,
        root_phys: u64,
        entry_size: usize,
        signature: &[u8; 4],
    ) -> Option<AcpiTable<'static>> {
        let root = load_table(root_phys)?;
        let payload = root.payload();
        let entry_count = payload.len() / entry_size;
        for i in 0..entry_count {
            let off = i * entry_size;
            let phys = if entry_size == 8 {
                read_packed::<u64>(payload, off)?
            } else {
                read_packed::<u32>(payload, off)? as u64
            };
            let Some(candidate) = load_table(phys) else {
                continue;
            };
            if &candidate.signature() == signature {
                return Some(candidate);
            }
        }
        None
    }
}

/// Checksum *not* validated: some firmware ships a DSDT with a stale checksum
/// that [`AcpiTable::from_bytes`] would (correctly) reject.
pub fn table_bytes_at(phys: u64) -> Option<&'static [u8]> {
    let header_size = mem::size_of::<SdtHeader>();
    let header_bytes = acpi_region_bytes(phys, header_size)?;
    if header_bytes.len() < header_size {
        return None;
    }
    let length = read_packed::<u32>(header_bytes, 4)? as usize;
    if length < header_size {
        return None;
    }
    acpi_region_bytes(phys, length)
}

/// Checksum-validated, unlike [`table_bytes_at`].
fn load_table(phys: u64) -> Option<AcpiTable<'static>> {
    let header_size = mem::size_of::<SdtHeader>();
    let header_bytes = acpi_region_bytes(phys, header_size)?;
    if header_bytes.len() < header_size {
        return None;
    }
    let length = read_packed::<u32>(header_bytes, 4)? as usize;
    if length < header_size {
        return None;
    }
    let full = acpi_region_bytes(phys, length)?;
    AcpiTable::from_bytes(full)
}
