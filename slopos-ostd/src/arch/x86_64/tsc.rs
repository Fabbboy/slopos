#[allow(unused_imports)]
use core::arch::asm;

/// Read the Time Stamp Counter via `RDTSC`.
///
/// On non-`target_os = "none"` builds (host integration tests, including
/// `cargo miri test`) the asm cannot execute; the host stub returns a
/// monotonically-increasing counter sufficient for any code that uses
/// `rdtsc` as a coarse clock (e.g. RCU stall detection).
#[inline(always)]
pub fn rdtsc() -> u64 {
    #[cfg(target_os = "none")]
    {
        let lo: u32;
        let hi: u32;
        unsafe {
            asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags)
            );
        }
        ((hi as u64) << 32) | (lo as u64)
    }
    #[cfg(not(target_os = "none"))]
    {
        use core::sync::atomic::{AtomicU64, Ordering};
        static MOCK_TSC: AtomicU64 = AtomicU64::new(0);
        MOCK_TSC.fetch_add(1, Ordering::Relaxed)
    }
}
