use core::ffi::c_int;
use core::ptr;

use slopos_drivers::serial_println;
use slopos_lib::FramebufferInfo;
use slopos_mm::phys_virt::mm_phys_to_virt;
use spin::Mutex;

const PIXEL_FORMAT_RGB: u8 = 0x01;
const PIXEL_FORMAT_BGR: u8 = 0x02;
const PIXEL_FORMAT_RGBA: u8 = 0x03;
const PIXEL_FORMAT_BGRA: u8 = 0x04;

const MAX_FRAMEBUFFER_WIDTH: u32 = 4096;
const MAX_FRAMEBUFFER_HEIGHT: u32 = 4096;
const MIN_FRAMEBUFFER_WIDTH: u32 = 320;
const MIN_FRAMEBUFFER_HEIGHT: u32 = 240;
const MAX_BUFFER_SIZE: u32 = 64 * 1024 * 1024;

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct FramebufferInfoC {
    pub initialized: u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub pixel_format: u32,
}

impl FramebufferInfoC {
    pub const fn new() -> Self {
        Self {
            initialized: 0,
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            pixel_format: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub(crate) struct FbState {
    pub(crate) base: *mut u8,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) bpp: u8,
    pub(crate) pixel_format: u8,
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
static FRAMEBUFFER_INFO_EXPORT: Mutex<FramebufferInfoC> =
    Mutex::new(const { FramebufferInfoC::new() });

unsafe impl Send for FbState {}
unsafe impl Send for FramebufferState {}

fn determine_pixel_format(bpp: u8) -> u8 {
    match bpp {
        16 => PIXEL_FORMAT_RGB,
        24 => PIXEL_FORMAT_RGB,
        32 => PIXEL_FORMAT_RGBA,
        _ => PIXEL_FORMAT_RGB,
    }
}

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

fn framebuffer_convert_color_internal(state: &FbState, color: u32) -> u32 {
    match state.pixel_format {
        PIXEL_FORMAT_BGR | PIXEL_FORMAT_BGRA => {
            ((color & 0xFF0000) >> 16)
                | (color & 0x00FF00)
                | ((color & 0x0000FF) << 16)
                | (color & 0xFF000000)
        }
        _ => color,
    }
}

fn init_state_from_raw(addr: u64, width: u32, height: u32, pitch: u32, bpp: u8) -> i32 {
    if addr == 0 || width < MIN_FRAMEBUFFER_WIDTH || width > MAX_FRAMEBUFFER_WIDTH {
        return -1;
    }
    if height < MIN_FRAMEBUFFER_HEIGHT || height > MAX_FRAMEBUFFER_HEIGHT {
        return -1;
    }
    if bpp != 16 && bpp != 24 && bpp != 32 {
        return -1;
    }
    let _buffer_size = match pitch.checked_mul(height) {
        Some(sz) if sz > 0 && sz <= MAX_BUFFER_SIZE => sz,
        _ => return -1,
    };

    // Translate the physical address into the higher-half mapping if available.
    let virt_addr = mm_phys_to_virt(addr);
    let mapped_base = if virt_addr != 0 { virt_addr } else { addr };

    let fb_state = FbState {
        base: mapped_base as *mut u8,
        width,
        height,
        pitch,
        bpp,
        pixel_format: determine_pixel_format(bpp),
    };

    let mut guard = FRAMEBUFFER.lock();
    guard.fb = Some(fb_state);
    0
}

pub fn init_with_info(info: FramebufferInfo) -> i32 {
    let rc = init_state_from_raw(
        info.address as u64,
        info.width as u32,
        info.height as u32,
        info.pitch as u32,
        info.bpp as u8,
    );

    if rc == 0 {
        if let Some(fb) = FRAMEBUFFER.lock().fb {
            serial_println!(
                "Framebuffer init: phys=0x{:x} virt=0x{:x} {}x{} pitch={} bpp={}",
                info.address as u64,
                fb.base as u64,
                fb.width,
                fb.height,
                fb.pitch,
                fb.bpp
            );
        } else {
            serial_println!("Framebuffer init: state missing after init");
        }
    } else {
        serial_println!(
            "Framebuffer init failed: phys=0x{:x} {}x{} pitch={} bpp={}",
            info.address as u64,
            info.width,
            info.height,
            info.pitch,
            info.bpp
        );
    }

    rc
}
pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    let guard = FRAMEBUFFER.lock();
    let mut export = FRAMEBUFFER_INFO_EXPORT.lock();
    if let Some(fb) = guard.fb {
        *export = FramebufferInfoC {
            initialized: 1,
            width: fb.width,
            height: fb.height,
            pitch: fb.pitch,
            bpp: fb.bpp as u32,
            pixel_format: fb.pixel_format as u32,
        };
    } else {
        *export = FramebufferInfoC::default();
    }

    &mut *export as *mut FramebufferInfoC
}
pub fn framebuffer_is_initialized() -> i32 {
    FRAMEBUFFER.lock().fb.is_some() as i32
}
pub fn framebuffer_clear(color: u32) {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return,
    };

    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let converted = framebuffer_convert_color_internal(&fb, color);
    let base = fb.base;
    let pitch = fb.pitch as usize;

    for y in 0..fb.height as usize {
        let row_ptr = unsafe { base.add(y * pitch) };
        for x in 0..fb.width as usize {
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

    if x >= fb.width || y >= fb.height {
        return;
    }

    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let converted = framebuffer_convert_color_internal(&fb, color);
    let offset = y as usize * fb.pitch as usize + x as usize * bytes_pp;
    let pixel_ptr = unsafe { fb.base.add(offset) };

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

    if x >= fb.width || y >= fb.height {
        return 0;
    }

    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let offset = y as usize * fb.pitch as usize + x as usize * bytes_pp;
    let pixel_ptr = unsafe { fb.base.add(offset) };

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

    framebuffer_convert_color_internal(&fb, color)
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
    let bpp = fb.bpp as usize;
    if bpp == 0 {
        return -1;
    }
    let bytes_per_pixel = bpp.div_ceil(8);
    if bytes_per_pixel == 0 {
        return -1;
    }
    let fb_width = fb.width as i32;
    let fb_height = fb.height as i32;
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
    let src_pitch = fb.pitch as usize;
    let base = fb.base;
    if base.is_null() {
        return -1;
    }

    if dst_y > src_y {
        for row in (0..height).rev() {
            let src_offset =
                (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset =
                (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    } else {
        for row in 0..height {
            let src_offset =
                (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset =
                (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    }
    0
}
pub fn framebuffer_get_width() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.width).unwrap_or(0)
}
pub fn framebuffer_get_height() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.height).unwrap_or(0)
}
pub fn framebuffer_get_bpp() -> u8 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.bpp).unwrap_or(0)
}
pub fn framebuffer_rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32) | ((a as u32) << 24)
}
pub fn framebuffer_rgb(r: u8, g: u8, b: u8) -> u32 {
    framebuffer_rgba(r, g, b, 0xFF)
}
pub fn framebuffer_convert_color(color: u32) -> u32 {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return color,
    };
    framebuffer_convert_color_internal(&fb, color)
}

pub(crate) fn snapshot() -> Option<FbState> {
    FRAMEBUFFER.lock().fb
}
