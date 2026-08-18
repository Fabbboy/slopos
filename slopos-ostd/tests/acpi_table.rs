//! Host-side tests for `slopos_ostd::acpi`.

use slopos_ostd::acpi::{AcpiTable, RSDP_SIGNATURE, RSDP_V1_SIZE, Rsdp, SdtHeader};

/// Set byte `idx` so the 8-bit additive sum over `bytes` is zero.
fn fix_checksum(bytes: &mut [u8], idx: usize) {
    let sum: u8 = bytes
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .fold(0u8, |acc, (_, b)| acc.wrapping_add(*b));
    bytes[idx] = 0u8.wrapping_sub(sum);
}

fn synth_rsdp_v1() -> [u8; RSDP_V1_SIZE] {
    let mut b = [0u8; RSDP_V1_SIZE];
    b[0..8].copy_from_slice(&RSDP_SIGNATURE);
    b[9..15].copy_from_slice(b"SLPOS_");
    b[15] = 0; // revision
    b[16..20].copy_from_slice(&0xCAFE_BABEu32.to_le_bytes());
    fix_checksum(&mut b, 8);
    b
}

fn synth_rsdp_v2() -> [u8; 36] {
    let mut b = [0u8; 36];
    b[0..8].copy_from_slice(&RSDP_SIGNATURE);
    b[9..15].copy_from_slice(b"SLPOS_");
    b[15] = 2; // revision
    b[16..20].copy_from_slice(&0xCAFE_BABEu32.to_le_bytes());
    b[20..24].copy_from_slice(&36u32.to_le_bytes());
    b[24..32].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    // V1 checksum
    fix_checksum(&mut b[..20], 8);
    // extended checksum
    let sum: u8 = b
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 32)
        .fold(0u8, |acc, (_, x)| acc.wrapping_add(*x));
    b[32] = 0u8.wrapping_sub(sum);
    b
}

#[test]
fn rsdp_v1_validates_when_well_formed() {
    let bytes = synth_rsdp_v1();
    let r = Rsdp::validate(&bytes).expect("valid v1 rsdp");
    assert_eq!(r.revision, 0);
    let rsdt = r.rsdt_address;
    assert_eq!(rsdt, 0xCAFE_BABE);
}

#[test]
fn rsdp_rejects_bad_signature() {
    let mut bytes = synth_rsdp_v1();
    bytes[0] = b'X';
    fix_checksum(&mut bytes, 8);
    assert!(Rsdp::validate(&bytes).is_none());
}

#[test]
fn rsdp_rejects_bad_v1_checksum() {
    let mut bytes = synth_rsdp_v1();
    bytes[8] = bytes[8].wrapping_add(1);
    assert!(Rsdp::validate(&bytes).is_none());
}

#[test]
fn rsdp_v2_validates_when_well_formed() {
    let bytes = synth_rsdp_v2();
    let r = Rsdp::validate(&bytes).expect("valid v2 rsdp");
    assert_eq!(r.revision, 2);
    let xsdt = r.xsdt_address;
    assert_eq!(xsdt, 0x1122_3344_5566_7788);
}

#[test]
fn rsdp_v2_rejects_bad_extended_checksum() {
    let mut bytes = synth_rsdp_v2();
    bytes[32] = bytes[32].wrapping_add(1);
    assert!(Rsdp::validate(&bytes).is_none());
}

#[test]
fn rsdp_rejects_short_slice() {
    let bytes = [0u8; 8];
    assert!(Rsdp::validate(&bytes).is_none());
}

fn synth_sdt(signature: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let hdr_len = core::mem::size_of::<SdtHeader>();
    let total = hdr_len + payload.len();
    let mut b = vec![0u8; total];
    b[0..4].copy_from_slice(signature);
    b[4..8].copy_from_slice(&(total as u32).to_le_bytes());
    b[8] = 1; // revision
    // checksum at byte 9
    b[10..16].copy_from_slice(b"SLPOS_");
    b[16..24].copy_from_slice(b"SLOPTBL_");
    b[24..28].copy_from_slice(&1u32.to_le_bytes());
    b[28..32].copy_from_slice(&0x4242_4242u32.to_le_bytes());
    b[32..36].copy_from_slice(&1u32.to_le_bytes());
    b[hdr_len..].copy_from_slice(payload);
    fix_checksum(&mut b, 9);
    b
}

#[test]
fn acpi_table_accepts_synthetic_table() {
    let bytes = synth_sdt(b"APIC", b"hello-world");
    let t = AcpiTable::from_bytes(&bytes).expect("valid synthetic table");
    assert_eq!(t.signature(), *b"APIC");
    assert_eq!(t.length() as usize, bytes.len());
    assert_eq!(t.payload(), b"hello-world");
    let h = t.header();
    let sig = h.signature;
    assert_eq!(sig, *b"APIC");
    let creator = h.creator_id;
    assert_eq!(creator, 0x4242_4242);
}

#[test]
fn acpi_table_rejects_length_mismatch() {
    let mut bytes = synth_sdt(b"APIC", b"data");
    let bogus_len = (bytes.len() + 1) as u32;
    bytes[4..8].copy_from_slice(&bogus_len.to_le_bytes());
    fix_checksum(&mut bytes, 9);
    assert!(AcpiTable::from_bytes(&bytes).is_none());
}

#[test]
fn acpi_table_rejects_bad_checksum() {
    let mut bytes = synth_sdt(b"APIC", b"data");
    bytes[9] = bytes[9].wrapping_add(1);
    assert!(AcpiTable::from_bytes(&bytes).is_none());
}

#[test]
fn acpi_table_rejects_short_slice() {
    let bytes = [0u8; 16];
    assert!(AcpiTable::from_bytes(&bytes).is_none());
}

#[test]
fn acpi_table_payload_lifetime_matches_input() {
    let bytes = synth_sdt(b"MCFG", &[1, 2, 3, 4]);
    let t = AcpiTable::from_bytes(&bytes).unwrap();
    let payload = t.payload();
    assert_eq!(payload, [1, 2, 3, 4]);
    assert_eq!(t.raw().len(), bytes.len());
}
