//! FADT (Fixed ACPI Description Table) parsing + DSDT `\_S5` decode.
//!
//! The FADT (signature `"FACP"`) describes the platform's fixed
//! power-management hardware: the PM1 control registers used to request
//! a sleep transition, the SMI command port that switches the firmware
//! into ACPI mode, and the reset register used to reboot. The S5
//! (soft-off) sleep-type values themselves live in the DSDT's `\_S5`
//! AML object, so a full power-off needs both tables.
//!
//! [`Fadt::parse`] and [`find_s5_sleep_types`] are pure functions over
//! `&[u8]`; [`PowerConfig::from_tables`] ties FADT + DSDT/SSDT together for
//! the kernel's shutdown/reboot code.

use slopos_ostd::util::packed_view::read_packed;

use crate::tables::{self, AcpiTables};

const FADT_SIGNATURE: &[u8; 4] = b"FACP";

// FADT field offsets (absolute, from the start of the table including the
// 36-byte SDT header). ACPI 6.x, Table 5.9.
const OFF_REVISION: usize = 8; // SDT header revision byte
const OFF_SMI_CMD: usize = 48;
const OFF_ACPI_ENABLE: usize = 52;
const OFF_PM1A_CNT_BLK: usize = 64;
const OFF_PM1B_CNT_BLK: usize = 68;
const OFF_DSDT: usize = 40;
/// IA-PC Boot Architecture Flags (`u16`, ACPI 2.0+; absent on ACPI 1.0 FADTs).
const OFF_IAPC_BOOT_ARCH: usize = 109;
const OFF_FLAGS: usize = 112;

/// IAPC_BOOT_ARCH bit 1: the platform has an i8042 (PS/2) controller.
const IAPC_BOOT_ARCH_8042: u16 = 1 << 1;
const OFF_RESET_REG: usize = 116; // 12-byte GAS
const OFF_RESET_VALUE: usize = 128;
const OFF_X_DSDT: usize = 140;
const OFF_X_PM1A_CNT_BLK: usize = 172; // 12-byte GAS
const OFF_X_PM1B_CNT_BLK: usize = 184; // 12-byte GAS

// Generic Address Structure (GAS) sub-field offsets.
const GAS_ADDRESS_SPACE: usize = 0;
const GAS_ADDRESS: usize = 4;
const GAS_LEN: usize = 12;

/// ACPI address-space id for I/O ports (GAS `AddressSpaceId == 1`).
pub const ACPI_ADDR_SPACE_IO: u8 = 1;

/// FADT flags bit 10: the RESET_REG / RESET_VALUE fields are supported.
const FADT_FLAG_RESET_REG_SUP: u32 = 1 << 10;

/// A decoded ACPI Generic Address Structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    /// `0` = System Memory, `1` = System I/O (see [`ACPI_ADDR_SPACE_IO`]).
    pub address_space_id: u8,
    /// Register / port address (an I/O port number when in I/O space).
    pub address: u64,
}

impl Gas {
    fn read(bytes: &[u8], off: usize) -> Option<Gas> {
        if off.checked_add(GAS_LEN)? > bytes.len() {
            return None;
        }
        Some(Gas {
            address_space_id: read_packed::<u8>(bytes, off + GAS_ADDRESS_SPACE)?,
            address: read_packed::<u64>(bytes, off + GAS_ADDRESS)?,
        })
    }
}

/// Power-management facts extracted from the FADT.
#[derive(Clone, Copy, Debug)]
pub struct Fadt {
    /// PM1a control register I/O port (`0` if absent).
    pub pm1a_cnt_port: u16,
    /// PM1b control register I/O port (`0` for single-block systems).
    pub pm1b_cnt_port: u16,
    /// SMI command port for the ACPI-enable handshake (`0` if none).
    pub smi_cmd: u32,
    /// Value written to `smi_cmd` to request ACPI mode.
    pub acpi_enable: u8,
    /// Reset register + value, present only when the FADT advertises
    /// `RESET_REG_SUP`.
    pub reset: Option<(Gas, u8)>,
    /// Physical address of the DSDT (`X_DSDT` preferred when present).
    pub dsdt_phys: u64,
    /// IA-PC Boot Architecture Flags (`0` on ACPI 1.0 FADTs that lack the field).
    pub iapc_boot_arch: u16,
}

impl Fadt {
    /// Extract power-management fields from a FADT byte slice. Pure: no
    /// HHDM / firmware access, so it is unit-testable over a synthetic
    /// table. Returns `None` only if the buffer is too short to hold the
    /// mandatory ACPI 1.0 fields.
    pub fn parse(bytes: &[u8]) -> Option<Fadt> {
        // The legacy 32-bit PM1 control block fields are mandatory back
        // to ACPI 1.0 (FADT length 116). Anything shorter is malformed.
        if bytes.len() < OFF_PM1B_CNT_BLK + 4 {
            return None;
        }

        let revision = read_packed::<u8>(bytes, OFF_REVISION)?;
        let smi_cmd = read_packed::<u32>(bytes, OFF_SMI_CMD)?;
        let acpi_enable = read_packed::<u8>(bytes, OFF_ACPI_ENABLE)?;

        // Prefer the 64-bit extended (`X_`) GAS control blocks when the
        // FADT is long enough to carry them and they describe an I/O-space
        // register with a non-zero address; otherwise fall back to the
        // legacy 32-bit port fields. PM1 control is I/O space on every PC.
        let pm1a_cnt_port = resolve_cnt_port(bytes, OFF_PM1A_CNT_BLK, OFF_X_PM1A_CNT_BLK)?;
        let pm1b_cnt_port = resolve_cnt_port(bytes, OFF_PM1B_CNT_BLK, OFF_X_PM1B_CNT_BLK)?;

        // DSDT: prefer X_DSDT (64-bit) on revision >= 2 when present.
        let dsdt32 = read_packed::<u32>(bytes, OFF_DSDT)? as u64;
        let dsdt_phys = if revision >= 2 && bytes.len() >= OFF_X_DSDT + 8 {
            let x = read_packed::<u64>(bytes, OFF_X_DSDT)?;
            if x != 0 { x } else { dsdt32 }
        } else {
            dsdt32
        };

        // Reset register: only when the table is long enough to hold it
        // and the flags advertise support.
        let reset = if bytes.len() >= OFF_RESET_VALUE + 1 {
            let flags = read_packed::<u32>(bytes, OFF_FLAGS)?;
            if flags & FADT_FLAG_RESET_REG_SUP != 0 {
                let reg = Gas::read(bytes, OFF_RESET_REG)?;
                let value = read_packed::<u8>(bytes, OFF_RESET_VALUE)?;
                if reg.address != 0 {
                    Some((reg, value))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // IA-PC Boot Architecture Flags: ACPI 2.0+ (revision >= 2) and the
        // table must be long enough to carry the field. Absent ⇒ 0 ("unknown").
        let iapc_boot_arch = if revision >= 2 && bytes.len() >= OFF_IAPC_BOOT_ARCH + 2 {
            read_packed::<u16>(bytes, OFF_IAPC_BOOT_ARCH).unwrap_or(0)
        } else {
            0
        };

        Some(Fadt {
            pm1a_cnt_port,
            pm1b_cnt_port,
            smi_cmd,
            acpi_enable,
            reset,
            dsdt_phys,
            iapc_boot_arch,
        })
    }

    /// Whether the FADT advertises an i8042 (PS/2) controller
    /// (IAPC_BOOT_ARCH bit 1). `false` on ACPI 1.0 FADTs that lack the field —
    /// callers should treat that as "unknown" and fall back to a DSDT node check.
    pub fn has_8042(&self) -> bool {
        self.iapc_boot_arch & IAPC_BOOT_ARCH_8042 != 0
    }
}

/// Resolve a PM1 control-register port, preferring the extended GAS
/// field when it names a non-zero I/O-space register. Returns the
/// 16-bit port (`0` meaning "absent"). Errors only on a truncated read.
fn resolve_cnt_port(bytes: &[u8], legacy_off: usize, x_off: usize) -> Option<u16> {
    if bytes.len() >= x_off + GAS_LEN {
        if let Some(gas) = Gas::read(bytes, x_off) {
            if gas.address_space_id == ACPI_ADDR_SPACE_IO
                && gas.address != 0
                && gas.address <= u16::MAX as u64
            {
                return Some(gas.address as u16);
            }
        }
    }
    let legacy = read_packed::<u32>(bytes, legacy_off)?;
    Some(if legacy <= u16::MAX as u32 {
        legacy as u16
    } else {
        0
    })
}

// AML opcodes used by the `\_S5` package scan.
const AML_NAME_OP: u8 = 0x08;
const AML_ROOT_CHAR: u8 = 0x5C; // '\'
const AML_PACKAGE_OP: u8 = 0x12;
const AML_BYTE_PREFIX: u8 = 0x0A;

/// Scan DSDT AML for the `\_S5` sleep package and decode `(SLP_TYPa,
/// SLP_TYPb)`. `aml` is the DSDT body *after* its 36-byte SDT header.
///
/// The encoding searched for is the canonical
/// `NameOp '_S5_' PackageOp <PkgLength> <NumElements> <elem0> <elem1>`
/// shape that every PC firmware emits (each element is either a small
/// integer opcode or a `BytePrefix`-tagged byte). Pure / no-`unsafe`,
/// so it is unit-tested directly. Returns `None` if no valid `\_S5` is
/// found.
pub fn find_s5_sleep_types(aml: &[u8]) -> Option<(u8, u8)> {
    let pattern = b"_S5_";
    let last = aml.len().checked_sub(pattern.len())?;
    let mut i = 0usize;
    while i <= last {
        if &aml[i..i + pattern.len()] == pattern {
            // Validate the name is introduced by a NameOp (optionally
            // through the root '\' prefix) and followed by a PackageOp.
            let name_ok = (i >= 1 && aml[i - 1] == AML_NAME_OP)
                || (i >= 2 && aml[i - 2] == AML_NAME_OP && aml[i - 1] == AML_ROOT_CHAR);
            let pkg_at = i + pattern.len();
            if name_ok && aml.get(pkg_at).copied() == Some(AML_PACKAGE_OP) {
                if let Some(types) = decode_s5_package(aml, pkg_at + 1) {
                    return Some(types);
                }
            }
        }
        i += 1;
    }
    None
}

/// Decode the package body following the `PackageOp`. `p` points at the
/// first PkgLength byte.
fn decode_s5_package(aml: &[u8], p: usize) -> Option<(u8, u8)> {
    let lead = *aml.get(p)?;
    // Top two bits of the lead byte give the count of trailing PkgLength
    // bytes (0..=3); skip the whole PkgLength field plus the 1-byte
    // NumElements that follows it.
    let extra = (lead >> 6) as usize;
    let mut cur = p.checked_add(extra + 2)?;
    let slp_a = read_aml_small_int(aml, &mut cur)?;
    // A few firmwares list only one element; reuse it for PM1b.
    let slp_b = read_aml_small_int(aml, &mut cur).unwrap_or(slp_a);
    Some((slp_a & 0x7, slp_b & 0x7))
}

/// Read one small AML integer at `*p`, advancing past it. Handles both a
/// bare integer opcode (e.g. `ZeroOp`/`OneOp`/small constant) and a
/// `BytePrefix`-tagged byte.
fn read_aml_small_int(aml: &[u8], p: &mut usize) -> Option<u8> {
    let mut b = *aml.get(*p)?;
    if b == AML_BYTE_PREFIX {
        *p += 1;
        b = *aml.get(*p)?;
    }
    *p += 1;
    Some(b)
}

/// Power-management configuration the kernel's shutdown/reboot paths
/// consume: FADT register facts plus the DSDT-derived S5 sleep types.
#[derive(Clone, Copy, Debug)]
pub struct PowerConfig {
    /// PM1a control register I/O port (`0` if unavailable).
    pub pm1a_cnt_port: u16,
    /// PM1b control register I/O port (`0` if absent).
    pub pm1b_cnt_port: u16,
    /// S5 sleep type for PM1a, decoded from the DSDT `\_S5` package.
    pub slp_typ_a: Option<u8>,
    /// S5 sleep type for PM1b.
    pub slp_typ_b: Option<u8>,
    /// SMI command port for the ACPI-enable handshake (`0` if none).
    pub smi_cmd: u32,
    /// Value written to `smi_cmd` to enter ACPI mode.
    pub acpi_enable: u8,
    /// Reset register + value when the FADT advertises `RESET_REG_SUP`.
    pub reset: Option<(Gas, u8)>,
}

impl PowerConfig {
    /// Build a [`PowerConfig`] from a validated ACPI table hierarchy:
    /// locate the FADT, parse it, then read the DSDT and decode `\_S5`.
    /// Returns `None` if the FADT is missing or malformed; an absent /
    /// unreadable DSDT degrades to `slp_typ_* == None` rather than
    /// failing, so the reset register is still usable for reboot.
    pub fn from_tables(tables: &AcpiTables) -> Option<PowerConfig> {
        let facp = tables.find_table(FADT_SIGNATURE)?;
        let fadt = Fadt::parse(facp.raw())?;

        let (slp_typ_a, slp_typ_b) = find_s5(tables, fadt.dsdt_phys)
            .map(|(a, b)| (Some(a), Some(b)))
            .unwrap_or((None, None));

        Some(PowerConfig {
            pm1a_cnt_port: fadt.pm1a_cnt_port,
            pm1b_cnt_port: fadt.pm1b_cnt_port,
            slp_typ_a,
            slp_typ_b,
            smi_cmd: fadt.smi_cmd,
            acpi_enable: fadt.acpi_enable,
            reset: fadt.reset,
        })
    }
}

/// Locate the `\_S5` sleep types, scanning the DSDT first and then every
/// SSDT. Real Intel UEFI laptops frequently define `\_S5` in an SSDT
/// rather than the DSDT, so a DSDT-only scan would silently miss it and
/// leave ACPI soft-off unable to run.
fn find_s5(tables: &AcpiTables, dsdt_phys: u64) -> Option<(u8, u8)> {
    if let Some(types) = read_table_s5(dsdt_phys) {
        return Some(types);
    }
    tables.find_map_raw(b"SSDT", |bytes| {
        let aml = bytes.get(core::mem::size_of::<crate::tables::SdtHeader>()..)?;
        find_s5_sleep_types(aml)
    })
}

/// Read the SDT at `phys` and decode its `\_S5` sleep types. The checksum
/// is intentionally *not* validated — some firmware ships a stale DSDT
/// checksum, and we only scan the AML body for `\_S5`.
fn read_table_s5(phys: u64) -> Option<(u8, u8)> {
    let bytes = tables::table_bytes_at(phys)?;
    let aml = bytes.get(core::mem::size_of::<crate::tables::SdtHeader>()..)?;
    find_s5_sleep_types(aml)
}
