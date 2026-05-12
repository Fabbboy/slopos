//! Host-side tests for `slopos_ostd::boot::handoff`.
//!
//! Each handoff fn (`acpi_handoff`, `framebuffer_handoff`,
//! `memmap_handoff`, `elf_image_handoff`) is exercised with a
//! synthetic byte slice or in-memory entry array. Tests that touch
//! the BSP-gated HHDM-offset registry serialise on a `Mutex` because
//! the registry is process-global and `cargo test` parallelises
//! `#[test]` items by default.

use core::ptr::NonNull;
use std::sync::Mutex;

use slopos_abi::addr::PhysAddr;
use slopos_ostd::boot::handoff::{
    ElfImage, Framebuffer, MemmapEntry, acpi_handoff, elf_image_handoff, framebuffer_handoff,
    memmap_handoff,
};
use slopos_ostd::boot::hhdm::{register_hhdm_offset, reset_hhdm_offset_for_tests};
use slopos_ostd::sync::{reset_bsp_token_for_tests, run_bsp_init};

/// Serialises every test that touches the BSP-gated HHDM-offset
/// registry. `cargo test` parallelises tests by default.
static BSP_LOCK: Mutex<()> = Mutex::new(());

// ----------------------------------------------------------------------------
// acpi_handoff
// ----------------------------------------------------------------------------

/// Build a synthetic ACPI table: 4-byte sig + 4-byte length + revision
/// + checksum byte + padding to `length`. The 8-bit additive checksum
/// of the full table is forced to zero.
fn build_synthetic_acpi_table(sig: [u8; 4], total_len: usize) -> Vec<u8> {
    assert!(total_len >= 36, "ACPI SDT header is 36 bytes");
    let mut table = vec![0u8; total_len];
    table[0..4].copy_from_slice(&sig);
    let length = total_len as u32;
    table[4..8].copy_from_slice(&length.to_le_bytes());
    table[8] = 1; // revision
    table[9] = 0; // checksum placeholder
    // OEM fields, etc. — leave zero.

    // Compute 8-bit additive checksum and patch byte 9 so the sum
    // is zero modulo 256.
    let sum: u8 = table.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    table[9] = (0u8).wrapping_sub(sum);
    table
}

#[test]
fn acpi_handoff_requires_hhdm_registration() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();

    // No HHDM offset registered yet → handoff returns None.
    let result = acpi_handoff(PhysAddr::new(0x1000), 36);
    assert!(result.is_none(), "must reject when HHDM unregistered");
}

#[test]
fn acpi_handoff_round_trips_checksum_validated_table() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();

    let table = build_synthetic_acpi_table(*b"APIC", 36);
    let table_ptr = table.as_ptr() as u64;
    // Choose offset so phys + offset = table_ptr. The kernel's HHDM
    // is `virt = phys + offset`, so we just register `table_ptr`
    // itself as the offset (phys 0 is then mapped at table_ptr).
    // Use phys = 0x1000 → virt = offset + 0x1000.
    let phys_base = 0x1000_u64;
    let offset = table_ptr.wrapping_sub(phys_base);

    run_bsp_init(|tok| register_hhdm_offset(tok, offset));

    let view = acpi_handoff(PhysAddr::new(phys_base), table.len())
        .expect("checksum-valid synthetic table must parse");
    assert_eq!(view.signature(), *b"APIC");
    assert_eq!(view.length() as usize, table.len());
}

#[test]
fn acpi_handoff_rejects_null_phys_or_zero_len() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();
    run_bsp_init(|tok| register_hhdm_offset(tok, 0));
    assert!(acpi_handoff(PhysAddr::new(0), 36).is_none());
    assert!(acpi_handoff(PhysAddr::new(0x1000), 0).is_none());
}

// ----------------------------------------------------------------------------
// framebuffer_handoff
// ----------------------------------------------------------------------------

#[test]
fn framebuffer_handoff_exposes_dimensions_and_byte_slice() {
    let mut backing: Vec<u8> = vec![0xAB; 64 * 16]; // 64 px × 16 rows
    let ptr = NonNull::new(backing.as_mut_ptr()).unwrap();
    let fb: Framebuffer = framebuffer_handoff(ptr, 64, 16);
    assert_eq!(fb.pitch(), 64);
    assert_eq!(fb.height(), 16);
    assert_eq!(fb.byte_size(), 64 * 16);
    let bytes = fb.as_bytes_mut();
    assert_eq!(bytes.len(), 64 * 16);
    assert!(bytes.iter().all(|b| *b == 0xAB));
    // Write back, observe through the view's slice.
    bytes[0] = 0xCD;
    let bytes2 = fb.as_bytes_mut();
    assert_eq!(bytes2[0], 0xCD);
}

// ----------------------------------------------------------------------------
// memmap_handoff
// ----------------------------------------------------------------------------

#[test]
fn memmap_handoff_borrows_entry_array() {
    // Use a Box::leak so the entries live for 'static — `memmap_handoff`
    // returns `&'static [MemmapEntry]` and dropping `entries` here
    // would dangle.
    let entries: &'static mut [MemmapEntry] = Box::leak(Box::new([
        MemmapEntry {
            base: 0x1000,
            length: 0x1000,
            typ: 1,
        },
        MemmapEntry {
            base: 0x2000,
            length: 0x2000,
            typ: 2,
        },
        MemmapEntry {
            base: 0x4000,
            length: 0x4000,
            typ: 3,
        },
    ]));
    let ptr = NonNull::new(entries.as_mut_ptr()).unwrap();

    let slice = memmap_handoff(ptr, 3);
    assert_eq!(slice.len(), 3);
    assert_eq!(slice[0].base, 0x1000);
    assert_eq!(slice[1].length, 0x2000);
    assert_eq!(slice[2].typ, 3);
}

// ----------------------------------------------------------------------------
// elf_image_handoff
// ----------------------------------------------------------------------------

#[test]
fn elf_image_handoff_accepts_valid_magic() {
    let bytes: &'static mut [u8] =
        Box::leak(vec![0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00].into_boxed_slice());
    let ptr = NonNull::new(bytes.as_mut_ptr()).unwrap();
    let image: ElfImage<'static> = elf_image_handoff(ptr, bytes.len()).expect("valid ELF magic");
    assert_eq!(image.magic(), Some([0x7F, b'E', b'L', b'F']));
    assert_eq!(image.len(), 8);
    assert!(!image.is_empty());
}

#[test]
fn elf_image_handoff_rejects_bad_magic() {
    let bytes: &'static mut [u8] = Box::leak(vec![0xDE, 0xAD, 0xBE, 0xEF].into_boxed_slice());
    let ptr = NonNull::new(bytes.as_mut_ptr()).unwrap();
    assert!(elf_image_handoff(ptr, bytes.len()).is_none());
}

#[test]
fn elf_image_handoff_rejects_short_payload() {
    let bytes: &'static mut [u8] = Box::leak(vec![0x7F, b'E', b'L'].into_boxed_slice());
    let ptr = NonNull::new(bytes.as_mut_ptr()).unwrap();
    assert!(elf_image_handoff(ptr, bytes.len()).is_none());
}
