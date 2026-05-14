//! ACPI table lookup over the OSTD `Rsdp` / `AcpiTable` primitives.
//!
//! `slopos_ostd::acpi` provides the validated `Rsdp::validate` /
//! `AcpiTable::from_bytes` primitives over byte slices. This module
//! is the kernel-side consumer: it takes the bootloader-published
//! RSDP physical address, slices the HHDM-mapped ACPI region, and
//! returns checksum-validated `AcpiTable<'static>` views over each
//! discovered table.
//!
//! All HHDM byte-borrows funnel through
//! [`slopos_ostd::boot::handoff::acpi_region_bytes`]; this module
//! holds no `unsafe` of its own.

use core::mem;

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm;
use slopos_ostd::boot::handoff::acpi_region_bytes as ostd_acpi_region_bytes;
use slopos_ostd::klog_info;
use slopos_ostd::util::packed_view::read_packed;

pub use slopos_ostd::acpi::{AcpiTable, RSDP_SIGNATURE, RSDP_V1_SIZE, Rsdp, SdtHeader};

/// Borrow `len` bytes of the HHDM-mapped ACPI region starting at the
/// given physical address. Returns `None` if HHDM is unavailable or
/// the address translates to a null pointer.
///
/// Thin delegate over [`slopos_ostd::boot::handoff::acpi_region_bytes`]:
/// gates on the kernel-side `hhdm::is_available()` flag (the OSTD
/// helper independently re-checks its own HHDM-offset registry), then
/// forwards.
fn acpi_region_bytes(phys: u64, len: usize) -> Option<&'static [u8]> {
    if !hhdm::is_available() {
        return None;
    }
    ostd_acpi_region_bytes(PhysAddr::new(phys), len)
}

/// Validated handle to the ACPI table hierarchy rooted at an RSDP.
pub struct AcpiTables {
    rsdt_phys: u32,
    xsdt_phys: u64,
    revision: u8,
}

impl AcpiTables {
    /// Probe the RSDP at the given physical address, validate the
    /// checksum, and return a handle for table lookups.
    pub fn from_phys(rsdp_phys: u64) -> Option<Self> {
        if !hhdm::is_available() {
            klog_info!("ACPI: HHDM unavailable, cannot parse tables");
            return None;
        }
        // Probe the v1 prefix to check the signature and read the
        // revision byte.
        let probe = acpi_region_bytes(rsdp_phys, RSDP_V1_SIZE)?;
        if probe.len() < RSDP_V1_SIZE {
            return None;
        }
        let revision = probe[15];
        let rsdp = if revision >= 2 {
            // V2: re-borrow the full structure for the v2 checksum.
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

    /// Find an ACPI table by its 4-byte ASCII signature.
    ///
    /// Searches XSDT first (64-bit entries) when the RSDP is v2+;
    /// falls back to RSDT (32-bit entries) otherwise.
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

    /// Walk an XSDT/RSDT root table, looking for `signature` among
    /// its entries.
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

/// Load a checksum-validated table at a physical address.
///
/// Probes the SDT header to read its declared length, then re-borrows
/// the full table and validates via [`AcpiTable::from_bytes`].
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
