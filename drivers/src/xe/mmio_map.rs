//! BAR0 (Intel GTTMMADR) mapping for the xe driver: a 64-bit window with display
//! registers in the low half and the Global GTT page-table in the high half
//! (16 MiB on the target silicon).

use slopos_mm::mmio::MmioRegion;

use crate::driver_core::bound::{BoundDevice, BoundError};
use crate::xe_logic::regs;

/// Map BAR0 (GTTMMADR) and return an owned handle over the full register window.
///
/// The handle is cloned to owned storage: its kernel VA lives as long as the
/// binding's devres bag, so the clone outlives the borrow `map_bar` hands back.
pub fn map_gttmmadr(bound: &mut BoundDevice<'_>) -> Result<MmioRegion, BoundError> {
    let bar = *bound.info().bars.first().ok_or(BoundError::NoSuchBar)?;
    if bar.is_io != 0 || bar.base == 0 || bar.size == 0 {
        return Err(BoundError::NoSuchBar);
    }
    let region = bound.map_bar(0, 0, bar.size as usize)?;
    Ok(region.clone())
}

/// Carve the Global GTT bank — `[GTTMMADR_GGTT_OFFSET, region.size())` — out of a
/// mapped GTTMMADR window, sharing the parent mapping rather than reserving
/// anew. `None` if the window is too small to contain the bank.
pub fn ggtt_bank(region: &MmioRegion) -> Option<MmioRegion> {
    let bank_len = region.size().checked_sub(regs::GTTMMADR_GGTT_OFFSET)?;
    region.sub_region(regs::GTTMMADR_GGTT_OFFSET, bank_len)
}
