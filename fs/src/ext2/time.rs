/// Seconds since the Unix epoch for an inode timestamp, or `0` when the
/// kernel has no wall clock (`CLOCK_REALTIME` unset because the bootloader
/// reported no boot date).
///
/// Zero is what ext2 itself means by "unset", so a boot without an RTC leaves
/// the field as every other implementation reads an unstamped one, rather than
/// claiming 1970.
pub fn now_unix() -> u32 {
    now_unix_opt().unwrap_or(0)
}

/// The wall clock, or `None` when the boot established none.
pub fn now_unix_opt() -> Option<u32> {
    slopos_kernel_services::clock::realtime_unix_secs()
}

/// Stamp a timestamp field only when the clock can answer, so a clockless boot
/// preserves whatever an earlier boot wrote instead of resetting it to zero.
pub fn stamp(field: &mut u32) {
    if let Some(now) = slopos_kernel_services::clock::realtime_unix_secs() {
        *field = now;
    }
}
