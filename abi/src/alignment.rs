//! Alignment helpers (canonical — all other crates re-export or delegate here).

/// If `alignment` is zero the input is returned unchanged.
#[inline(always)]
pub const fn align_down_u64(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment == 0 || alignment.is_power_of_two());
    if alignment == 0 {
        return value;
    }
    value & !(alignment - 1)
}

/// If `alignment` is zero the input is returned unchanged.
/// Uses saturating arithmetic to prevent overflow.
#[inline(always)]
pub const fn align_up_u64(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment == 0 || alignment.is_power_of_two());
    if alignment == 0 {
        return value;
    }
    let adjusted = value.saturating_add(alignment - 1);
    adjusted & !(alignment - 1)
}

/// If `alignment` is zero the input is returned unchanged.
#[inline(always)]
pub const fn align_down_usize(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment == 0 || alignment.is_power_of_two());
    if alignment == 0 {
        return value;
    }
    value & !(alignment - 1)
}

/// If `alignment` is zero the input is returned unchanged.
/// Uses saturating arithmetic to prevent overflow.
#[inline(always)]
pub const fn align_up_usize(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment == 0 || alignment.is_power_of_two());
    if alignment == 0 {
        return value;
    }
    let adjusted = value.saturating_add(alignment - 1);
    adjusted & !(alignment - 1)
}
