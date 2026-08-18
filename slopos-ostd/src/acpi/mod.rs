//! ACPI table primitives.
//!
//! Pod definitions of the on-wire structures (RSDP, SDT header) plus a
//! checksum-validated borrow over a pre-mapped byte slice.
//!
//! The structures carry a hand-written `unsafe impl Pod` because the derive
//! macro rejects packed layouts; every byte sequence of the right length is a
//! valid ACPI structure, since validation is over the checksum and signature
//! rather than the type.

use crate::mm::Pod;

/// Root System Description Pointer: 36 bytes for revision 2+, 20 bytes for
/// revision 0/1.
#[repr(C, packed)]
#[derive(Clone, Copy)]
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

// SAFETY: packed over `Copy` primitives and fixed-size byte arrays; no
// padding, no invalid bit patterns.
unsafe impl Pod for Rsdp {}

/// System Description Table header, 36 bytes. Every ACPI table (RSDT, XSDT,
/// MADT, MCFG, HPET, FADT, ...) begins with one of these.
#[repr(C, packed)]
#[derive(Clone, Copy)]
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

// SAFETY: packed over `Copy` primitives and fixed-size byte arrays; no
// padding, no invalid bit patterns.
unsafe impl Pod for SdtHeader {}

/// Canonical RSDP signature; the trailing byte is a space, not NUL.
pub const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";

/// Spec-fixed length of the RSDP v1 structure: signature through the v1 RSDT
/// address. The v2 fields follow and are covered by the extended checksum.
pub const RSDP_V1_SIZE: usize = 20;

impl Rsdp {
    /// Validate `bytes` as a complete RSDP and copy out the structure:
    /// signature, length for the indicated revision, and the applicable
    /// checksums summing to zero.
    pub fn validate(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RSDP_V1_SIZE {
            return None;
        }
        if bytes[0..8] != RSDP_SIGNATURE {
            return None;
        }
        if checksum(&bytes[..RSDP_V1_SIZE]) != 0 {
            return None;
        }
        let revision = bytes[15];
        if revision >= 2 {
            if bytes.len() < core::mem::size_of::<Rsdp>() {
                return None;
            }
            // V2 records the full structure length in bytes 20..24.
            let length = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
            if length < core::mem::size_of::<Rsdp>() || length > bytes.len() {
                return None;
            }
            if checksum(&bytes[..length]) != 0 {
                return None;
            }
        }
        // Field-by-field copy builds the packed struct without a
        // `read_unaligned`.
        let mut sig = [0u8; 8];
        sig.copy_from_slice(&bytes[0..8]);
        let mut oem_id = [0u8; 6];
        oem_id.copy_from_slice(&bytes[9..15]);
        let rsdt_address = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let (length, xsdt_address, ext_csum, reserved) = if revision >= 2 {
            let length = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            let xsdt = u64::from_le_bytes([
                bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30],
                bytes[31],
            ]);
            let ext_csum = bytes[32];
            let reserved = [bytes[33], bytes[34], bytes[35]];
            (length, xsdt, ext_csum, reserved)
        } else {
            (0u32, 0u64, 0u8, [0u8; 3])
        };
        Some(Rsdp {
            signature: sig,
            checksum: bytes[8],
            oem_id,
            revision,
            rsdt_address,
            length,
            xsdt_address,
            extended_checksum: ext_csum,
            reserved,
        })
    }
}

/// Checksum-validated borrow over a complete ACPI table: the slice must span
/// the [`size_of::<SdtHeader>()`]-byte header and the full payload declared by
/// `header.length`.
pub struct AcpiTable<'a> {
    bytes: &'a [u8],
}

impl<'a> AcpiTable<'a> {
    /// Returns `None` unless `bytes` covers a full header, `header.length`
    /// equals `bytes.len()`, and the byte sum of `bytes` is zero.
    pub fn from_bytes(bytes: &'a [u8]) -> Option<Self> {
        let hdr_size = core::mem::size_of::<SdtHeader>();
        if bytes.len() < hdr_size {
            return None;
        }
        // Length lives at offset 4..8, little-endian per the ACPI spec.
        let length = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if length != bytes.len() || length < hdr_size {
            return None;
        }
        if checksum(bytes) != 0 {
            return None;
        }
        Some(Self { bytes })
    }

    /// Copy out the SDT header. By value because the struct is packed: the
    /// reads happen here, aligned, so callers never reference into it.
    pub fn header(&self) -> SdtHeader {
        let b = self.bytes;
        let mut signature = [0u8; 4];
        signature.copy_from_slice(&b[0..4]);
        let length = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let revision = b[8];
        let csum = b[9];
        let mut oem_id = [0u8; 6];
        oem_id.copy_from_slice(&b[10..16]);
        let mut oem_table_id = [0u8; 8];
        oem_table_id.copy_from_slice(&b[16..24]);
        let oem_revision = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        let creator_id = u32::from_le_bytes([b[28], b[29], b[30], b[31]]);
        let creator_revision = u32::from_le_bytes([b[32], b[33], b[34], b[35]]);
        SdtHeader {
            signature,
            length,
            revision,
            checksum: csum,
            oem_id,
            oem_table_id,
            oem_revision,
            creator_id,
            creator_revision,
        }
    }

    #[inline]
    pub fn signature(&self) -> [u8; 4] {
        let b = self.bytes;
        [b[0], b[1], b[2], b[3]]
    }

    #[inline]
    pub fn length(&self) -> u32 {
        let b = self.bytes;
        u32::from_le_bytes([b[4], b[5], b[6], b[7]])
    }

    #[inline]
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[core::mem::size_of::<SdtHeader>()..]
    }

    #[inline]
    pub fn raw(&self) -> &'a [u8] {
        self.bytes
    }
}

/// 8-bit additive ACPI checksum: the byte sum of a valid table is zero.
#[inline]
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}
