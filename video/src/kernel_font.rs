use slopos_utils::{klog_info, klog_warn};

pub fn init() {
    if slopos_font::atlas::init_global_bitmap() {
        if let Some(atlas) = slopos_font::atlas::global() {
            klog_info!(
                "Font atlas ready: {}x{} cells (VGA bitmap, source={:?})",
                atlas.cell_width(),
                atlas.cell_height(),
                atlas.source(),
            );
        }
    } else {
        klog_warn!("Failed to initialise font atlas");
    }
}

#[inline]
pub fn atlas() -> Option<slopos_font::atlas::AtlasRef> {
    slopos_font::atlas::global()
}
