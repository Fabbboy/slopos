//! ACPI platform-bus building blocks: `_CRS` small-descriptor parsing
//! (`IO`/`FixedIO`/`IRQ`), IAPC_BOOT_ARCH 8042 detection, and EISA id packing.
//!
//! The byte fixtures mirror what the Lenovo keyboard's `_CRS` and the platform
//! FADT actually contain.

use slopos_acpi::aml::eisa_pack;
use slopos_acpi::aml::resource::{parse_io_ports, parse_irqs};
use slopos_acpi::fadt::Fadt;
use slopos_testing::{TestResult, fail, pass};

const KBD_CRS: &[u8] = &[
    0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01, // IO Port: base 0x60, len 1
    0x47, 0x01, 0x64, 0x00, 0x64, 0x00, 0x01, 0x01, // IO Port: base 0x64, len 1
    0x23, 0x02, 0x00, 0x01, // IRQ descriptor: mask 0x0002 (IRQ 1), info edge/active-high
    0x79, 0x00, // End tag + checksum
];

pub fn test_crs_io_ports() -> TestResult {
    let io = parse_io_ports(KBD_CRS);
    if io.len() != 2 {
        return fail!("expected 2 IO windows, got {}", io.len());
    }
    if io[0].base != 0x60 || io[0].len != 1 {
        return fail!("IO[0] = {:?}", io[0]);
    }
    if io[1].base != 0x64 || io[1].len != 1 {
        return fail!("IO[1] = {:?}", io[1]);
    }
    pass!()
}

pub fn test_crs_irq() -> TestResult {
    let irqs = parse_irqs(KBD_CRS);
    if irqs.len() != 1 {
        return fail!("expected 1 IRQ descriptor, got {}", irqs.len());
    }
    if irqs[0].first_line() != Some(1) {
        return fail!("expected IRQ line 1, got {:?}", irqs[0].first_line());
    }
    if !irqs[0].edge {
        return fail!("expected edge-triggered");
    }
    if irqs[0].active_low {
        return fail!("expected active-high");
    }
    pass!()
}

pub fn test_fixed_io_descriptor() -> TestResult {
    // FixedIO 0x60 len 1: tag 0x4B (type 9, len 3), base(2 LE), length(1).
    let crs: &[u8] = &[0x4B, 0x60, 0x00, 0x01, 0x79, 0x00];
    let io = parse_io_ports(crs);
    if io.len() != 1 || io[0].base != 0x60 || io[0].len != 1 {
        return fail!("FixedIO not parsed: {:?}", io.as_slice());
    }
    pass!()
}

pub fn test_eisa_pack_known_ids() -> TestResult {
    // PNP0C50 cross-checks against the existing EISAID_PNP0C50 constant.
    if eisa_pack(b"PNP0C50") != Some(0x500C_D041) {
        return fail!("PNP0C50 pack wrong: {:?}", eisa_pack(b"PNP0C50"));
    }
    if eisa_pack(b"PNP0303") != Some(0x0303_D041) {
        return fail!("PNP0303 pack wrong: {:?}", eisa_pack(b"PNP0303"));
    }
    if eisa_pack(b"BAD").is_some() {
        return fail!("a short id must not pack");
    }
    pass!()
}

pub fn test_fadt_has_8042() -> TestResult {
    // Minimal ACPI 2.0+ FADT (revision 3) with IAPC_BOOT_ARCH at offset 109.
    let mut fadt = [0u8; 276];
    fadt[8] = 3; // SDT header revision
    fadt[109] = 0x02; // IAPC_BOOT_ARCH bit 1 = 8042 present
    let parsed = match Fadt::parse(&fadt) {
        Some(f) => f,
        None => return fail!("FADT failed to parse"),
    };
    if !parsed.has_8042() {
        return fail!("8042 bit should be detected");
    }

    fadt[109] = 0x00;
    let cleared = match Fadt::parse(&fadt) {
        Some(f) => f,
        None => return fail!("FADT (cleared) failed to parse"),
    };
    if cleared.has_8042() {
        return fail!("8042 bit should be clear");
    }
    pass!()
}

slopos_testing::stest!(name = test_crs_io_ports, suite = platform_acpi);
slopos_testing::stest!(name = test_crs_irq, suite = platform_acpi);
slopos_testing::stest!(name = test_fixed_io_descriptor, suite = platform_acpi);
slopos_testing::stest!(name = test_eisa_pack_known_ids, suite = platform_acpi);
slopos_testing::stest!(name = test_fadt_has_8042, suite = platform_acpi);
