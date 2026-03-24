use slopos_font::atlas::AtlasGuard;
use slopos_utils::{klog_info, klog_warn};

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

#[inline]
pub fn atlas() -> Option<AtlasGuard> {
    slopos_font::atlas::global()
}
