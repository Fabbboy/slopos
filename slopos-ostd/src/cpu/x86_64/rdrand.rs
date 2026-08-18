//! Hardware random number generation via RDRAND and RDSEED instructions.
//!
//! RDRAND draws from the CPU's conditioned DRBG, RDSEED from the raw hardware
//! entropy source. Both are emulated by QEMU using the host's `/dev/urandom`.

use core::arch::asm;

use crate::arch::x86_64::cpuid::{
    CPUID_FEAT_ECX_RDRAND, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_RDSEED,
    cpuid, cpuid_count,
};

/// CPUID.1:ECX bit 30.
#[inline]
pub fn has_rdrand() -> bool {
    let (_, _, ecx, _) = cpuid(CPUID_LEAF_FEATURES);
    (ecx & CPUID_FEAT_ECX_RDRAND) != 0
}

/// CPUID.7.0:EBX bit 18.
#[inline]
pub fn has_rdseed() -> bool {
    let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_STRUCTURED_EXT, 0);
    (ebx & CPUID_SEXT_EBX_RDSEED) != 0
}

/// Witness that CPUID reports RDRAND on this CPU. [`RdRand::probe`] is the
/// only way to get one, so a caller cannot reach the instruction unprobed.
#[derive(Clone, Copy)]
pub struct RdRand(());

impl RdRand {
    #[inline]
    pub fn probe() -> Option<Self> {
        has_rdrand().then_some(Self(()))
    }

    /// A 64-bit random value, or `None` if the DRBG underflowed ten times.
    #[inline]
    pub fn next(self) -> Option<u64> {
        rdrand64_raw()
    }
}

/// Witness that CPUID reports RDSEED on this CPU.
#[derive(Clone, Copy)]
pub struct RdSeed(());

impl RdSeed {
    #[inline]
    pub fn probe() -> Option<Self> {
        has_rdseed().then_some(Self(()))
    }

    /// A 64-bit entropy value, or `None` after ten failed attempts.
    #[inline]
    pub fn next(self) -> Option<u64> {
        rdseed64_raw()
    }
}

/// Ten retries per Intel's recommendation; CF=0 means the DRBG underflowed.
#[inline]
fn rdrand64_raw() -> Option<u64> {
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        unsafe {
            asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

#[inline]
fn rdseed64_raw() -> Option<u64> {
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        unsafe {
            asm!(
                "rdseed {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}
