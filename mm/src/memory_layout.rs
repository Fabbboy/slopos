use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::klog_debug;
use slopos_ostd::sync::InitFlag;

use crate::symbols;

static KERNEL_IMAGE_START: AtomicU64 = AtomicU64::new(0);
static KERNEL_IMAGE_END: AtomicU64 = AtomicU64::new(0);
static BOUNDS_INIT: InitFlag = InitFlag::new();

/// Resolve linker-provided `_kernel_start` / `_kernel_end` and cache their
/// addresses. Idempotent — only the first call has an effect.
///
/// The cached values are linker symbol addresses and may be virtual; callers
/// needing physical addresses must translate via `virt_to_phys_kernel()`.
pub fn init_kernel_bounds() {
    if !BOUNDS_INIT.init_once() {
        return;
    }

    let (start, end) = symbols::kernel_bounds();
    KERNEL_IMAGE_START.store(start as usize as u64, Ordering::Release);
    KERNEL_IMAGE_END.store(end as usize as u64, Ordering::Release);

    klog_debug!(
        "kernel image bounds: {:#x}..{:#x}",
        start as u64,
        end as u64
    );
}

/// Returns `(start, end)` of the kernel image, or `(0, 0)` if
/// [`init_kernel_bounds`] has not been called yet.
pub fn kernel_image_bounds() -> (u64, u64) {
    if !BOUNDS_INIT.is_set() {
        return (0, 0);
    }
    (
        KERNEL_IMAGE_START.load(Ordering::Acquire),
        KERNEL_IMAGE_END.load(Ordering::Acquire),
    )
}
