//! Kernel font service — initialises the global glyph atlas with a VGA bitmap
//! fallback and provides RCU-protected access via [`AtlasGuard`].
//!
//! Userspace upgrades the font at runtime via `SYS_FONT_SET`; the vconsole is
//! notified and resizes automatically.

use slopos_font::atlas::AtlasGuard;
use slopos_ostd::{klog_info, klog_warn};

/// Initialise the kernel font subsystem.
///
/// Must be called after the heap allocator is available but before any
/// framebuffer text is rendered.
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
/// `None` before [`init`] or if it failed. The guard holds an RCU read-side
/// critical section — drop promptly.
#[inline]
pub fn atlas() -> Option<AtlasGuard> {
    slopos_font::atlas::global()
}
