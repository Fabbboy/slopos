use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::{DisplayInfo, PixelFormat};
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_utils::{klog_debug, klog_warn};

const MIN_FRAMEBUFFER_WIDTH: u32 = 320;
const MIN_FRAMEBUFFER_HEIGHT: u32 = 240;
const MAX_BUFFER_SIZE: u32 = 64 * 1024 * 1024;

#[derive(Copy, Clone)]
pub(crate) struct FbState {
    pub(crate) base: VirtAddr,
    pub(crate) info: DisplayInfo,
}

impl FbState {
    #[inline]
    pub(crate) fn width(&self) -> u32 {
        self.info.width
    }

    #[inline]
    pub(crate) fn height(&self) -> u32 {
        self.info.height
    }

    #[inline]
    pub(crate) fn pitch(&self) -> u32 {
        self.info.pitch
    }

    #[inline]
    pub(crate) fn bpp(&self) -> u8 {
        self.info.bytes_per_pixel() * 8
    }

    #[inline]
    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base.as_mut_ptr()
    }

    #[inline]
    pub(crate) fn buffer_size(&self) -> usize {
        self.info.buffer_size() as usize
    }

    #[inline]
    fn checked_ptr(&self, offset: usize, len: usize) -> Option<*mut u8> {
        let end = offset.checked_add(len)?;
        if end > self.buffer_size() {
            return None;
        }
        let base = self.base_ptr();
        if base.is_null() {
            return None;
        }
        // SAFETY: offset and len were bounds-checked against framebuffer size above.
        Some(unsafe { base.add(offset) })
    }
}

struct FramebufferState {
    fb: Option<FbState>,
}

impl FramebufferState {
    const fn new() -> Self {
        Self { fb: None }
    }
}

static FRAMEBUFFER: SpinLock<FramebufferState> =
    SpinLock::new(FramebufferState::new(), LOCK_LEVEL_RESOURCE);
static FRAMEBUFFER_FLUSH: SpinLock<Option<fn() -> c_int>> =
    SpinLock::new(None, LOCK_LEVEL_RESOURCE);

fn init_state_from_raw(addr: u64, width: u32, height: u32, pitch: u32, bpp: u8) -> i32 {
    if addr == 0 || width < MIN_FRAMEBUFFER_WIDTH || width > DisplayInfo::MAX_DIMENSION {
        return -1;
    }
    if height < MIN_FRAMEBUFFER_HEIGHT || height > DisplayInfo::MAX_DIMENSION {
        return -1;
    }
    if bpp != 16 && bpp != 24 && bpp != 32 {
        return -1;
    }
    let _buffer_size = match pitch.checked_mul(height) {
        Some(sz) if sz > 0 && sz <= MAX_BUFFER_SIZE => sz,
        _ => return -1,
    };

    let mapped_base = if let Some(hhdm_base) = slopos_mm::hhdm::try_offset() {
        if addr >= hhdm_base {
            VirtAddr::try_new(addr).unwrap_or(VirtAddr::NULL)
        } else {
            PhysAddr::try_new(addr)
                .and_then(|phys| phys.to_virt_checked())
                .unwrap_or(VirtAddr::NULL)
        }
    } else {
        PhysAddr::try_new(addr)
            .and_then(|phys| phys.to_virt_checked())
            .unwrap_or(VirtAddr::NULL)
    };

    if mapped_base.is_null() {
        return -1;
    }

    let display_info = DisplayInfo::new(width, height, pitch, PixelFormat::from_bpp(bpp));

    let fb_state = FbState {
        base: mapped_base,
        info: display_info,
    };

    let mut guard = FRAMEBUFFER.lock();
    guard.fb = Some(fb_state);
    0
}

pub fn init_with_display_info(address: *mut u8, info: &DisplayInfo) -> i32 {
    let rc = init_state_from_raw(
        address as u64,
        info.width,
        info.height,
        info.pitch,
        info.bytes_per_pixel() * 8,
    );

    if rc == 0 {
        if let Some(fb) = FRAMEBUFFER.lock().fb {
            klog_debug!(
                "Framebuffer init: phys=0x{:x} virt=0x{:x} {}x{} pitch={} bpp={}",
                address as u64,
                fb.base.as_u64(),
                fb.width(),
                fb.height(),
                fb.pitch(),
                fb.bpp()
            );
        } else {
            klog_warn!("Framebuffer init: state missing after init");
        }
    } else {
        klog_warn!(
            "Framebuffer init failed: phys=0x{:x} {}x{} pitch={} bpp={}",
            address as u64,
            info.width,
            info.height,
            info.pitch,
            info.bytes_per_pixel() * 8
        );
    }

    rc
}
pub fn get_display_info() -> Option<DisplayInfo> {
    FRAMEBUFFER.lock().fb.map(|fb| fb.info)
}

pub fn get_fb_base_ptr() -> Option<*mut u8> {
    FRAMEBUFFER.lock().fb.map(|fb| fb.base_ptr())
}

pub(crate) fn snapshot() -> Option<FbState> {
    FRAMEBUFFER.lock().fb
}

pub fn register_flush_callback(callback: fn() -> c_int) {
    let mut guard = FRAMEBUFFER_FLUSH.lock();
    *guard = Some(callback);
}

pub fn framebuffer_flush() -> c_int {
    let guard = FRAMEBUFFER_FLUSH.lock();
    if let Some(cb) = *guard { cb() } else { 0 }
}

fn copy_rect_from_shm(
    fb: &FbState,
    shm_virt: *const u8,
    shm_size: usize,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
) -> bool {
    let fb_width = fb.width() as i32;
    let fb_height = fb.height() as i32;
    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let fb_pitch = fb.pitch() as usize;

    let cx0 = x0.max(0);
    let cy0 = y0.max(0);
    let cx1 = x1.min(fb_width - 1);
    let cy1 = y1.min(fb_height - 1);
    if cx0 > cx1 || cy0 > cy1 {
        return true;
    }

    let row_bytes = (cx1 - cx0 + 1) as usize * bytes_pp;
    for row in cy0..=cy1 {
        let row_usize = row as usize;
        let src_off = row_usize
            .checked_mul(fb_pitch)
            .and_then(|v| v.checked_add(cx0 as usize * bytes_pp));
        let dst_off = src_off;
        let Some(src_off) = src_off else {
            return false;
        };
        let Some(dst_off) = dst_off else {
            return false;
        };

        let Some(src_end) = src_off.checked_add(row_bytes) else {
            return false;
        };
        if src_end > shm_size {
            return false;
        }

        let Some(dst_ptr) = fb.checked_ptr(dst_off, row_bytes) else {
            return false;
        };
        // SAFETY: src range is checked against shm_size, dst range checked by checked_ptr.
        unsafe {
            ptr::copy_nonoverlapping(shm_virt.add(src_off), dst_ptr, row_bytes);
        }
    }

    true
}

/// Release compositor's framebuffer ownership, restoring the vconsole.
/// Called when the compositor task dies (crash recovery path).
pub fn release_compositor_fb() {
    COMPOSITOR_FB_ACQUIRED.store(false, core::sync::atomic::Ordering::Relaxed);
    slopos_drivers::tty::vconsole::compositor_release_fb();
}

static COMPOSITOR_FB_ACQUIRED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn fb_flip_from_shm(shm_phys: PhysAddr, size: usize) -> c_int {
    fb_flip_from_shm_damage(shm_phys, size, core::ptr::null(), 0)
}

pub fn fb_flip_from_shm_damage(
    shm_phys: PhysAddr,
    size: usize,
    damage: *const slopos_abi::damage::DamageRect,
    damage_count: u32,
) -> c_int {
    // On first compositor flip, take framebuffer ownership from the vconsole.
    if !COMPOSITOR_FB_ACQUIRED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        slopos_drivers::tty::vconsole::compositor_acquire_fb();
    }

    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return -1,
    };

    let fb_size = fb.info.buffer_size();
    let copy_size = size.min(fb_size);
    if copy_size == 0 {
        return -1;
    }

    let shm_virt = match shm_phys.to_virt_checked() {
        Some(v) => v.as_u64(),
        None => return -1,
    };

    let shm_ptr = shm_virt as *const u8;

    if damage.is_null() || damage_count == 0 {
        let Some(dst_ptr) = fb.checked_ptr(0, copy_size) else {
            return -1;
        };
        // SAFETY: source and destination have been validated and are non-overlapping.
        unsafe {
            ptr::copy_nonoverlapping(shm_ptr, dst_ptr, copy_size);
        }
        return framebuffer_flush();
    }

    let max_regions = slopos_abi::damage::MAX_DAMAGE_REGIONS as u32;
    let region_count = damage_count.min(max_regions) as usize;
    // Kernel syscall path validates this pointer and length before calling us.
    let regions = slopos_ostd::util::ptr_buf::borrow_buf(damage, region_count);

    let mut any_failed = false;
    for rect in regions {
        if !rect.is_valid() {
            continue;
        }
        if !copy_rect_from_shm(&fb, shm_ptr, copy_size, rect.x0, rect.y0, rect.x1, rect.y1) {
            any_failed = true;
            // Continue processing remaining rects instead of aborting —
            // partial presentation is better than no presentation.
        }
    }
    let flush = framebuffer_flush();
    if any_failed { -1 } else { flush }
}
