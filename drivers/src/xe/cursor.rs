//! Hardware-cursor plane.
//!
//! The cursor is a SEPARATE plane: this module writes ONLY the active pipe's
//! `CUR_CTL`, `CUR_POS`, and `CUR_BASE` registers, with `CUR_BASE` the last
//! (arming) write of the group. It never writes the primary plane or any
//! pipe/transcoder register, so a cursor failure can never disturb the primary
//! scanout. The pure size/mode/position encoding lives in
//! [`crate::xe_logic::cursor_config`]; this half holds the register window and
//! the GGTT-mapped cursor surface and turns those encodings into MMIO writes.
//!
//! The cursor surface is allocated once at [`init`] (sized for the largest
//! supported 256x256 cursor) as a Write-Combining buffer — the cursor plane scans
//! it directly from RAM, so its pixels must bypass the WriteBack cache. It is
//! GGTT-mapped strictly ABOVE both the firmware framebuffer and the primary
//! surface (the placement reads the live primary `PLANE_SURF` — a read, never a
//! write — and reserves room past the largest possible primary so the cursor's
//! PTEs never collide with it). If allocation fails [`init`] returns `false` and
//! the scanout front-end falls back to a software cursor.

use slopos_abi::DisplayInfo;
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::free_page_frame;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::util::ptr_buf;

use super::{fb_mem, ggtt, mmio_map};
use crate::xe_logic::cursor_config::{self, CursorMode};
use crate::xe_logic::ggtt_pte;
use crate::xe_logic::regs::{self, Pipe};

/// Edge length, in pixels, of the largest ARGB cursor the Gen12 plane supports.
const CURSOR_MAX_SIDE: u32 = 256;

/// Bytes of the lazily-allocated cursor surface: the largest cursor square at
/// four bytes per ARGB8888 pixel. Every supported mode fits, so the surface is
/// allocated and GGTT-mapped exactly once and reused across image changes.
const CURSOR_SURFACE_BYTES: u64 = (CURSOR_MAX_SIDE as u64) * (CURSOR_MAX_SIDE as u64) * 4;

/// Page count covering [`CURSOR_SURFACE_BYTES`].
const CURSOR_SURFACE_PAGES: u32 = (CURSOR_SURFACE_BYTES / PAGE_SIZE_4KB) as u32;

/// GGTT placement alignment for the cursor surface (64 KiB).
const CURSOR_GGTT_ALIGN: u64 = 0x1_0000;

/// Bytes reserved above the live primary `PLANE_SURF` before the cursor surface
/// is placed. Sized to the largest framebuffer the display ABI permits
/// (`MAX_DIMENSION` square at four bytes per pixel), so the cursor's PTEs land
/// strictly above any possible primary surface and never overwrite it.
const CURSOR_PLACEMENT_RESERVE_BYTES: u64 =
    (DisplayInfo::MAX_DIMENSION as u64) * (DisplayInfo::MAX_DIMENSION as u64) * 4;

/// Retained cursor-plane binding: the register window, the pipe whose
/// `CUR_CTL`/`CUR_POS`/`CUR_BASE` this driver owns, the lazily-allocated cursor
/// surface, the programmed mode, the active hotspot, and the last cursor
/// position. `None` until [`init`] records it after a committed repoint.
struct CursorState {
    mmio: MmioRegion,
    pipe: Pipe,
    /// Display IP major version (12 / 13), needed so `CUR_CTL` programming can
    /// apply the version-13 cursor-arbitration workaround (Wa_22012358565).
    display_ip_version: u8,
    /// GGTT byte address the cursor plane scans, fixed at [`init`].
    surf_ggtt: u32,
    /// Write-Combining virtual address of the cursor surface backing, kept as a
    /// `u64` so the retained state stays `Send` for the static below.
    surf_virt: u64,
    /// The last programmed cursor mode; `None` until the first image upload.
    mode: Option<CursorMode>,
    /// Active hotspot, subtracted from the position so the hot pixel lands on
    /// the reported coordinate.
    hot_x: u32,
    hot_y: u32,
    /// Last requested cursor position (top-left, before hotspot adjustment).
    x: u32,
    y: u32,
}

/// Installed by [`init`]; consulted by the cursor entry points. `None` until the
/// cursor plane is bound.
static CURSOR_STATE: SpinLock<Option<CursorState>> = SpinLock::new(None, LOCK_LEVEL_RESOURCE);

/// Allocate the cursor surface and record the binding the cursor plane will drive.
///
/// Allocates and GGTT-maps the Write-Combining cursor surface strictly above the
/// firmware and primary surfaces, then records the register window, active pipe,
/// `display_ip_version` (so `CUR_CTL` programming can apply the version-13
/// arbitration workaround), and surface. The `MmioRegion` is cloned to owned
/// storage so the binding outlives the borrow. Returns `false` — touching nothing
/// — if allocation, placement, or mapping
/// fails, so the caller can fall back to a software cursor. Writes no register.
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
/// bytes per ARGB pixel), which selects the smallest covering [`CursorMode`]. On
/// the first call the cursor surface is allocated and GGTT-mapped above the
/// firmware and primary surfaces; subsequent calls reuse it. The image is copied
/// into the surface (bounded by both `len` and the surface capacity so the
/// hardware never scans past the mapping), then the plane is programmed
/// `CUR_CTL` → `CUR_POS` → `CUR_BASE`, with `CUR_BASE` last so it arms only after
/// the mode and position are in place. Returns `false` — disturbing nothing — if
/// the cursor is unbound, the image is empty or too large, or any allocation,
/// mapping, or placement step fails.
///
/// No `SpinLock` is held across the page allocation, GGTT mapping, image copy, or
/// any MMIO write: the binding is snapshotted under the lock, the lock is
/// dropped, the hardware work runs, then the lock is retaken to record the
/// surface, mode, and hotspot.
pub fn set_image(image: *const u8, len: usize, hot_x: u32, hot_y: u32) -> bool {
    // Reject an empty or null image before any hardware work.
    if image.is_null() || len == 0 {
        return false;
    }

    // ARGB8888 is four bytes per pixel; a square cursor's side is the integer
    // square root of the pixel count. A side outside the supported range yields
    // no mode and declines.
    let pixels = (len / 4) as u64;
    let side = integer_sqrt(pixels);
    let Some(mode) = cursor_config::mode_for_side(side) else {
        return false;
    };

    // Snapshot the binding under the lock, then drop it before any hardware
    // access.
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

    // Copy the ARGB image into the WC surface (allocated at init), bounded by both
    // the image length and the surface capacity so the cursor plane never scans
    // past our mapping.
    let copy_len = core::cmp::min(len as u64, CURSOR_SURFACE_BYTES) as usize;
    ptr_buf::copy_bytes(surf_virt as *mut u8, image, copy_len);

    // Program the cursor plane: mode, then position, then CUR_BASE last to arm.
    // Only CUR_CTL/CUR_POS/CUR_BASE of the active pipe are written.
    mmio.write::<u32>(
        regs::cur_ctl(pipe),
        cursor_config::cur_ctl_value(mode, true, display_ip_version),
    );
    mmio.write::<u32>(
        regs::cur_pos(pipe),
        cursor_config::cur_pos_pack(x as i32 - hot_x as i32, y as i32 - hot_y as i32),
    );
    // CUR_BASE last: latches the surface address and arms the cursor.
    mmio.write::<u32>(regs::cur_base(pipe), surf_ggtt);

    // Retake the lock to record the allocated surface, the programmed mode, and
    // the active hotspot for subsequent moves and uploads.
    if let Some(state) = CURSOR_STATE.lock().as_mut() {
        state.mode = Some(mode);
        state.hot_x = hot_x;
        state.hot_y = hot_y;
    }
    true
}

/// Move the hardware cursor to `(x, y)`.
///
/// Stores the new position, rewrites `CUR_POS` with the hotspot-adjusted
/// coordinate (a negative result places the cursor partly off the top/left edge),
/// then re-arms the plane with `CUR_BASE`. `CUR_POS` is double-buffered and does
/// NOT latch on its own — it takes effect only when the cursor arms `CUR_BASE`,
/// so a `CUR_POS`-only write would leave the cursor frozen at its last armed
/// position. Re-issuing `CUR_BASE` (the same surface, so the image is unchanged
/// and nothing flickers) latches the new position. Only the cursor plane's
/// `CUR_POS`/`CUR_BASE` are written. Returns `false` when the cursor is unbound.
/// The binding is
/// snapshotted (and the position recorded) under the lock, which is dropped
/// before the MMIO writes.
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
    // Lock dropped above. CUR_POS sets the new position into the shadow; CUR_BASE
    // last arms the cursor plane so the position latches at the next vblank.
    mmio.write::<u32>(
        regs::cur_pos(pipe),
        cursor_config::cur_pos_pack(x as i32 - hot_x as i32, y as i32 - hot_y as i32),
    );
    mmio.write::<u32>(regs::cur_base(pipe), surf_ggtt);
    true
}

/// Whether the hardware cursor plane is available to the scanout front-end.
pub fn available() -> bool {
    true
}

/// Allocate the kernel-owned Write-Combining cursor surface and GGTT-map it
/// strictly above both the firmware framebuffer and the primary surface.
///
/// Returns the surface's GGTT byte address and Write-Combining virtual address,
/// or `None` (freeing any partial allocation) when the GGTT bank is unavailable,
/// the backing cannot be allocated, no GGTT room exists above the primary, or the
/// PTE write fails. Reads the primary `PLANE_SURF` only to choose a placement
/// above it — never writes the primary plane.
fn allocate_surface(mmio: &MmioRegion, pipe: Pipe) -> Option<(u32, u64)> {
    // The GGTT page-table bank, needed to write the cursor surface's PTEs, and
    // the total GGTT-addressable byte range the placement must fit within.
    let bank = mmio_map::ggtt_bank(mmio)?;
    let ggtt_total = (bank.size() as u64 / regs::GGTT_PTE_BYTES as u64) * PAGE_SIZE_4KB;

    // Allocate the cursor backing as Write-Combining: the cursor plane scans it
    // directly from RAM, so its pixels must bypass the WriteBack cache.
    let (phys, virt) = fb_mem::alloc_wc_scanout(CURSOR_SURFACE_PAGES)?;

    // The live primary `PLANE_SURF` is the primary's current GGTT base, itself
    // already placed above the firmware framebuffer. Reserving past the largest
    // possible primary and rounding up puts the cursor strictly above both, so
    // its PTEs never collide with the firmware or primary surfaces. This is a
    // read for placement only; the primary plane is never written.
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

/// Floor of the integer square root of `value`, by Newton's method over `u64`.
///
/// Pure integer arithmetic (no floating point, so the kernel's soft-float
/// guarantee holds): used to recover a square cursor's side length from its ARGB
/// pixel count.
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
