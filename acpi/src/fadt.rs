//! FADT (`"FACP"`) power-management registers plus DSDT `\_S5` decode. The S5
//! sleep-type values live in AML, so a full power-off needs both tables.

use slopos_ostd::util::packed_view::read_packed;

use crate::tables::{self, AcpiTables};

const FADT_SIGNATURE: &[u8; 4] = b"FACP";

// Absolute offsets from the start of the table, i.e. including the 36-byte SDT
// header. ACPI 6.x, Table 5.9.
const OFF_REVISION: usize = 8;
const OFF_SMI_CMD: usize = 48;
const OFF_ACPI_ENABLE: usize = 52;
const OFF_PM1A_CNT_BLK: usize = 64;
const OFF_PM1B_CNT_BLK: usize = 68;
const OFF_DSDT: usize = 40;
/// `u16`, ACPI 2.0+; absent on ACPI 1.0 FADTs.
const OFF_IAPC_BOOT_ARCH: usize = 109;
const OFF_FLAGS: usize = 112;

/// Bit 1: the platform has an i8042 (PS/2) controller.
const IAPC_BOOT_ARCH_8042: u16 = 1 << 1;
const OFF_RESET_REG: usize = 116; // 12-byte GAS
const OFF_RESET_VALUE: usize = 128;
const OFF_X_DSDT: usize = 140;
const OFF_X_PM1A_CNT_BLK: usize = 172; // 12-byte GAS
const OFF_X_PM1B_CNT_BLK: usize = 184; // 12-byte GAS

const GAS_ADDRESS_SPACE: usize = 0;
const GAS_ADDRESS: usize = 4;
const GAS_LEN: usize = 12;

pub const ACPI_ADDR_SPACE_IO: u8 = 1;

/// FADT flags bit 10: `RESET_REG` / `RESET_VALUE` are supported.
const FADT_FLAG_RESET_REG_SUP: u32 = 1 << 10;

/// ACPI Generic Address Structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    /// `0` = System Memory, `1` = System I/O (see [`ACPI_ADDR_SPACE_IO`]).
    pub address_space_id: u8,
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

#[derive(Clone, Copy, Debug)]
pub struct Fadt {
    /// `0` if absent.
    pub pm1a_cnt_port: u16,
    /// `0` on single-block systems.
    pub pm1b_cnt_port: u16,
    /// ACPI-enable handshake port; `0` if none.
    pub smi_cmd: u32,
    /// Value written to `smi_cmd` to request ACPI mode.
    pub acpi_enable: u8,
    /// Present only when the FADT advertises `RESET_REG_SUP`.
    pub reset: Option<(Gas, u8)>,
    /// `X_DSDT` preferred when present.
    pub dsdt_phys: u64,
    /// IA-PC Boot Architecture Flags; `0` on ACPI 1.0 FADTs that lack the field.
    pub iapc_boot_arch: u16,
}

impl Fadt {
    /// `None` only when the buffer is too short for the mandatory ACPI 1.0
    /// fields.
    pub fn parse(bytes: &[u8]) -> Option<Fadt> {
        // The legacy 32-bit PM1 control blocks are mandatory back to ACPI 1.0.
        if bytes.len() < OFF_PM1B_CNT_BLK + 4 {
            return None;
        }

        let revision = read_packed::<u8>(bytes, OFF_REVISION)?;
        let smi_cmd = read_packed::<u32>(bytes, OFF_SMI_CMD)?;
        let acpi_enable = read_packed::<u8>(bytes, OFF_ACPI_ENABLE)?;

        // PM1 control is I/O space on every PC, so an `X_` GAS naming any other
        // space is ignored in favour of the legacy 32-bit port field.
        let pm1a_cnt_port = resolve_cnt_port(bytes, OFF_PM1A_CNT_BLK, OFF_X_PM1A_CNT_BLK)?;
        let pm1b_cnt_port = resolve_cnt_port(bytes, OFF_PM1B_CNT_BLK, OFF_X_PM1B_CNT_BLK)?;

        let dsdt32 = read_packed::<u32>(bytes, OFF_DSDT)? as u64;
        let dsdt_phys = if revision >= 2 && bytes.len() >= OFF_X_DSDT + 8 {
            let x = read_packed::<u64>(bytes, OFF_X_DSDT)?;
            if x != 0 { x } else { dsdt32 }
        } else {
            dsdt32
        };

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

        // Absent ⇒ 0, which callers must read as "unknown", not "no i8042".
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

    /// `false` on ACPI 1.0 FADTs that lack the field, so a `false` means
    /// "unknown" and callers must fall back to a DSDT node check.
    pub fn has_8042(&self) -> bool {
        self.iapc_boot_arch & IAPC_BOOT_ARCH_8042 != 0
    }
}

/// `0` means "absent"; `None` only on a truncated read.
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

const AML_NAME_OP: u8 = 0x08;
const AML_ROOT_CHAR: u8 = 0x5C; // '\'
const AML_PACKAGE_OP: u8 = 0x12;
const AML_BYTE_PREFIX: u8 = 0x0A;

/// `aml` is the table body *after* its 36-byte SDT header. Matches the
/// canonical `NameOp '_S5_' PackageOp <PkgLength> <NumElements> <e0> <e1>`.
pub fn find_s5_sleep_types(aml: &[u8]) -> Option<(u8, u8)> {
    let pattern = b"_S5_";
    let last = aml.len().checked_sub(pattern.len())?;
    let mut i = 0usize;
    while i <= last {
        if &aml[i..i + pattern.len()] == pattern {
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

/// `p` points at the first PkgLength byte.
fn decode_s5_package(aml: &[u8], p: usize) -> Option<(u8, u8)> {
    let lead = *aml.get(p)?;
    // The lead byte's top two bits count the trailing PkgLength bytes; skip
    // those plus the 1-byte NumElements that follows.
    let extra = (lead >> 6) as usize;
    let mut cur = p.checked_add(extra + 2)?;
    let slp_a = read_aml_small_int(aml, &mut cur)?;
    // A few firmwares list only one element; reuse it for PM1b.
    let slp_b = read_aml_small_int(aml, &mut cur).unwrap_or(slp_a);
    Some((slp_a & 0x7, slp_b & 0x7))
}

/// Handles both a bare integer opcode and a `BytePrefix`-tagged byte.
fn read_aml_small_int(aml: &[u8], p: &mut usize) -> Option<u8> {
    let mut b = *aml.get(*p)?;
    if b == AML_BYTE_PREFIX {
        *p += 1;
        b = *aml.get(*p)?;
    }
    *p += 1;
    Some(b)
}

/// FADT register facts plus the DSDT-derived S5 sleep types.
#[derive(Clone, Copy, Debug)]
pub struct PowerConfig {
    /// `0` if unavailable.
    pub pm1a_cnt_port: u16,
    /// `0` if absent.
    pub pm1b_cnt_port: u16,
    /// Decoded from the `\_S5` package.
    pub slp_typ_a: Option<u8>,
    pub slp_typ_b: Option<u8>,
    /// ACPI-enable handshake port; `0` if none.
    pub smi_cmd: u32,
    /// Value written to `smi_cmd` to enter ACPI mode.
    pub acpi_enable: u8,
    /// Present only when the FADT advertises `RESET_REG_SUP`.
    pub reset: Option<(Gas, u8)>,
}

impl PowerConfig {
    /// `None` if the FADT is missing or malformed; an unreadable DSDT degrades
    /// to `slp_typ_* == None` so the reset register stays usable for reboot.
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

/// Scans the DSDT then every SSDT: real Intel UEFI firmware frequently defines
/// `\_S5` in an SSDT, which a DSDT-only scan would silently miss.
fn find_s5(tables: &AcpiTables, dsdt_phys: u64) -> Option<(u8, u8)> {
    if let Some(types) = read_table_s5(dsdt_phys) {
        return Some(types);
    }
    tables.find_map_raw(b"SSDT", |bytes| {
        let aml = bytes.get(core::mem::size_of::<crate::tables::SdtHeader>()..)?;
        find_s5_sleep_types(aml)
    })
}

/// Checksum intentionally *not* validated: some firmware ships a stale DSDT
/// checksum, and only the AML body is scanned.
fn read_table_s5(phys: u64) -> Option<(u8, u8)> {
    let bytes = tables::table_bytes_at(phys)?;
    let aml = bytes.get(core::mem::size_of::<crate::tables::SdtHeader>()..)?;
    find_s5_sleep_types(aml)
}
