//! GDT entry walker for kernel tests.
//!
//! Folds the `unsafe { *((base + offset) as *const u64) }` idiom in
//! `boot/src/tests/gdt_tests.rs` into safe `pub fn` helpers. Each entry
//! is a 64-bit raw descriptor for code / data / system selectors, or a
//! 128-bit pair for TSS / LDT descriptors.

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
    // SAFETY: caller upholds the precondition that `addr` lies inside
    // the GDT and the page is readable; descriptors are 8-byte aligned
    // and the kernel never invalidates the mapping during tests.
    unsafe { *(addr as *const u64) }
}

/// Read the 16-byte TSS descriptor pair at `index * 8` from `base` and
/// extract the 64-bit TSS base address.
///
/// TSS descriptors are double-wide: low 8 bytes carry the limit + low
/// 32 bits of base + access bits; high 8 bytes carry the upper 32 bits
/// of base.
///
/// Returns `(base, limit)` of the TSS structure pointed at by the GDT
/// entry.
#[inline]
pub fn read_tss_descriptor(gdt_base: u64, tss_index: usize) -> (u64, u32) {
    let lo = read_entry(gdt_base, tss_index);
    let hi = read_entry(gdt_base, tss_index + 1);

    // Limit is bits [0..16] of lo, with bits [16..20] of the limit
    // stored in bits [48..52] of lo. SlopOS' TSS limit fits in 16 bits.
    let limit_lo = (lo & 0xFFFF) as u32;
    let limit_hi = ((lo >> 48) & 0x0F) as u32;
    let limit = (limit_hi << 16) | limit_lo;

    // Base layout: bits [16..40] in lo, bits [56..64] in lo, bits
    // [0..32] in hi.
    let base_0_15 = (lo >> 16) & 0xFFFF;
    let base_16_23 = (lo >> 32) & 0xFF;
    let base_24_31 = (lo >> 56) & 0xFF;
    let base_32_63 = hi & 0xFFFF_FFFF;
    let base = base_0_15 | (base_16_23 << 16) | (base_24_31 << 24) | (base_32_63 << 32);

    (base, limit)
}

/// Read up to `len` bytes starting at virtual address `addr` and return
/// them as an owned fixed-size array. The kernel boot tests use this to
/// snapshot a few bytes of the `lstar` (syscall entry) trampoline for
/// instruction-prefix verification.
#[inline]
pub fn read_bytes_at<const N: usize>(addr: u64) -> [u8; N] {
    let mut buf = [0u8; N];
    let ptr = addr as *const u8;
    for i in 0..N {
        // SAFETY: caller upholds that `addr..addr+N` is mapped and
        // readable. Used for kernel-text byte sequences only.
        buf[i] = unsafe { ptr.add(i).read() };
    }
    buf
}
