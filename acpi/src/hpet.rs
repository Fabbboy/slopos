//! HPET base address and timer-block capabilities from the ACPI `"HPET"`
//! table (IA-PC HPET Specification §3.2.4).

use slopos_ostd::klog_info;
use slopos_ostd::util::packed_view::read_packed;

use crate::tables::AcpiTables;

const HPET_SIGNATURE: &[u8; 4] = b"HPET";

/// Payload layout after the SDT header: block id (4), GAS (12), hpet_number
/// (1), minimum_tick (2), page_protection (1).
const HPET_PAYLOAD_LEN: usize = 20;

const HPET_OFF_BLOCK_ID: usize = 0;
const HPET_OFF_GAS_ADDRESS_SPACE: usize = 4;
const HPET_OFF_GAS_ADDRESS: usize = 8;
const HPET_OFF_HPET_NUMBER: usize = 16;
const HPET_OFF_MIN_TICK: usize = 17;

#[derive(Clone, Copy, Debug)]
pub struct HpetInfo {
    pub base_phys: u64,
    pub hpet_number: u8,
    pub num_comparators: u8,
    pub counter_64bit: bool,
    /// Minimum clock tick for periodic mode.
    pub minimum_tick: u16,
}

pub struct Hpet {
    info: HpetInfo,
}

impl Hpet {
    pub fn from_tables(tables: &AcpiTables) -> Option<Self> {
        let Some(table) = tables.find_table(HPET_SIGNATURE) else {
            klog_info!("ACPI: HPET table not found");
            return None;
        };
        let payload = table.payload();
        if payload.len() < HPET_PAYLOAD_LEN {
            klog_info!("ACPI: HPET table too short ({} bytes)", payload.len());
            return None;
        }

        let addr_space = read_packed::<u8>(payload, HPET_OFF_GAS_ADDRESS_SPACE)?;
        if addr_space != 0 {
            klog_info!(
                "ACPI: HPET base address in unsupported space ({}), expected memory (0)",
                addr_space
            );
            return None;
        }

        let base_phys = read_packed::<u64>(payload, HPET_OFF_GAS_ADDRESS)?;
        if base_phys == 0 {
            klog_info!("ACPI: HPET base address is zero");
            return None;
        }

        let block_id = read_packed::<u32>(payload, HPET_OFF_BLOCK_ID)?;
        let num_comparators = (((block_id >> 8) & 0x1F) as u8).wrapping_add(1);
        let counter_64bit = (block_id >> 13) & 1 != 0;
        let hpet_number = read_packed::<u8>(payload, HPET_OFF_HPET_NUMBER)?;
        let minimum_tick = read_packed::<u16>(payload, HPET_OFF_MIN_TICK)?;

        Some(Self {
            info: HpetInfo {
                base_phys,
                hpet_number,
                num_comparators,
                counter_64bit,
                minimum_tick,
            },
        })
    }

    #[inline]
    pub fn info(&self) -> HpetInfo {
        self.info
    }
}
