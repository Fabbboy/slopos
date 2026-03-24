//! Kernel font service — initialises the global glyph atlas with a VGA
//! bitmap fallback and provides RCU-protected access via [`AtlasGuard`].
//!
//! At boot, the console starts with the embedded 8×16 VGA bitmap font.
//! Userspace can upgrade it at runtime via `SYS_FONT_SET` (coverage or
//! bitmap format); the vconsole is notified and resizes automatically.

use slopos_font::atlas::AtlasGuard;
use slopos_utils::{klog_info, klog_warn};

/// Initialise the kernel font subsystem.
///
/// Must be called after the heap allocator is available but before any
/// framebuffer text is rendered (splash screen, vconsole, etc.).
/// Boots with the VGA 8×16 bitmap font; userspace upgrades later.
pub fn init() {
    if slopos_font::atlas::init_global_bitmap() {
        if let Some(a) = atlas() {
            klog_info!(
                "Font atlas ready: {}x{} cells (VGA bitmap, source={:?})",
                a.cell_width(),
                a.cell_height(),
                a.source(),
            );
        }
    } else {
        klog_warn!("Failed to initialise font atlas");
    }
}

/// Acquire the global glyph atlas under an RCU read lock.
///
/// Returns `None` before [`init`] is called or if initialisation failed.
/// The guard holds an RCU read-side critical section — drop promptly.
#[inline]
pub fn atlas() -> Option<AtlasGuard> {
    slopos_font::atlas::global()
}
