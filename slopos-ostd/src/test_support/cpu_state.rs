//! Safe wrappers around the FPU / SSE / AVX / XSAVE inline-asm
//! sequences used by `ktesting/src/{fpu_tests, xsave_tests}.rs`.
//!
//! Each `unsafe { core::arch::asm!(...) }` block in those tests is
//! folded into one safe `pub fn` here. The test files become a
//! sequence of `let readback = cpu_state::...(...);` calls.

/// 128-bit XMM register payload.
pub type Xmm128 = [u64; 2];

/// Load `pattern` into XMM0 via an intermediate XMM register, then
/// read XMM0 back. Round-trip verifies that the XMM register file
/// preserves a 128-bit value across two `movdqa` hops.
///
/// The kernel is built `+soft-float` (no `sse` target feature), so the
/// `xmm_reg` operand class is unavailable here; this routes the payload
/// through memory with explicitly named registers, whose instruction
/// text the assembler accepts without the feature.
#[inline]
pub fn xmm0_roundtrip(pattern: Xmm128) -> Xmm128 {
    let mut readback: Xmm128 = [0; 2];
    // SAFETY: load 16 bytes from `pattern` into xmm1, hop to xmm0 and
    // back through xmm1, then store 16 bytes to `readback`. Both buffers
    // are owned, 16 bytes, and the named XMM registers are clobbered.
    unsafe {
        core::arch::asm!(
            "movdqu xmm1, [{src}]",
            "movdqa xmm0, xmm1",
            "movdqa xmm1, xmm0",
            "movdqu [{dst}], xmm1",
            src = in(reg) pattern.as_ptr(),
            dst = in(reg) readback.as_mut_ptr(),
            out("xmm0") _,
            out("xmm1") _,
        );
    }
    readback
}

/// Direct XMM1 round-trip: load `pattern` into XMM1, copy back into
/// `dst`, and store the result as `[u64; 2]`.
#[inline]
pub fn xmm1_roundtrip(pattern: Xmm128) -> Xmm128 {
    let mut readback: Xmm128 = [0; 2];
    // SAFETY: see xmm0_roundtrip.
    unsafe {
        core::arch::asm!(
            "movdqu xmm1, [{src}]",
            "movdqu [{dst}], xmm1",
            src = in(reg) pattern.as_ptr(),
            dst = in(reg) readback.as_mut_ptr(),
            out("xmm1") _,
        );
    }
    readback
}

/// Load `N` 128-bit patterns into XMM0..XMM(N-1), run xsave64,
/// zero those XMM registers, run xrstor64, and return the contents
/// re-read from XMM0..XMM(N-1) as an array of `[u64; 2]` slots.
///
/// `N` is 4 (canonical SSE roundtrip).
///
/// `xcr0` is the active XSAVE feature mask (passed to xsave/xrstor
/// in `edx:eax`). `buf` must be a 64-byte-aligned XSAVE area of at
/// least the size required by the active components.
#[inline]
pub fn sse_xsave_xrstor_roundtrip_4(
    patterns: &[Xmm128; 4],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 4] {
    debug_assert!(buf.as_ptr() as usize % 64 == 0);
    let mut readback: [Xmm128; 4] = [[0; 2]; 4];
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;
    let buf_ptr = buf.as_mut_ptr();
    let pat_ptr = patterns.as_ptr() as *const u8;
    let rb_ptr = readback.as_mut_ptr() as *mut u8;
    // SAFETY: pure CPU state save/restore round-trip with explicit
    // XCR0 feature mask. All buffers are exclusively owned by this
    // call and properly aligned.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{pat}]",
            "movdqu xmm1, [{pat} + 16]",
            "movdqu xmm2, [{pat} + 32]",
            "movdqu xmm3, [{pat} + 48]",
            "xsave64 [{buf}]",
            "xorps xmm0, xmm0",
            "xorps xmm1, xmm1",
            "xorps xmm2, xmm2",
            "xorps xmm3, xmm3",
            "xrstor64 [{buf}]",
            "movdqu [{rb}], xmm0",
            "movdqu [{rb} + 16], xmm1",
            "movdqu [{rb} + 32], xmm2",
            "movdqu [{rb} + 48], xmm3",
            buf = in(reg) buf_ptr,
            pat = in(reg) pat_ptr,
            rb = in(reg) rb_ptr,
            in("eax") xcr0_lo,
            in("edx") xcr0_hi,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
        );
    }
    readback
}

/// AVX YMM upper-half round-trip via XSAVE/XRSTOR. The `patterns`
/// array is interpreted as YMM0-lower, YMM0-upper, YMM1-lower,
/// YMM1-upper. Returns the same layout after the round-trip.
#[inline]
pub fn avx_xsave_xrstor_roundtrip_2ymm(
    patterns: &[Xmm128; 4],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 4] {
    debug_assert!(buf.as_ptr() as usize % 64 == 0);
    let mut readback: [Xmm128; 4] = [[0; 2]; 4];
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;
    let buf_ptr = buf.as_mut_ptr();
    let pat_ptr = patterns.as_ptr() as *const u8;
    let rb_ptr = readback.as_mut_ptr() as *mut u8;
    // SAFETY: see sse_xsave_xrstor_roundtrip_4; YMM variant uses
    // VINSERTF128 / VEXTRACTF128 to address the upper 128-bit halves.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{pat}]",
            "movdqu xmm5, [{pat} + 16]",
            "vinsertf128 ymm0, ymm0, xmm5, 1",
            "movdqu xmm1, [{pat} + 32]",
            "movdqu xmm5, [{pat} + 48]",
            "vinsertf128 ymm1, ymm1, xmm5, 1",
            "xsave64 [{buf}]",
            "vxorps ymm0, ymm0, ymm0",
            "vxorps ymm1, ymm1, ymm1",
            "xrstor64 [{buf}]",
            "movdqu [{rb}], xmm0",
            "vextractf128 [{rb} + 16], ymm0, 1",
            "movdqu [{rb} + 32], xmm1",
            "vextractf128 [{rb} + 48], ymm1, 1",
            buf = in(reg) buf_ptr,
            pat = in(reg) pat_ptr,
            rb = in(reg) rb_ptr,
            in("eax") xcr0_lo,
            in("edx") xcr0_hi,
            out("ymm0") _,
            out("ymm1") _,
            out("xmm5") _,
        );
    }
    readback
}

/// 8-register SSE isolation round-trip (XMM0..XMM7).
#[inline]
pub fn sse_xsave_xrstor_roundtrip_8(
    patterns: &[Xmm128; 8],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 8] {
    debug_assert!(buf.as_ptr() as usize % 64 == 0);
    let mut readback: [Xmm128; 8] = [[0; 2]; 8];
    let xcr0_lo = xcr0 as u32;
    let xcr0_hi = (xcr0 >> 32) as u32;
    let buf_ptr = buf.as_mut_ptr();
    let pat_ptr = patterns.as_ptr() as *const u8;
    let rb_ptr = readback.as_mut_ptr() as *mut u8;
    // SAFETY: see sse_xsave_xrstor_roundtrip_4 — same shape, 8 regs.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{pat}]",
            "movdqu xmm1, [{pat} + 16]",
            "movdqu xmm2, [{pat} + 32]",
            "movdqu xmm3, [{pat} + 48]",
            "movdqu xmm4, [{pat} + 64]",
            "movdqu xmm5, [{pat} + 80]",
            "movdqu xmm6, [{pat} + 96]",
            "movdqu xmm7, [{pat} + 112]",
            "xsave64 [{buf}]",
            "xorps xmm0, xmm0",
            "xorps xmm1, xmm1",
            "xorps xmm2, xmm2",
            "xorps xmm3, xmm3",
            "xorps xmm4, xmm4",
            "xorps xmm5, xmm5",
            "xorps xmm6, xmm6",
            "xorps xmm7, xmm7",
            "xrstor64 [{buf}]",
            "movdqu [{rb}], xmm0",
            "movdqu [{rb} + 16], xmm1",
            "movdqu [{rb} + 32], xmm2",
            "movdqu [{rb} + 48], xmm3",
            "movdqu [{rb} + 64], xmm4",
            "movdqu [{rb} + 80], xmm5",
            "movdqu [{rb} + 96], xmm6",
            "movdqu [{rb} + 112], xmm7",
            buf = in(reg) buf_ptr,
            pat = in(reg) pat_ptr,
            rb = in(reg) rb_ptr,
            in("eax") xcr0_lo,
            in("edx") xcr0_hi,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
            out("xmm4") _,
            out("xmm5") _,
            out("xmm6") _,
            out("xmm7") _,
        );
    }
    readback
}
