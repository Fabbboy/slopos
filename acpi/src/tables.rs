use core::mem;

use slopos_abi::addr::PhysAddr;
use slopos_mm::hhdm::{self, PhysAddrHhdm};
use slopos_ostd::util::packed_view::read_packed;
use slopos_utils::klog_info;

#[repr(C, packed)]
pub struct Rsdp {
    pub signature: [u8; 8],
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub revision: u8,
    pub rsdt_address: u32,
    pub length: u32,
    pub xsdt_address: u64,
    pub extended_checksum: u8,
    pub reserved: [u8; 3],
}

#[repr(C, packed)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_revision: u32,
}

fn checksum(data: *const u8, length: usize) -> u8 {
    if data.is_null() || length == 0 {
        return 0;
    }
    // SAFETY: caller ensures `[data, data + length)` is a contiguous,
    // mapped byte range representing an ACPI table; this byte slice is
    // borrowed only for the duration of the `iter().fold(...)` below.
    let bytes = unsafe { core::slice::from_raw_parts(data, length) };
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

fn validate_rsdp(rsdp: *const Rsdp) -> bool {
    if rsdp.is_null() {
        return false;
    }
    let rsdp_ref = unsafe { &*rsdp };
    if checksum(rsdp as *const u8, 20) != 0 {
        return false;
    }
    if rsdp_ref.revision >= 2 && rsdp_ref.length as usize >= mem::size_of::<Rsdp>() {
        if checksum(rsdp as *const u8, rsdp_ref.length as usize) != 0 {
            return false;
        }
    }
    true
}

fn validate_table(header: *const SdtHeader) -> bool {
    if header.is_null() {
        return false;
    }
    let hdr = unsafe { &*header };
    if hdr.length < mem::size_of::<SdtHeader>() as u32 {
        return false;
    }
    checksum(header as *const u8, hdr.length as usize) == 0
}

fn map_phys_table(phys_addr: u64) -> *const SdtHeader {
    if phys_addr == 0 {
        return core::ptr::null();
    }
    PhysAddr::new(phys_addr)
        .try_to_virt()
        .map(|v| v.as_ptr())
        .unwrap_or(core::ptr::null())
}

fn scan_sdt(sdt: *const SdtHeader, entry_size: usize, signature: &[u8; 4]) -> *const SdtHeader {
    if sdt.is_null() {
        return core::ptr::null();
    }

    let hdr = unsafe { &*sdt };
    if hdr.length < mem::size_of::<SdtHeader>() as u32 {
        return core::ptr::null();
    }

    let payload_bytes = hdr.length as usize - mem::size_of::<SdtHeader>();
    let entry_count = payload_bytes / entry_size;
    let entries = (sdt as *const u8).wrapping_add(mem::size_of::<SdtHeader>());

    // Borrow the SDT entry payload as a single byte slice and walk it
    // with `read_packed`. The unsafe `from_raw_parts` lives once at
    // the boundary where we go from raw `*const SdtHeader` (with a
    // length-validated `hdr.length`) to a safe slice.
    //
    // SAFETY: `hdr.length` was just validated against the SDT header
    // size; `entries` is the byte at `sdt + size_of::<SdtHeader>()`,
    // which the same allocation covers.
    let payload = unsafe { core::slice::from_raw_parts(entries, payload_bytes) };

    for i in 0..entry_count {
        let off = i * entry_size;
        let phys = if entry_size == 8 {
            read_packed::<u64>(payload, off).unwrap_or(0)
        } else {
            read_packed::<u32>(payload, off).unwrap_or(0) as u64
        };

        let candidate = map_phys_table(phys);
        if candidate.is_null() {
            continue;
        }
        let candidate_ref = unsafe { &*candidate };
        if candidate_ref.signature != *signature {
            continue;
        }
        if !validate_table(candidate) {
            klog_info!("ACPI: Found table with invalid checksum, skipping");
            continue;
        }
        return candidate;
    }
    core::ptr::null()
}

/// Validated handle to the ACPI table hierarchy rooted at an RSDP.
///
/// This is the entry point for all ACPI table access. Created via
/// [`AcpiTables::from_rsdp`], which validates the RSDP checksum and
/// verifies HHDM availability before returning.
pub struct AcpiTables {
    rsdp: *const Rsdp,
}

impl AcpiTables {
    /// Validate an RSDP pointer and return a handle for table lookups.
    ///
    /// Returns `None` if:
    /// - HHDM is not available (physical-to-virtual translation impossible)
    /// - `rsdp` is null
    /// - RSDP checksum validation fails
    pub fn from_rsdp(rsdp: *const Rsdp) -> Option<Self> {
        if !hhdm::is_available() {
            klog_info!("ACPI: HHDM unavailable, cannot parse tables");
            return None;
        }
        if !validate_rsdp(rsdp) {
            klog_info!("ACPI: RSDP checksum failed");
            return None;
        }
        Some(Self { rsdp })
    }

    /// Find an ACPI table by its 4-byte ASCII signature.
    ///
    /// Searches XSDT first (64-bit entries), falls back to RSDT (32-bit entries).
    /// Returns a pointer to the validated `SdtHeader`, or null if not found.
    pub fn find_table(&self, signature: &[u8; 4]) -> *const SdtHeader {
        let rsdp_ref = unsafe { &*self.rsdp };

        if rsdp_ref.revision >= 2 && rsdp_ref.xsdt_address != 0 {
            let xsdt = map_phys_table(rsdp_ref.xsdt_address);
            if !xsdt.is_null() && validate_table(xsdt) {
                let hit = scan_sdt(xsdt, mem::size_of::<u64>(), signature);
                if !hit.is_null() {
                    return hit;
                }
            }
        }

        if rsdp_ref.rsdt_address != 0 {
            let rsdt = map_phys_table(rsdp_ref.rsdt_address as u64);
            if !rsdt.is_null() && validate_table(rsdt) {
                let hit = scan_sdt(rsdt, mem::size_of::<u32>(), signature);
                if !hit.is_null() {
                    return hit;
                }
            }
        }

        core::ptr::null()
    }
}
