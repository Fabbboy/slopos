use slopos_font::atlas::GlyphAtlas;
use slopos_utils::{klog_info, klog_warn};

/// Self-owning atlas guard for kernel-side use.
///
/// Combines an [`RcuReadGuard`] with a raw atlas pointer so that the
/// RCU read-side critical section is held for exactly as long as the
/// caller keeps this value alive.  Derefs to [`GlyphAtlas`] for
/// ergonomic rendering calls.
///
/// Obtain via [`atlas()`].
pub struct KernelAtlasGuard {
    _rcu: slopos_sync::RcuReadGuard,
    ptr: *const GlyphAtlas,
}

impl core::ops::Deref for KernelAtlasGuard {
    type Target = GlyphAtlas;

    #[inline]
    fn deref(&self) -> &GlyphAtlas {
        // SAFETY: ptr is non-null (checked by atlas()), and _rcu keeps
        // the RCU read-side critical section active so no writer can
        // free the atlas while this guard exists.
        unsafe { &*self.ptr }
    }
}

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
/// The returned guard holds the RCU read-side critical section open,
/// preventing any concurrent writer from freeing the atlas.  Drop the
/// guard as soon as rendering is complete to minimise the critical
/// section length.
#[inline]
pub fn atlas() -> Option<KernelAtlasGuard> {
    let rcu = slopos_sync::rcu_read_lock();
    let ptr = slopos_font::atlas::global_ptr();
    if ptr.is_null() {
        None
    } else {
        Some(KernelAtlasGuard { _rcu: rcu, ptr })
    }
}
