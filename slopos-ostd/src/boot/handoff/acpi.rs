//! ACPI table region handoff.
//!
//! Reads `len` bytes of HHDM-mapped firmware memory at physical
//! address `phys`, validates the ACPI checksum, returns an
//! [`AcpiTable`] view. The `core::slice::from_raw_parts` call lives
//! here — consumers get a typed checksum-validated borrow.

use slopos_abi::addr::PhysAddr;

use crate::acpi::AcpiTable;
use crate::boot::hhdm;

/// Borrow `len` bytes of HHDM-mapped ACPI firmware memory and parse
/// them as an [`AcpiTable`]. Returns `None` if any of the following
/// fail:
///
/// - `len == 0`
/// - the HHDM offset has not been registered yet
///   (`crate::boot::hhdm::register_hhdm_offset` must precede this call)
/// - `phys == 0` (Limine never publishes ACPI tables at physical 0)
/// - the table-length field in the SDT header disagrees with `len`
/// - the 8-bit additive checksum is non-zero
///
/// # Safety (interior)
///
/// The bootloader publishes ACPI tables in firmware-reserved memory
/// that the kernel keeps mapped for its lifetime. The interior
/// `slice::from_raw_parts` call is sound under that contract; callers
/// see only `Option<AcpiTable<'static>>`.
pub fn acpi_handoff(phys: PhysAddr, len: usize) -> Option<AcpiTable<'static>> {
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
    // validated against `hdr.length` — `AcpiTable::from_bytes`
    // re-validates the length before returning `Some`.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(ptr, len) };
    AcpiTable::from_bytes(bytes)
}
