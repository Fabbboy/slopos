//! Control register access (CR0, CR2, CR3, CR4) with type-safe bitflags.
//!
//! Host builds (`cfg(not(target_os = "none"))`, including `cargo miri
//! test`) back each register with a static `AtomicU64` so reads observe
//! prior writes; the `mov cr*` / `stac` / `clac` / `xgetbv` / `xsetbv`
//! instructions are skipped.

use bitflags::bitflags;
#[allow(unused_imports)]
use core::arch::asm;
use core::marker::PhantomData;

#[cfg(not(target_os = "none"))]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(target_os = "none"))]
static MOCK_CR0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_os = "none"))]
static MOCK_CR3: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_os = "none"))]
static MOCK_CR4: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_os = "none"))]
static MOCK_XCR0: AtomicU64 = AtomicU64::new(0x1); // x87 bit is hardware-forced to 1.

bitflags! {
    /// Flags for the CR0 control register.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cr0Flags: u64 {
        /// Protected Mode Enable.
        const PE = 1 << 0;
        /// Monitor Coprocessor.
        const MP = 1 << 1;
        /// Emulate Coprocessor (force x87 #NE).
        const EM = 1 << 2;
        /// Task Switched (set by hardware on task switch).
        const TS = 1 << 3;
        /// Extension Type (hardwired to 1 on modern CPUs).
        const ET = 1 << 4;
        /// Numeric Error (enable native x87 FPU error reporting).
        const NE = 1 << 5;
        /// Write Protect (prevent supervisor writes to read-only pages).
        const WP = 1 << 16;
        /// Alignment Mask (enable alignment checking in ring 3).
        const AM = 1 << 18;
        /// Not Write-through (disables write-through for the entire cache).
        const NW = 1 << 29;
        /// Cache Disable.
        const CD = 1 << 30;
        /// Paging Enable.
        const PG = 1 << 31;
    }
}

#[inline(always)]
pub fn read_cr0() -> u64 {
    #[cfg(target_os = "none")]
    {
        let value: u64;
        unsafe {
            asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR0.load(Ordering::Relaxed)
    }
}

#[inline(always)]
pub fn read_cr0_flags() -> Cr0Flags {
    Cr0Flags::from_bits_truncate(read_cr0())
}

#[inline(always)]
pub fn write_cr0(value: u64) {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR0.store(value, Ordering::Relaxed);
    }
}

#[inline(always)]
pub fn write_cr0_flags(flags: Cr0Flags) {
    write_cr0(flags.bits());
}

#[inline(always)]
pub fn read_cr2() -> u64 {
    #[cfg(target_os = "none")]
    {
        let value: u64;
        unsafe {
            asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        // No #PF can be recorded by a host process; report 0.
        0
    }
}

#[inline(always)]
pub fn read_cr3() -> u64 {
    #[cfg(target_os = "none")]
    {
        let value: u64;
        unsafe {
            asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR3.load(Ordering::Relaxed)
    }
}

#[inline(always)]
pub fn write_cr3(value: u64) {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR3.store(value, Ordering::Relaxed);
    }
}

bitflags! {
    /// Flags for the CR4 control register.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Cr4Flags: u64 {
        /// Virtual-8086 Mode Extensions.
        const VME = 1 << 0;
        /// Protected-Mode Virtual Interrupts.
        const PVI = 1 << 1;
        /// Time Stamp Disable (restrict RDTSC to ring 0).
        const TSD = 1 << 2;
        /// Debugging Extensions.
        const DE = 1 << 3;
        /// Page Size Extensions (enable 4 MiB pages).
        const PSE = 1 << 4;
        /// Physical Address Extension.
        const PAE = 1 << 5;
        /// Machine Check Enable.
        const MCE = 1 << 6;
        /// Page Global Enable.
        const PGE = 1 << 7;
        /// Performance-Monitoring Counter Enable.
        const PCE = 1 << 8;
        /// OS support for FXSAVE/FXRSTOR.
        const OSFXSR = 1 << 9;
        /// OS support for unmasked SIMD floating-point exceptions.
        const OSXMMEXCPT = 1 << 10;
        /// User-Mode Instruction Prevention.
        const UMIP = 1 << 11;
        /// 57-bit Linear Addresses (5-level paging).
        const LA57 = 1 << 12;
        /// VMX Enable.
        const VMXE = 1 << 13;
        /// SMX Enable.
        const SMXE = 1 << 14;
        /// FSGSBASE instructions Enable.
        const FSGSBASE = 1 << 16;
        /// PCID Enable.
        const PCIDE = 1 << 17;
        /// XSAVE and Processor Extended States Enable.
        const OSXSAVE = 1 << 18;
        /// Supervisor Mode Execution Prevention.
        const SMEP = 1 << 20;
        /// Supervisor Mode Access Prevention.
        const SMAP = 1 << 21;
        /// Protection Key Enable.
        const PKE = 1 << 22;
    }
}

#[inline(always)]
pub fn read_cr4() -> u64 {
    #[cfg(target_os = "none")]
    {
        let value: u64;
        unsafe {
            asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR4.load(Ordering::Relaxed)
    }
}

#[inline(always)]
pub fn read_cr4_flags() -> Cr4Flags {
    Cr4Flags::from_bits_truncate(read_cr4())
}

#[inline(always)]
pub fn write_cr4(value: u64) {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_CR4.store(value, Ordering::Relaxed);
    }
}

#[inline(always)]
pub fn write_cr4_flags(flags: Cr4Flags) {
    write_cr4(flags.bits());
}

pub const CR0_PE: u64 = Cr0Flags::PE.bits();
pub const CR0_MP: u64 = Cr0Flags::MP.bits();
pub const CR0_EM: u64 = Cr0Flags::EM.bits();
pub const CR0_TS: u64 = Cr0Flags::TS.bits();
pub const CR0_ET: u64 = Cr0Flags::ET.bits();
pub const CR0_NE: u64 = Cr0Flags::NE.bits();
pub const CR0_WP: u64 = Cr0Flags::WP.bits();
pub const CR0_AM: u64 = Cr0Flags::AM.bits();
pub const CR0_NW: u64 = Cr0Flags::NW.bits();
pub const CR0_CD: u64 = Cr0Flags::CD.bits();
pub const CR0_PG: u64 = Cr0Flags::PG.bits();

pub const CR4_VME: u64 = Cr4Flags::VME.bits();
pub const CR4_PVI: u64 = Cr4Flags::PVI.bits();
pub const CR4_TSD: u64 = Cr4Flags::TSD.bits();
pub const CR4_DE: u64 = Cr4Flags::DE.bits();
pub const CR4_PSE: u64 = Cr4Flags::PSE.bits();
pub const CR4_PAE: u64 = Cr4Flags::PAE.bits();
pub const CR4_MCE: u64 = Cr4Flags::MCE.bits();
pub const CR4_PGE: u64 = Cr4Flags::PGE.bits();
pub const CR4_PCE: u64 = Cr4Flags::PCE.bits();
pub const CR4_OSFXSR: u64 = Cr4Flags::OSFXSR.bits();
pub const CR4_OSXMMEXCPT: u64 = Cr4Flags::OSXMMEXCPT.bits();
pub const CR4_UMIP: u64 = Cr4Flags::UMIP.bits();
pub const CR4_LA57: u64 = Cr4Flags::LA57.bits();
pub const CR4_VMXE: u64 = Cr4Flags::VMXE.bits();
pub const CR4_SMXE: u64 = Cr4Flags::SMXE.bits();
pub const CR4_FSGSBASE: u64 = Cr4Flags::FSGSBASE.bits();
pub const CR4_PCIDE: u64 = Cr4Flags::PCIDE.bits();
pub const CR4_OSXSAVE: u64 = Cr4Flags::OSXSAVE.bits();
pub const CR4_SMEP: u64 = Cr4Flags::SMEP.bits();
pub const CR4_SMAP: u64 = Cr4Flags::SMAP.bits();
pub const CR4_PKE: u64 = Cr4Flags::PKE.bits();

/// Set `RFLAGS.AC` via `STAC`, allowing supervisor-mode code to access
/// user pages while SMAP (CR4.SMAP) is enabled.
///
/// Wrap a user-memory access as tightly as possible:
///
/// ```ignore
/// stac();
/// // ... touch user page ...
/// clac();
/// ```
///
/// Hardware clears `AC` on exception entry and `iretq` restores it, so a
/// fault inside the window cannot leak SMAP-disabled state; the caller must
/// still pair every `stac` with a `clac` on the success path.
#[inline(always)]
pub fn stac() {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("stac", options(nomem, nostack));
    }
}

/// Clear `RFLAGS.AC` via `CLAC`. See [`stac`] for usage.
#[inline(always)]
pub fn clac() {
    #[cfg(target_os = "none")]
    unsafe {
        asm!("clac", options(nomem, nostack));
    }
}

bitflags! {
    /// Feature-enable bits for the Extended Control Register 0 (XCR0): the
    /// processor state components XSAVE/XRSTOR manage.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Xcr0Flags: u64 {
        /// x87 FPU state (always set — hardware enforces bit 0 = 1).
        const X87 = 1 << 0;
        /// SSE state: MXCSR + XMM0–XMM15.
        const SSE = 1 << 1;
        /// AVX state: upper halves of YMM0–YMM15.
        const AVX = 1 << 2;
        /// MPX bound registers (BND0–BND3).  Deprecated on recent CPUs.
        const BNDREG = 1 << 3;
        /// MPX bound configuration/status (BNDCFGU + BNDSTATUS).
        const BNDCSR = 1 << 4;
        /// AVX-512 opmask registers (k0–k7).
        const OPMASK = 1 << 5;
        /// AVX-512 upper 256 bits of ZMM0–ZMM15.
        const ZMM_HI256 = 1 << 6;
        /// AVX-512 full ZMM16–ZMM31.
        const HI16_ZMM = 1 << 7;
        /// Processor Trace state.
        const PT = 1 << 8;
        /// Protection Key Rights for User pages.
        const PKRU = 1 << 9;
    }
}

/// Witness that `CR4.OSXSAVE` is set on the current CPU.
///
/// `XGETBV`/`XSETBV` raise `#UD` when it is clear. Both mint paths establish
/// it for real: [`enable`](Osxsave::enable) sets it, [`probe`](Osxsave::probe)
/// observes it.
///
/// `!Send`/`!Sync`, because `CR4` is per-CPU: a witness taken on a CPU that
/// has enabled XSAVE says nothing about one that has not yet reached its
/// `enable_on_current_cpu`.
#[derive(Debug)]
pub struct Osxsave(PhantomData<*const ()>);

impl Osxsave {
    /// Set `CR4.OSXSAVE` on this CPU and witness it.
    #[inline]
    pub fn enable() -> Self {
        write_cr4(read_cr4() | Cr4Flags::OSXSAVE.bits());
        Self(PhantomData)
    }

    /// Witness the bit if it is already set on this CPU, else `None`.
    ///
    /// For code that must not change control-register state: conformance
    /// tests, diagnostics.
    #[inline]
    pub fn probe() -> Option<Self> {
        ((read_cr4() & Cr4Flags::OSXSAVE.bits()) != 0).then_some(Self(PhantomData))
    }
}

/// An XCR0 value this CPU will actually accept.
///
/// `XSETBV` raises `#GP` on a value with bit 0 clear or with any bit the CPU
/// does not report in `CPUID.0Dh:EAX|(EDX << 32)`; [`new`](Xcr0Mask::new)
/// checks both, so the obligation is discharged where the value is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Xcr0Mask(Xcr0Flags);

impl Xcr0Mask {
    /// `None` unless `flags` has `X87` set and names only CPUID-reported
    /// components.
    #[inline]
    pub fn new(flags: Xcr0Flags) -> Option<Self> {
        if !flags.contains(Xcr0Flags::X87) {
            return None;
        }
        let supported = crate::arch::x86_64::cpuid::XsaveFeatures::detect().xcr0_supported;
        (flags.bits() & !supported == 0).then_some(Self(flags))
    }

    #[inline]
    pub fn bits(self) -> u64 {
        self.0.bits()
    }

    #[inline]
    pub fn flags(self) -> Xcr0Flags {
        self.0
    }
}

/// Read the Extended Control Register 0 (XCR0) via `XGETBV`.
#[inline(always)]
pub fn xcr0_read(_osxsave: &Osxsave) -> u64 {
    #[cfg(target_os = "none")]
    {
        let lo: u32;
        let hi: u32;
        // ECX = 0 selects XCR0.
        unsafe {
            asm!(
                "xgetbv",
                in("ecx") 0u32,
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
        }
        ((hi as u64) << 32) | (lo as u64)
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_XCR0.load(Ordering::Relaxed)
    }
}

/// Read XCR0 and return typed [`Xcr0Flags`].
#[inline(always)]
pub fn xcr0_read_flags(osxsave: &Osxsave) -> Xcr0Flags {
    Xcr0Flags::from_bits_truncate(xcr0_read(osxsave))
}

/// Write the Extended Control Register 0 (XCR0) via `XSETBV`.
///
/// The two hardware conditions — `CR4.OSXSAVE` set, and a mask the CPU
/// accepts — are the two arguments.
#[inline(always)]
pub fn xcr0_write(_osxsave: &Osxsave, mask: Xcr0Mask) {
    let value = mask.bits();
    #[cfg(target_os = "none")]
    {
        let lo = value as u32;
        let hi = (value >> 32) as u32;
        // ECX = 0 selects XCR0.
        unsafe {
            asm!(
                "xsetbv",
                in("ecx") 0u32,
                in("eax") lo,
                in("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        MOCK_XCR0.store(value, Ordering::Relaxed);
    }
}

pub const XCR0_X87: u64 = Xcr0Flags::X87.bits();
pub const XCR0_SSE: u64 = Xcr0Flags::SSE.bits();
pub const XCR0_AVX: u64 = Xcr0Flags::AVX.bits();
pub const XCR0_BNDREG: u64 = Xcr0Flags::BNDREG.bits();
pub const XCR0_BNDCSR: u64 = Xcr0Flags::BNDCSR.bits();
pub const XCR0_OPMASK: u64 = Xcr0Flags::OPMASK.bits();
pub const XCR0_ZMM_HI256: u64 = Xcr0Flags::ZMM_HI256.bits();
pub const XCR0_HI16_ZMM: u64 = Xcr0Flags::HI16_ZMM.bits();
pub const XCR0_PT: u64 = Xcr0Flags::PT.bits();
pub const XCR0_PKRU: u64 = Xcr0Flags::PKRU.bits();
