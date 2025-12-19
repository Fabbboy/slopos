/// Align `value` down to the nearest multiple of `alignment`.
/// If `alignment` is zero, the input is returned unchanged (matching the C helper).
#[inline(always)]
pub const fn align_down_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    value & !(alignment - 1)
}

/// Align `value` up to the nearest multiple of `alignment`.
/// If `alignment` is zero, the input is returned unchanged.
#[inline(always)]
pub const fn align_up_u64(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    // Saturating add avoids wraparound that the original C might allow.
    let adjusted = value.saturating_add(alignment - 1);
    adjusted & !(alignment - 1)
}
