//! Hardware random number generation via RDRAND and RDSEED instructions.
//!
//! RDRAND draws from the CPU's DRBG (available since Ivy Bridge, 2012).
//! RDSEED draws from the raw hardware entropy source (available since Broadwell, 2014).
//! Both are emulated by QEMU using the host's `/dev/urandom`.

use core::arch::asm;

use crate::arch::x86_64::cpuid::{
    CPUID_FEAT_ECX_RDRAND, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_RDSEED,
    cpuid, cpuid_count,
};

/// Check if the CPU supports the RDRAND instruction (CPUID.1:ECX bit 30).
#[inline]
pub fn has_rdrand() -> bool {
    let (_, _, ecx, _) = cpuid(CPUID_LEAF_FEATURES);
    (ecx & CPUID_FEAT_ECX_RDRAND) != 0
}

/// Check if the CPU supports the RDSEED instruction (CPUID.7.0:EBX bit 18).
#[inline]
pub fn has_rdseed() -> bool {
    let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_STRUCTURED_EXT, 0);
    (ebx & CPUID_SEXT_EBX_RDSEED) != 0
}

/// Execute RDRAND and return a 64-bit random value, or `None` on failure.
///
/// Retries up to 10 times per Intel's recommendation. Each attempt checks
/// the carry flag: CF=1 means valid output, CF=0 means underflow (retry).
///
/// # Safety
/// Caller must verify `has_rdrand()` before calling.
#[inline]
pub fn rdrand64() -> Option<u64> {
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

/// Execute RDSEED and return a 64-bit entropy value, or `None` on failure.
///
/// Retries up to 10 times. RDSEED can fail more often than RDRAND because
/// it draws from the raw entropy source rather than a conditioned DRBG.
///
/// # Safety
/// Caller must verify `has_rdseed()` before calling.
#[inline]
pub fn rdseed64() -> Option<u64> {
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
