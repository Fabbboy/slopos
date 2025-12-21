use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::video_bridge::{VideoError, VideoResult};
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames};
use slopos_mm::phys_virt::mm_phys_to_virt;
use slopos_lib::klog_info;
use core::sync::atomic::{AtomicU8, Ordering};
use slopos_sched::MAX_TASKS;
use spin::Mutex;

use crate::framebuffer;
use crate::font;

const SURFACE_BG_COLOR: u32 = 0x0000_0000;

#[derive(Copy, Clone)]
struct Surface {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
    bytes_pp: u8,
    pixel_format: u8,
    dirty: bool,
    dirty_x0: i32,
    dirty_y0: i32,
    dirty_x1: i32,
    dirty_y1: i32,
    buffer: *mut u8,
    x: i32,
    y: i32,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    const fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            bytes_pp: 0,
            pixel_format: 0,
            dirty: false,
            dirty_x0: 0,
            dirty_y0: 0,
            dirty_x1: -1,
            dirty_y1: -1,
            buffer: ptr::null_mut(),
            x: 0,
            y: 0,
        }
    }
}

#[derive(Copy, Clone)]
struct SurfaceSlot {
    active: bool,
    task_id: u32,
    surface: Surface,
}

unsafe impl Send for SurfaceSlot {}
unsafe impl Sync for SurfaceSlot {}
impl SurfaceSlot {
    const fn empty() -> Self {
        Self {
            active: false,
            task_id: 0,
            surface: Surface::empty(),
        }
    }
}

static SURFACES: Mutex<[SurfaceSlot; MAX_TASKS]> =
    Mutex::new([SurfaceSlot::empty(); MAX_TASKS]);
static SURFACE_CREATE_LOGGED: [AtomicU8; MAX_TASKS] = {
    const ZERO: AtomicU8 = AtomicU8::new(0);
    [ZERO; MAX_TASKS]
};
static COMPOSITOR_LOGGED: AtomicU8 = AtomicU8::new(0);
static COMPOSITOR_EMPTY_LOGGED: AtomicU8 = AtomicU8::new(0);

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

fn find_slot(slots: &[SurfaceSlot; MAX_TASKS], task_id: u32) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| {
            if slot.active && slot.task_id == task_id {
                Some(idx)
            } else {
                None
            }
        })
}

fn find_free_slot(slots: &[SurfaceSlot; MAX_TASKS]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| if !slot.active { Some(idx) } else { None })
}

fn mark_dirty(surface: &mut Surface, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
    if x0 > x1 || y0 > y1 {
        return;
    }
    if x0 < 0 {
        x0 = 0;
    }
    if y0 < 0 {
        y0 = 0;
    }
    let max_x = surface.width as i32 - 1;
    let max_y = surface.height as i32 - 1;
    if x1 > max_x {
        x1 = max_x;
    }
    if y1 > max_y {
        y1 = max_y;
    }
    if x0 > x1 || y0 > y1 {
        return;
    }
    if !surface.dirty {
        surface.dirty = true;
        surface.dirty_x0 = x0;
        surface.dirty_y0 = y0;
        surface.dirty_x1 = x1;
        surface.dirty_y1 = y1;
    } else {
        surface.dirty_x0 = surface.dirty_x0.min(x0);
        surface.dirty_y0 = surface.dirty_y0.min(y0);
        surface.dirty_x1 = surface.dirty_x1.max(x1);
        surface.dirty_y1 = surface.dirty_y1.max(y1);
    }
}

fn create_surface_for_task(
    slots: &mut [SurfaceSlot; MAX_TASKS],
    task_id: u32,
) -> Result<usize, VideoError> {
    let fb = framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)?;
    let bytes_pp = bytes_per_pixel(fb.bpp) as u8;
    if bytes_pp != 3 && bytes_pp != 4 {
        if (task_id as usize) < MAX_TASKS
            && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
        {
            klog_info!(
                "surface: invalid bytes_per_pixel {} for task {}",
                bytes_pp,
                task_id
            );
        }
        return Err(VideoError::Invalid);
    }

    let candidates = [
        (fb.width, fb.height),
        (800, 600),
        (640, 480),
        (320, 240),
    ];

    for (width, height) in candidates {
        if width == 0 || height == 0 || width > fb.width || height > fb.height {
            continue;
        }
        let pitch = width.saturating_mul(bytes_pp as u32);
        let size = (pitch as u64).saturating_mul(height as u64);
        if size == 0 || size > usize::MAX as u64 {
            continue;
        }
        let pages = (size + (PAGE_SIZE_4KB - 1)) / PAGE_SIZE_4KB;
        if pages == 0 || pages > u32::MAX as u64 {
            continue;
        }
        let phys = alloc_page_frames(pages as u32, ALLOC_FLAG_ZERO);
        if phys == 0 {
            continue;
        }
        let virt = mm_phys_to_virt(phys);
        let virt = if virt != 0 { virt } else { phys };

        let slot = match find_free_slot(slots) {
            Some(idx) => idx,
            None => {
                if (task_id as usize) < MAX_TASKS
                    && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
                {
                    klog_info!("surface: no free slot for task {}", task_id);
                }
                return Err(VideoError::Invalid);
            }
        };

        slots[slot] = SurfaceSlot {
            active: true,
            task_id,
            surface: Surface {
                width,
                height,
                pitch,
                bpp: fb.bpp,
                bytes_pp,
                pixel_format: fb.pixel_format,
                dirty: true,
                dirty_x0: 0,
                dirty_y0: 0,
                dirty_x1: width as i32 - 1,
                dirty_y1: height as i32 - 1,
                buffer: virt as *mut u8,
                x: 0,
                y: 0,
            },
        };

        if SURFACE_BG_COLOR != 0 {
            surface_clear(&mut slots[slot].surface, SURFACE_BG_COLOR)?;
        }
        return Ok(slot);
    }

    if (task_id as usize) < MAX_TASKS
        && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
    {
        klog_info!("surface: page alloc failed for task {}", task_id);
    }
    Err(VideoError::Invalid)
}

fn with_surface_mut(task_id: u32, f: impl FnOnce(&mut Surface) -> VideoResult) -> VideoResult {
    let surface_ptr = {
        let mut slots = SURFACES.lock();
        let slot = match find_slot(&slots, task_id) {
            Some(idx) => idx,
            None => create_surface_for_task(&mut slots, task_id)?,
        };
        &mut slots[slot].surface as *mut Surface
    };
    // Avoid holding the global surface lock during long draws.
    unsafe { f(&mut *surface_ptr) }
}

fn surface_clear(surface: &mut Surface, color: u32) -> VideoResult {
    if surface.buffer.is_null() {
        return Err(VideoError::Invalid);
    }
    let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
    let row_bytes = surface.width.saturating_mul(surface.bytes_pp as u32) as usize;
    for row in 0..surface.height as usize {
        let row_ptr = unsafe { surface.buffer.add(row * surface.pitch as usize) };
        for col in 0..surface.width as usize {
            let pixel_ptr = unsafe { row_ptr.add(col * surface.bytes_pp as usize) };
            unsafe { write_pixel(pixel_ptr, surface.bytes_pp, converted) };
        }
        let _ = row_bytes;
    }
    mark_dirty(
        surface,
        0,
        0,
        surface.width as i32 - 1,
        surface.height as i32 - 1,
    );
    Ok(())
}

unsafe fn write_pixel(ptr: *mut u8, bytes_pp: u8, color: u32) {
    match bytes_pp {
        4 => {
            let dst = ptr as *mut u32;
            dst.write_unaligned(color);
        }
        3 => {
            let bytes = color.to_le_bytes();
            ptr.copy_from_nonoverlapping(bytes.as_ptr(), 3);
        }
        _ => {}
    }
}

fn surface_set_pixel(surface: &mut Surface, x: i32, y: i32, color: u32) -> VideoResult {
    if x < 0 || y < 0 || x as u32 >= surface.width || y as u32 >= surface.height {
        return Err(VideoError::OutOfBounds);
    }
    if surface.buffer.is_null() {
        return Err(VideoError::Invalid);
    }

    let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
    let offset = (y as usize * surface.pitch as usize)
        + (x as usize * surface.bytes_pp as usize);
    unsafe {
        let ptr = surface.buffer.add(offset);
        write_pixel(ptr, surface.bytes_pp, converted);
    }
    Ok(())
}

pub fn surface_draw_rect_filled_fast(
    task_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    if w <= 0 || h <= 0 {
        return Err(VideoError::Invalid);
    }
    let result = with_surface_mut(task_id, |surface| {
        let mut x0 = x;
        let mut y0 = y;
        let mut x1 = x + w - 1;
        let mut y1 = y + h - 1;
        clip_rect(surface, &mut x0, &mut y0, &mut x1, &mut y1)?;
        if surface.buffer.is_null() {
            return Err(VideoError::Invalid);
        }
        let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
        let bytes_pp = surface.bytes_pp as usize;
        let pitch = surface.pitch as usize;
        let span_w = (x1 - x0 + 1) as usize;
        for row in y0..=y1 {
            let row_off = row as usize * pitch + x0 as usize * bytes_pp;
            unsafe {
                let dst = surface.buffer.add(row_off);
                match bytes_pp {
                    4 => {
                        if converted == 0 {
                            ptr::write_bytes(dst, 0, span_w * 4);
                        } else {
                            let dst32 = dst as *mut u32;
                            for col in 0..span_w {
                                dst32.add(col).write_unaligned(converted);
                            }
                        }
                    }
                    3 => {
                        let bytes = converted.to_le_bytes();
                        for col in 0..span_w {
                            let px = dst.add(col * 3);
                            px.write(bytes[0]);
                            px.add(1).write(bytes[1]);
                            px.add(2).write(bytes[2]);
                        }
                    }
                    _ => {}
                }
            }
        }
        mark_dirty(surface, x0, y0, x1, y1);
        Ok(())
    });
    result
}

pub fn surface_draw_line(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    with_surface_mut(task_id, |surface| {
        let mut x0 = x0;
        let mut y0 = y0;
        let x1 = x1;
        let y1 = y1;
        let min_x = x0.min(x1);
        let min_y = y0.min(y1);
        let max_x = x0.max(x1);
        let max_y = y0.max(y1);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            let _ = surface_set_pixel(surface, x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
        mark_dirty(surface, min_x, min_y, max_x, max_y);
        Ok(())
    })
}

pub fn surface_draw_circle(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        let mut x = radius;
        let mut y = 0;
        let mut err = 1 - radius;
        while x >= y {
            let _ = surface_set_pixel(surface, cx + x, cy + y, color);
            let _ = surface_set_pixel(surface, cx + y, cy + x, color);
            let _ = surface_set_pixel(surface, cx - y, cy + x, color);
            let _ = surface_set_pixel(surface, cx - x, cy + y, color);
            let _ = surface_set_pixel(surface, cx - x, cy - y, color);
            let _ = surface_set_pixel(surface, cx - y, cy - x, color);
            let _ = surface_set_pixel(surface, cx + y, cy - x, color);
            let _ = surface_set_pixel(surface, cx + x, cy - y, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
        mark_dirty(
            surface,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        Ok(())
    })
}

pub fn surface_draw_circle_filled(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        let mut x = radius;
        let mut y = 0;
        let mut err = 1 - radius;
        while x >= y {
            draw_hline(surface, cx - x, cx + x, cy + y, color);
            draw_hline(surface, cx - x, cx + x, cy - y, color);
            draw_hline(surface, cx - y, cx + y, cy + x, color);
            draw_hline(surface, cx - y, cx + y, cy - x, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
        mark_dirty(
            surface,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        Ok(())
    })
}

pub fn surface_font_draw_string(
    task_id: u32,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if str_ptr.is_null() {
        return -1;
    }
    let mut tmp = [0u8; 1024];
    let text = unsafe { c_str_to_bytes(str_ptr, &mut tmp) };
    let rc = with_surface_mut(task_id, |surface| {
        let mut cx = x;
        let mut cy = y;
        let mut dirty = false;
        let mut dirty_x0 = 0;
        let mut dirty_y0 = 0;
        let mut dirty_x1 = 0;
        let mut dirty_y1 = 0;
        for &ch in text {
            match ch {
                b'\n' => {
                    cx = x;
                    cy += font::FONT_CHAR_HEIGHT;
                }
                b'\r' => {
                    cx = x;
                }
                b'\t' => {
                    let tab_width = 4 * font::FONT_CHAR_WIDTH;
                    cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
                }
                _ => {
                    draw_glyph(surface, cx, cy, ch, fg_color, bg_color)?;
                    let gx0 = cx;
                    let gy0 = cy;
                    let gx1 = cx + font::FONT_CHAR_WIDTH - 1;
                    let gy1 = cy + font::FONT_CHAR_HEIGHT - 1;
                    if !dirty {
                        dirty = true;
                        dirty_x0 = gx0;
                        dirty_y0 = gy0;
                        dirty_x1 = gx1;
                        dirty_y1 = gy1;
                    } else {
                        dirty_x0 = dirty_x0.min(gx0);
                        dirty_y0 = dirty_y0.min(gy0);
                        dirty_x1 = dirty_x1.max(gx1);
                        dirty_y1 = dirty_y1.max(gy1);
                    }
                    cx += font::FONT_CHAR_WIDTH;
                    if cx + font::FONT_CHAR_WIDTH > surface.width as i32 {
                        cx = x;
                        cy += font::FONT_CHAR_HEIGHT;
                    }
                }
            }
            if cy >= surface.height as i32 {
                break;
            }
        }
        if dirty {
            mark_dirty(surface, dirty_x0, dirty_y0, dirty_x1, dirty_y1);
        }
        Ok(())
    });
    if rc.is_ok() { 0 } else { -1 }
}

pub fn surface_blit(
    task_id: u32,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> VideoResult {
    if width <= 0 || height <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        if surface.buffer.is_null() {
            return Err(VideoError::Invalid);
        }
        let bytes_pp = surface.bytes_pp as usize;
        let mut w = width;
        let mut h = height;
        if src_x < 0 || src_y < 0 || dst_x < 0 || dst_y < 0 {
            return Err(VideoError::OutOfBounds);
        }
        if src_x + w > surface.width as i32 {
            w = surface.width as i32 - src_x;
        }
        if dst_x + w > surface.width as i32 {
            w = surface.width as i32 - dst_x;
        }
        if src_y + h > surface.height as i32 {
            h = surface.height as i32 - src_y;
        }
        if dst_y + h > surface.height as i32 {
            h = surface.height as i32 - dst_y;
        }
        if w <= 0 || h <= 0 {
            return Err(VideoError::Invalid);
        }
        for row in 0..h {
            let src_off = ((src_y + row) as usize * surface.pitch as usize)
                + (src_x as usize * bytes_pp);
            let dst_off = ((dst_y + row) as usize * surface.pitch as usize)
                + (dst_x as usize * bytes_pp);
            unsafe {
                let src_ptr = surface.buffer.add(src_off);
                let dst_ptr = surface.buffer.add(dst_off);
                ptr::copy(src_ptr, dst_ptr, (w as usize) * bytes_pp);
            }
        }
        mark_dirty(
            surface,
            dst_x,
            dst_y,
            dst_x + w - 1,
            dst_y + h - 1,
        );
        Ok(())
    })
}

pub fn compositor_present() -> c_int {
    if COMPOSITOR_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        klog_info!("compositor: present loop online");
    }
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return -1,
    };
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let slots_snapshot = {
        let slots = SURFACES.lock();
        *slots
    };
    let mut active = 0u32;
    let mut dirty_tasks = [0u32; MAX_TASKS];
    let mut dirty_count = 0usize;
    let mut did_work = false;
    let fb_width = fb.width as i32;
    let fb_height = fb.height as i32;
    for slot in slots_snapshot.iter() {
        if !slot.active {
            continue;
        }
        active = active.saturating_add(1);
        let surface = &slot.surface;
        if surface.buffer.is_null() {
            continue;
        }
        if !surface.dirty {
            continue;
        }
        if surface.bpp != fb.bpp {
            return -1;
        }

        let mut src_x = surface.dirty_x0;
        let mut src_y = surface.dirty_y0;
        let mut src_x1 = surface.dirty_x1;
        let mut src_y1 = surface.dirty_y1;
        if src_x < 0 {
            src_x = 0;
        }
        if src_y < 0 {
            src_y = 0;
        }
        let max_x = surface.width as i32 - 1;
        let max_y = surface.height as i32 - 1;
        if src_x1 > max_x {
            src_x1 = max_x;
        }
        if src_y1 > max_y {
            src_y1 = max_y;
        }
        if src_x > src_x1 || src_y > src_y1 {
            continue;
        }

        let mut dst_x = surface.x + src_x;
        let mut dst_y = surface.y + src_y;
        let mut copy_w = src_x1 - src_x + 1;
        let mut copy_h = src_y1 - src_y + 1;
        if dst_x < 0 {
            let delta = -dst_x;
            src_x += delta;
            copy_w -= delta;
            dst_x = 0;
        }
        if dst_y < 0 {
            let delta = -dst_y;
            src_y += delta;
            copy_h -= delta;
            dst_y = 0;
        }
        if dst_x + copy_w > fb_width {
            copy_w = fb_width - dst_x;
        }
        if dst_y + copy_h > fb_height {
            copy_h = fb_height - dst_y;
        }
        if copy_w <= 0 || copy_h <= 0 {
            continue;
        }

        for row in 0..copy_h {
            let src_row = (src_y + row) as usize * surface.pitch as usize;
            let dst_off = ((dst_y + row) as usize * fb.pitch as usize)
                + (dst_x as usize * bytes_pp);
            unsafe {
                let src_ptr = surface.buffer.add(src_row + (src_x as usize * bytes_pp));
                let dst_ptr = fb.base.add(dst_off);
                let row_bytes = copy_w as usize * bytes_pp;
                ptr::copy_nonoverlapping(src_ptr, dst_ptr, row_bytes);
            }
        }
        did_work = true;
        if dirty_count < MAX_TASKS {
            dirty_tasks[dirty_count] = slot.task_id;
            dirty_count += 1;
        }
    }
    if dirty_count > 0 {
        let mut slots = SURFACES.lock();
        for idx in 0..dirty_count {
            let task_id = dirty_tasks[idx];
            if let Some(slot_idx) = find_slot(&slots, task_id) {
                let surface = &mut slots[slot_idx].surface;
                surface.dirty = false;
                surface.dirty_x0 = 0;
                surface.dirty_y0 = 0;
                surface.dirty_x1 = -1;
                surface.dirty_y1 = -1;
            }
        }
    }
    if active == 0 && COMPOSITOR_EMPTY_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        klog_info!("compositor: no active surfaces to present");
    }
    if did_work { 1 } else { 0 }
}

fn clip_rect(
    surface: &Surface,
    x0: &mut i32,
    y0: &mut i32,
    x1: &mut i32,
    y1: &mut i32,
) -> VideoResult {
    if *x0 < 0 {
        *x0 = 0;
    }
    if *y0 < 0 {
        *y0 = 0;
    }
    if *x1 >= surface.width as i32 {
        *x1 = surface.width as i32 - 1;
    }
    if *y1 >= surface.height as i32 {
        *y1 = surface.height as i32 - 1;
    }
    if *x0 > *x1 || *y0 > *y1 {
        return Err(VideoError::OutOfBounds);
    }
    Ok(())
}

fn draw_hline(surface: &mut Surface, x0: i32, x1: i32, y: i32, color: u32) {
    let mut x0 = x0;
    let mut x1 = x1;
    if y < 0 || y >= surface.height as i32 {
        return;
    }
    if x0 < 0 {
        x0 = 0;
    }
    if x1 >= surface.width as i32 {
        x1 = surface.width as i32 - 1;
    }
    for x in x0..=x1 {
        let _ = surface_set_pixel(surface, x, y, color);
    }
}

fn draw_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    ch: u8,
    fg_color: u32,
    bg_color: u32,
) -> VideoResult {
    let glyph = font::font_glyph(ch).unwrap_or_else(|| {
        font::font_glyph(b' ').unwrap()
    });
    for (row_idx, row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py >= surface.height as i32 {
            continue;
        }
        for col in 0..font::FONT_CHAR_WIDTH {
            let px = x + col;
            if px < 0 || px >= surface.width as i32 {
                continue;
            }
            let mask = 1u8 << (7 - col);
            let color = if (row_bits & mask) != 0 {
                fg_color
            } else {
                bg_color
            };
            let _ = surface_set_pixel(surface, px, py, color);
        }
    }
    Ok(())
}

unsafe fn c_str_len(ptr: *const c_char) -> usize {
    if ptr.is_null() {
        return 0;
    }
    let mut len = 0usize;
    let mut p = ptr;
    while unsafe { *p } != 0 {
        len += 1;
        p = unsafe { p.add(1) };
    }
    len
}

unsafe fn c_str_to_bytes<'a>(ptr: *const c_char, buf: &'a mut [u8]) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    let len = unsafe { c_str_len(ptr) }.min(buf.len());
    for i in 0..len {
        unsafe {
            buf[i] = *ptr.add(i) as u8;
        }
    }
    &buf[..len]
}
