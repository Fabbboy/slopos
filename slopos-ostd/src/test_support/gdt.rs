//! GDT entry walker for kernel tests: safe reads of the raw descriptors,
//! 64-bit for code/data/system selectors and a 128-bit pair for TSS/LDT.

/// Read a single 8-byte GDT entry at byte offset `index * 8` from `base`.
///
/// # Preconditions
/// - `base` is the GDTR base obtained from
///   [`crate::test_support::arch::read_gdtr`] (or another authoritative
///   source),
/// - `index * 8` is within the GDTR `limit` reported alongside `base`,
/// - the GDT memory is mapped and readable from the calling CPU.
#[inline]
pub fn read_entry(base: u64, index: usize) -> u64 {
    let addr = base + (index as u64) * 8;
    // SAFETY: caller upholds that `addr` lies inside the readable GDT;
    // descriptors are 8-byte aligned.
    unsafe { *(addr as *const u64) }
}

/// Read the `(base, limit)` of the TSS the descriptor pair at `tss_index`
/// points at.
///
/// TSS descriptors are double-wide: low 8 bytes carry the limit, the low 32
/// bits of base and the access bits; high 8 bytes carry base bits 32..64.
#[inline]
pub fn read_tss_descriptor(gdt_base: u64, tss_index: usize) -> (u64, u32) {
    let lo = read_entry(gdt_base, tss_index);
    let hi = read_entry(gdt_base, tss_index + 1);

    // Limit: bits [0..16] of lo, top nibble at bits [48..52] of lo.
    let limit_lo = (lo & 0xFFFF) as u32;
    let limit_hi = ((lo >> 48) & 0x0F) as u32;
    let limit = (limit_hi << 16) | limit_lo;

    // Base: bits [16..40] and [56..64] of lo, then bits [0..32] of hi.
    let base_0_15 = (lo >> 16) & 0xFFFF;
    let base_16_23 = (lo >> 32) & 0xFF;
    let base_24_31 = (lo >> 56) & 0xFF;
    let base_32_63 = hi & 0xFFFF_FFFF;
    let base = base_0_15 | (base_16_23 << 16) | (base_24_31 << 24) | (base_32_63 << 32);

    (base, limit)
}

#[inline]
pub fn read_bytes_at<const N: usize>(addr: u64) -> [u8; N] {
    let mut buf = [0u8; N];
    let ptr = addr as *const u8;
    for i in 0..N {
        // SAFETY: caller upholds that `addr..addr+N` is mapped and readable.
        buf[i] = unsafe { ptr.add(i).read() };
    }
    buf
}
