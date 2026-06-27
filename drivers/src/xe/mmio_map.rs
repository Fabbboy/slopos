//! BAR0 (Intel GTTMMADR) mapping for the xe driver.
//!
//! BAR0 is the 64-bit GTTMMADR window: display registers in the low half, the
//! Global GTT page-table in the high half (16 MiB total on the target silicon).
//! The probe maps the whole BAR and reads through the returned [`MmioRegion`]
//! handle. The handle is cloned to owned storage because its kernel VA persists
//! for the lifetime of the binding's devres bag, so a clone is a stable working
//! copy that outlives the borrow handed back by `map_bar`.

use slopos_mm::mmio::MmioRegion;

use crate::driver_core::bound::{BoundDevice, BoundError};
use crate::xe_logic::regs;

/// Map BAR0 (GTTMMADR) and return an owned handle over the full register window.
///
/// Validates that BAR0 is a usable memory window — present (non-zero base),
/// non-empty (non-zero size), and not I/O-mapped — then maps exactly the
/// firmware-reported BAR length and clones the borrowed handle to owned storage.
/// Any failure surfaces as a [`BoundError`]; nothing is written.
pub fn map_gttmmadr(bound: &mut BoundDevice<'_>) -> Result<MmioRegion, BoundError> {
    let bar = *bound.info().bars.first().ok_or(BoundError::NoSuchBar)?;
    if bar.is_io != 0 || bar.base == 0 || bar.size == 0 {
        return Err(BoundError::NoSuchBar);
    }
    let region = bound.map_bar(0, 0, bar.size as usize)?;
    Ok(region.clone())
}

/// Carve the Global GTT bank out of a mapped GTTMMADR window.
///
/// The GGTT page-table occupies `[GTTMMADR_GGTT_OFFSET, region.size())` — the
/// high half of BAR0. Sub-regioning shares the parent mapping (no new
/// reservation) and writes nothing; later phases use the returned handle to read
/// or program PTEs. Returns `None` if the window is too small to contain the
/// bank.
pub fn ggtt_bank(region: &MmioRegion) -> Option<MmioRegion> {
    let bank_len = region.size().checked_sub(regs::GTTMMADR_GGTT_OFFSET)?;
    region.sub_region(regs::GTTMMADR_GGTT_OFFSET, bank_len)
}
