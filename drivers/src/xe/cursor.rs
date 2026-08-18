//! Hardware-cursor plane.
//!
//! Writes only the active pipe's `CUR_CTL`, `CUR_POS` and `CUR_BASE`, with
//! `CUR_BASE` the arming write of the group; no primary-plane or
//! pipe/transcoder register is ever written. The pure size/mode/position
//! encoding lives in [`crate::xe_logic::cursor_config`].
//!
//! The surface is allocated once at [`init`], sized for a 256x256 cursor, as a
//! Write-Combining buffer — the plane scans it directly from RAM, so its pixels
//! must bypass the WriteBack cache — and GGTT-mapped strictly above both the
//! firmware framebuffer and the primary surface. [`init`] returning `false`
//! leaves the scanout front-end on a software cursor.

use slopos_abi::DisplayInfo;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::free_page_frame;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::util::ptr_buf;

use super::{fb_mem, ggtt, mmio_map};
use crate::xe_logic::cursor_config::{self, CursorMode};
use crate::xe_logic::ggtt_pte;
use crate::xe_logic::regs::{self, Pipe};

/// Edge length, in pixels, of the largest ARGB cursor the Gen12 plane supports.
const CURSOR_MAX_SIDE: u32 = 256;

/// The largest cursor square at four bytes per ARGB8888 pixel: every supported
/// mode fits, so the surface is allocated and GGTT-mapped once and reused.
const CURSOR_SURFACE_BYTES: u64 = (CURSOR_MAX_SIDE as u64) * (CURSOR_MAX_SIDE as u64) * 4;

const CURSOR_SURFACE_PAGES: u32 = (CURSOR_SURFACE_BYTES / PAGE_SIZE_4KB) as u32;

/// GGTT placement alignment for the cursor surface (64 KiB).
const CURSOR_GGTT_ALIGN: u64 = 0x1_0000;

/// Bytes reserved above the live primary `PLANE_SURF` before the cursor surface
/// is placed: the largest framebuffer the display ABI permits, so the cursor's
/// PTEs land strictly above any possible primary surface.
const CURSOR_PLACEMENT_RESERVE_BYTES: u64 =
    (DisplayInfo::MAX_DIMENSION as u64) * (DisplayInfo::MAX_DIMENSION as u64) * 4;

struct CursorState {
    mmio: MmioRegion,
    pipe: Pipe,
    /// Display IP major version; version 13 needs the cursor-arbitration
    /// workaround Wa_22012358565.
    display_ip_version: u8,
    /// GGTT byte address the cursor plane scans, fixed at [`init`].
    surf_ggtt: u32,
    /// Write-Combining virtual address of the cursor surface backing, kept as a
    /// `u64` so the retained state stays `Send` for the static below.
    surf_virt: u64,
    mode: Option<CursorMode>,
    /// Active hotspot, subtracted from the position so the hot pixel lands on
    /// the reported coordinate.
    hot_x: u32,
    hot_y: u32,
    /// Last requested cursor position (top-left, before hotspot adjustment).
    x: u32,
    y: u32,
}

/// `None` until [`init`] binds the cursor plane.
static CURSOR_STATE: SpinLock<Option<CursorState>> =
    SpinLock::new(None, lock_class!("CURSOR_STATE", LOCK_LEVEL_RESOURCE));

/// Allocate the cursor surface and record the binding the cursor plane will
/// drive. Writes no register; on failure it returns `false` having touched
/// nothing, so the caller can fall back to a software cursor.
pub fn init(mmio: &MmioRegion, pipe: Pipe, display_ip_version: u8) -> bool {
    let Some((surf_ggtt, surf_virt)) = allocate_surface(mmio, pipe) else {
        return false;
    };
    *CURSOR_STATE.lock() = Some(CursorState {
        mmio: mmio.clone(),
        pipe,
        display_ip_version,
        surf_ggtt,
        surf_virt,
        mode: None,
        hot_x: 0,
        hot_y: 0,
        x: 0,
        y: 0,
    });
    true
}

/// Upload a new cursor image (ARGB8888, `hot_x`/`hot_y` hotspot) and arm the
/// cursor plane.
///
/// The image is square: its side is the integer square root of `len / 4` (four
/// bytes per ARGB pixel), which selects the smallest covering [`CursorMode`].
/// The copy into the surface is bounded by its capacity so the hardware never
/// scans past the mapping, and the plane is programmed `CUR_CTL` → `CUR_POS` →
/// `CUR_BASE`, `CUR_BASE` last so it arms only once mode and position are in
/// place.
///
/// No `SpinLock` is held across the image copy or any MMIO write.
pub fn set_image(image: *const u8, len: usize, hot_x: u32, hot_y: u32) -> bool {
    if image.is_null() || len == 0 {
        return false;
    }

    let pixels = (len / 4) as u64;
    let side = integer_sqrt(pixels);
    let Some(mode) = cursor_config::mode_for_side(side) else {
        return false;
    };

    let snapshot = {
        let guard = CURSOR_STATE.lock();
        guard.as_ref().map(|state| {
            (
                state.mmio.clone(),
                state.pipe,
                state.display_ip_version,
                state.surf_ggtt,
                state.surf_virt,
                state.x,
                state.y,
            )
        })
    };
    let Some((mmio, pipe, display_ip_version, surf_ggtt, surf_virt, x, y)) = snapshot else {
        // `init` never ran or its allocation failed: nothing is bound.
        return false;
    };

    let copy_len = core::cmp::min(len as u64, CURSOR_SURFACE_BYTES) as usize;
    ptr_buf::copy_bytes(surf_virt as *mut u8, image, copy_len);

    mmio.write::<u32>(
        regs::cur_ctl(pipe),
        cursor_config::cur_ctl_value(mode, true, display_ip_version),
    );
    mmio.write::<u32>(
        regs::cur_pos(pipe),
        cursor_config::cur_pos_pack(x as i32 - hot_x as i32, y as i32 - hot_y as i32),
    );
    mmio.write::<u32>(regs::cur_base(pipe), surf_ggtt);

    if let Some(state) = CURSOR_STATE.lock().as_mut() {
        state.mode = Some(mode);
        state.hot_x = hot_x;
        state.hot_y = hot_y;
    }
    true
}

/// Move the hardware cursor to `(x, y)`.
///
/// `CUR_POS` carries the hotspot-adjusted coordinate; a negative result places
/// the cursor partly off the top/left edge. It is double-buffered and does not
/// latch on its own, so `CUR_BASE` is re-issued with the same surface address —
/// the image is unchanged and nothing flickers — to arm the new position; a
/// `CUR_POS`-only write would leave the cursor frozen where it last armed.
/// `false` when the cursor is unbound.
pub fn move_cursor(x: u32, y: u32) -> bool {
    let snapshot = {
        let mut guard = CURSOR_STATE.lock();
        match guard.as_mut() {
            Some(state) => {
                state.x = x;
                state.y = y;
                Some((
                    state.mmio.clone(),
                    state.pipe,
                    state.surf_ggtt,
                    state.hot_x,
                    state.hot_y,
                ))
            }
            None => None,
        }
    };
    let Some((mmio, pipe, surf_ggtt, hot_x, hot_y)) = snapshot else {
        return false;
    };
    // CUR_BASE re-arms the plane so the shadowed position latches at the next
    // vblank.
    mmio.write::<u32>(
        regs::cur_pos(pipe),
        cursor_config::cur_pos_pack(x as i32 - hot_x as i32, y as i32 - hot_y as i32),
    );
    mmio.write::<u32>(regs::cur_base(pipe), surf_ggtt);
    true
}

pub fn available() -> bool {
    true
}

/// Allocate the Write-Combining cursor surface and GGTT-map it strictly above
/// both the firmware framebuffer and the primary surface.
///
/// Returns its GGTT byte address and Write-Combining virtual address, or `None`
/// after freeing any partial allocation. Reads the primary `PLANE_SURF` only to
/// place above it — never writes the primary plane.
fn allocate_surface(mmio: &MmioRegion, pipe: Pipe) -> Option<(u32, u64)> {
    let bank = mmio_map::ggtt_bank(mmio)?;
    let ggtt_total = (bank.size() as u64 / regs::GGTT_PTE_BYTES as u64) * PAGE_SIZE_4KB;

    let (phys, virt) = fb_mem::alloc_wc_scanout(CURSOR_SURFACE_PAGES)?;

    // The live primary `PLANE_SURF` already sits above the firmware framebuffer,
    // so reserving past the largest possible primary places the cursor's PTEs
    // strictly above both.
    let primary_surf = mmio.read::<u32>(regs::plane_surf(pipe)) as u64;
    let Some(cursor_ggtt) = ggtt_pte::alloc_above(
        primary_surf,
        CURSOR_PLACEMENT_RESERVE_BYTES,
        CURSOR_GGTT_ALIGN,
        CURSOR_SURFACE_PAGES,
        ggtt_total,
    ) else {
        free_page_frame(phys);
        return None;
    };

    if !ggtt::map_pages(&bank, cursor_ggtt, phys, CURSOR_SURFACE_PAGES) {
        free_page_frame(phys);
        return None;
    }

    Some((cursor_ggtt as u32, virt))
}

/// Floor of the integer square root of `value`, by Newton's method — integer
/// arithmetic only, since the kernel is built soft-float.
fn integer_sqrt(value: u64) -> u32 {
    if value == 0 {
        return 0;
    }
    let mut estimate = value;
    let mut next = (estimate + 1) / 2;
    while next < estimate {
        estimate = next;
        next = (estimate + value / estimate) / 2;
    }
    estimate as u32
}
