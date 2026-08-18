//! Safe wrappers around the FPU / SSE / AVX / XSAVE inline-asm sequences the
//! kernel's register-state tests drive.
//!
//! A caller may sequence `xmm_load_4` … kernel code … `xmm_read_4` and expect
//! the register file to survive in between: the kernel is built `+soft-float`
//! and never touches it (`check_kernel_softfloat.sh` enforces that), and plain
//! `asm!` is volatile so the blocks cannot be reordered or elided. Callers must
//! keep interrupts disabled across such a sequence — a context switch would
//! legitimately save and restore the whole register file underneath them.
//!
//! `+soft-float` also makes the `xmm_reg` operand class unavailable, so every
//! routine below routes payloads through memory with explicitly named
//! registers, whose instruction text the assembler accepts without the feature.

pub type Xmm128 = [u64; 2];

#[inline]
pub fn xmm0_roundtrip(pattern: Xmm128) -> Xmm128 {
    let mut readback: Xmm128 = [0; 2];
    // SAFETY: both buffers are owned and 16 bytes; the named XMM registers are
    // declared clobbered.
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

/// Load four 128-bit patterns into XMM0..XMM3.
#[inline]
pub fn xmm_load_4(patterns: &[Xmm128; 4]) {
    let pat = patterns.as_ptr().cast::<u8>();
    // SAFETY: reads 64 owned bytes and writes four named XMM registers.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{pat}]",
            "movdqu xmm1, [{pat} + 16]",
            "movdqu xmm2, [{pat} + 32]",
            "movdqu xmm3, [{pat} + 48]",
            pat = in(reg) pat,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
        );
    }
}

/// Read XMM0..XMM3 back out. Does not modify them.
#[inline]
pub fn xmm_read_4() -> [Xmm128; 4] {
    let mut readback: [Xmm128; 4] = [[0; 2]; 4];
    let rb = readback.as_mut_ptr().cast::<u8>();
    // SAFETY: writes 64 owned bytes; the `movdqu` stores read the named XMM
    // registers without altering them.
    unsafe {
        core::arch::asm!(
            "movdqu [{rb}], xmm0",
            "movdqu [{rb} + 16], xmm1",
            "movdqu [{rb} + 32], xmm2",
            "movdqu [{rb} + 48], xmm3",
            rb = in(reg) rb,
        );
    }
    readback
}

/// Zero XMM0..XMM3, so a later read proves a restore actually happened.
#[inline]
pub fn xmm_zero_4() {
    // SAFETY: writes four named XMM registers and nothing else.
    unsafe {
        core::arch::asm!(
            "xorps xmm0, xmm0",
            "xorps xmm1, xmm1",
            "xorps xmm2, xmm2",
            "xorps xmm3, xmm3",
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
        );
    }
}

/// Load eight 128-bit patterns into XMM0..XMM7.
#[inline]
pub fn xmm_load_8(patterns: &[Xmm128; 8]) {
    let pat = patterns.as_ptr().cast::<u8>();
    // SAFETY: reads 128 owned bytes and writes eight named XMM registers.
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
            pat = in(reg) pat,
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
}

/// Read XMM0..XMM7 back out. Does not modify them.
#[inline]
pub fn xmm_read_8() -> [Xmm128; 8] {
    let mut readback: [Xmm128; 8] = [[0; 2]; 8];
    let rb = readback.as_mut_ptr().cast::<u8>();
    // SAFETY: writes 128 owned bytes; stores do not alter the source registers.
    unsafe {
        core::arch::asm!(
            "movdqu [{rb}], xmm0",
            "movdqu [{rb} + 16], xmm1",
            "movdqu [{rb} + 32], xmm2",
            "movdqu [{rb} + 48], xmm3",
            "movdqu [{rb} + 64], xmm4",
            "movdqu [{rb} + 80], xmm5",
            "movdqu [{rb} + 96], xmm6",
            "movdqu [{rb} + 112], xmm7",
            rb = in(reg) rb,
        );
    }
    readback
}

/// Zero XMM0..XMM7.
#[inline]
pub fn xmm_zero_8() {
    // SAFETY: writes eight named XMM registers and nothing else.
    unsafe {
        core::arch::asm!(
            "xorps xmm0, xmm0",
            "xorps xmm1, xmm1",
            "xorps xmm2, xmm2",
            "xorps xmm3, xmm3",
            "xorps xmm4, xmm4",
            "xorps xmm5, xmm5",
            "xorps xmm6, xmm6",
            "xorps xmm7, xmm7",
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
}

/// Load two 256-bit patterns into YMM0..YMM1, laid out as YMM0-lower,
/// YMM0-upper, YMM1-lower, YMM1-upper. The upper halves are the ones `fxsave`
/// cannot represent. Requires AVX enabled in XCR0; the caller checks.
#[inline]
pub fn ymm_load_2(patterns: &[Xmm128; 4]) {
    let pat = patterns.as_ptr().cast::<u8>();
    // SAFETY: reads 64 owned bytes; writes YMM0/YMM1 and the XMM5 scratch used
    // to stage each upper half before `vinsertf128`.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{pat}]",
            "movdqu xmm5, [{pat} + 16]",
            "vinsertf128 ymm0, ymm0, xmm5, 1",
            "movdqu xmm1, [{pat} + 32]",
            "movdqu xmm5, [{pat} + 48]",
            "vinsertf128 ymm1, ymm1, xmm5, 1",
            pat = in(reg) pat,
            out("ymm0") _,
            out("ymm1") _,
            out("xmm5") _,
        );
    }
}

/// Read YMM0..YMM1 back out in the same lower/upper layout as
/// [`ymm_load_2`]. Does not modify them.
#[inline]
pub fn ymm_read_2() -> [Xmm128; 4] {
    let mut readback: [Xmm128; 4] = [[0; 2]; 4];
    let rb = readback.as_mut_ptr().cast::<u8>();
    // SAFETY: writes 64 owned bytes; the stores and `vextractf128` read the
    // named registers without altering them.
    unsafe {
        core::arch::asm!(
            "movdqu [{rb}], xmm0",
            "vextractf128 [{rb} + 16], ymm0, 1",
            "movdqu [{rb} + 32], xmm1",
            "vextractf128 [{rb} + 48], ymm1, 1",
            rb = in(reg) rb,
        );
    }
    readback
}

/// Zero YMM0..YMM1, upper halves included.
#[inline]
pub fn ymm_zero_2() {
    // SAFETY: writes two named YMM registers and nothing else.
    unsafe {
        core::arch::asm!(
            "vxorps ymm0, ymm0, ymm0",
            "vxorps ymm1, ymm1, ymm1",
            out("ymm0") _,
            out("ymm1") _,
        );
    }
}

/// `xsave64` the live register file into `buf`.
///
/// `buf` must be a 64-byte-aligned XSAVE area at least as large as the active
/// components require; `xcr0` is the feature mask.
#[inline]
pub fn xsave_to(buf: &mut [u8], xcr0: u64) {
    debug_assert!(buf.as_ptr() as usize % 64 == 0);
    let lo = xcr0 as u32;
    let hi = (xcr0 >> 32) as u32;
    let ptr = buf.as_mut_ptr();
    // SAFETY: `buf` is exclusively borrowed, 64-byte aligned (asserted) and
    // large enough by the caller's contract.
    unsafe {
        core::arch::asm!(
            "xsave64 [{buf}]",
            buf = in(reg) ptr,
            in("eax") lo,
            in("edx") hi,
            options(nostack),
        );
    }
}

/// `xrstor64` the register file from `buf`. Mirror of [`xsave_to`].
#[inline]
pub fn xrstor_from(buf: &[u8], xcr0: u64) {
    debug_assert!(buf.as_ptr() as usize % 64 == 0);
    let lo = xcr0 as u32;
    let hi = (xcr0 >> 32) as u32;
    let ptr = buf.as_ptr();
    // SAFETY: `buf` is borrowed for the call, 64-byte aligned (asserted) and a
    // valid XSAVE image by the caller's contract; `xrstor64` only reads it.
    unsafe {
        core::arch::asm!(
            "xrstor64 [{buf}]",
            buf = in(reg) ptr,
            in("eax") lo,
            in("edx") hi,
            clobber_abi("sysv64"),
            options(nostack, readonly),
        );
    }
}

#[inline]
pub fn sse_xsave_xrstor_roundtrip_4(
    patterns: &[Xmm128; 4],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 4] {
    xmm_load_4(patterns);
    xsave_to(buf, xcr0);
    xmm_zero_4();
    xrstor_from(buf, xcr0);
    xmm_read_4()
}

/// AVX YMM upper-half round-trip; `patterns` and the result use
/// [`ymm_load_2`]'s layout.
#[inline]
pub fn avx_xsave_xrstor_roundtrip_2ymm(
    patterns: &[Xmm128; 4],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 4] {
    ymm_load_2(patterns);
    xsave_to(buf, xcr0);
    ymm_zero_2();
    xrstor_from(buf, xcr0);
    ymm_read_2()
}

/// Eight-register SSE isolation round-trip (XMM0..XMM7).
#[inline]
pub fn sse_xsave_xrstor_roundtrip_8(
    patterns: &[Xmm128; 8],
    buf: &mut [u8],
    xcr0: u64,
) -> [Xmm128; 8] {
    xmm_load_8(patterns);
    xsave_to(buf, xcr0);
    xmm_zero_8();
    xrstor_from(buf, xcr0);
    xmm_read_8()
}
