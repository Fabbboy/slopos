use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::pixel::DrawPixelFormat;
use slopos_abi::{DisplayInfo, PixelFormat};
use slopos_lib::{klog_debug, klog_warn};
use slopos_mm::hhdm::PhysAddrHhdm;
use spin::Mutex;

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
    pub(crate) fn draw_pixel_format(&self) -> DrawPixelFormat {
        DrawPixelFormat::from_pixel_format(self.info.format)
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

static FRAMEBUFFER: Mutex<FramebufferState> = Mutex::new(FramebufferState::new());
static FRAMEBUFFER_FLUSH: Mutex<Option<fn() -> c_int>> = Mutex::new(None);

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

pub fn framebuffer_is_initialized() -> i32 {
    FRAMEBUFFER.lock().fb.is_some() as i32
}

pub fn framebuffer_clear(color: u32) {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return,
    };

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let converted = fb.draw_pixel_format().convert_color(color);
    let base = fb.base_ptr();
    let pitch = fb.pitch() as usize;

    for y in 0..fb.height() as usize {
        let row_ptr = unsafe { base.add(y * pitch) };
        for x in 0..fb.width() as usize {
            let pixel_ptr = unsafe { row_ptr.add(x * bytes_pp) };
            unsafe {
                match bytes_pp {
                    2 => ptr::write_volatile(pixel_ptr as *mut u16, converted as u16),
                    3 => {
                        ptr::write_volatile(pixel_ptr, ((converted >> 16) & 0xFF) as u8);
                        ptr::write_volatile(pixel_ptr.add(1), ((converted >> 8) & 0xFF) as u8);
                        ptr::write_volatile(pixel_ptr.add(2), (converted & 0xFF) as u8);
                    }
                    4 => ptr::write_volatile(pixel_ptr as *mut u32, converted),
                    _ => {}
                }
            }
        }
    }
}

pub fn framebuffer_set_pixel(x: u32, y: u32, color: u32) {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return,
    };

    if x >= fb.width() || y >= fb.height() {
        return;
    }

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let converted = fb.draw_pixel_format().convert_color(color);
    let offset = y as usize * fb.pitch() as usize + x as usize * bytes_pp;
    let pixel_ptr = unsafe { fb.base_ptr().add(offset) };

    unsafe {
        match bytes_pp {
            2 => ptr::write_volatile(pixel_ptr as *mut u16, converted as u16),
            3 => {
                ptr::write_volatile(pixel_ptr, ((converted >> 16) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(1), ((converted >> 8) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(2), (converted & 0xFF) as u8);
            }
            4 => ptr::write_volatile(pixel_ptr as *mut u32, converted),
            _ => {}
        }
    }
}

pub fn framebuffer_get_pixel(x: u32, y: u32) -> u32 {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return 0,
    };

    if x >= fb.width() || y >= fb.height() {
        return 0;
    }

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let offset = y as usize * fb.pitch() as usize + x as usize * bytes_pp;
    let pixel_ptr = unsafe { fb.base_ptr().add(offset) };

    let mut color = 0u32;
    unsafe {
        match bytes_pp {
            2 => color = ptr::read_volatile(pixel_ptr as *const u16) as u32,
            3 => {
                let b0 = ptr::read_volatile(pixel_ptr) as u32;
                let b1 = ptr::read_volatile(pixel_ptr.add(1)) as u32;
                let b2 = ptr::read_volatile(pixel_ptr.add(2)) as u32;
                color = (b0 << 16) | (b1 << 8) | b2;
            }
            4 => color = ptr::read_volatile(pixel_ptr as *const u32),
            _ => {}
        }
    }

    fb.draw_pixel_format().convert_color(color)
}

pub fn framebuffer_blit(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> c_int {
    if width <= 0 || height <= 0 {
        return -1;
    }
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return -1,
    };
    let bpp = fb.bpp() as usize;
    if bpp == 0 {
        return -1;
    }
    let bytes_per_pixel = bpp.div_ceil(8);
    if bytes_per_pixel == 0 {
        return -1;
    }
    let fb_width = fb.width() as i32;
    let fb_height = fb.height() as i32;
    if src_x < 0
        || src_y < 0
        || dst_x < 0
        || dst_y < 0
        || src_x.saturating_add(width) > fb_width
        || src_y.saturating_add(height) > fb_height
        || dst_x.saturating_add(width) > fb_width
        || dst_y.saturating_add(height) > fb_height
    {
        return -1;
    }

    let row_bytes = width as usize * bytes_per_pixel;
    let src_pitch = fb.pitch() as usize;
    let base = fb.base_ptr();
    if base.is_null() {
        return -1;
    }

    if dst_y > src_y {
        for row in (0..height).rev() {
            let src_offset = (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset = (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    } else {
        for row in 0..height {
            let src_offset = (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset = (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    }
    0
}

pub fn framebuffer_get_width() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.width()).unwrap_or(0)
}

pub fn framebuffer_get_height() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.height()).unwrap_or(0)
}

pub fn framebuffer_get_bpp() -> u8 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.bpp()).unwrap_or(0)
}

pub fn framebuffer_convert_color(color: u32) -> u32 {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return color,
    };
    fb.draw_pixel_format().convert_color(color)
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

pub fn fb_flip_from_shm(shm_phys: PhysAddr, size: usize) -> c_int {
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

    unsafe {
        ptr::copy_nonoverlapping(shm_virt as *const u8, fb.base_ptr(), copy_size);
    }

    framebuffer_flush()
}
