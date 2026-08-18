//! ACPI table region handoff.
//!
//! Localises the `core::slice::from_raw_parts` over HHDM-mapped firmware
//! memory; consumers get a typed, checksum-validated borrow.

use slopos_abi::addr::PhysAddr;

use crate::acpi::AcpiTable;
use crate::boot::hhdm;

/// Borrow `len` bytes of HHDM-mapped ACPI firmware memory and parse them as an
/// [`AcpiTable`]. `None` unless `crate::boot::hhdm::register_hhdm_offset` has
/// already run, `len` and `phys` are non-zero (Limine never publishes ACPI
/// tables at physical 0), and the SDT length field and 8-bit additive checksum
/// both validate.
pub fn acpi_handoff(phys: PhysAddr, len: usize) -> Option<AcpiTable<'static>> {
    let bytes = acpi_region_bytes(phys, len)?;
    AcpiTable::from_bytes(bytes)
}

/// Raw byte primitive behind [`acpi_handoff`]: same preconditions, but no
/// checksum or table-length validation, for consumers that probe RSDP or
/// SDT-header prefixes at several lengths before re-borrowing the full table.
pub fn acpi_region_bytes(phys: PhysAddr, len: usize) -> Option<&'static [u8]> {
    if len == 0 || phys.0 == 0 {
        return None;
    }
    let off = hhdm::hhdm_offset()?;
    let virt = phys.0.checked_add(off)?;
    let ptr = virt as *const u8;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: bootloader-published, HHDM-mapped, kernel-lifetime
    // backing. `len` is supplied by the caller and either fits inside
    // a probe size (e.g. `size_of::<SdtHeader>()`) or has been
    // validated against `hdr.length` by a higher-level checksum step.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}
