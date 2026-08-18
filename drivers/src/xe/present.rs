//! Tear-free damage-scoped double-buffer present.
//!
//! Two linear scanout surfaces are GGTT-mapped strictly ABOVE every other
//! surface. The compositor draws into the installed *draw* buffer; the plane
//! always scans one of the two scanout buffers, never the draw buffer and never
//! the buffer it is currently displaying. A present copies the damaged rows into
//! the *back* scanout buffer, fences the write-combining stores, then flips the
//! plane to it; `PLANE_SURF` latches at the next vblank, so the display only ever
//! switches between two fully-rendered frames.
//!
//! The back buffer was last written two presents ago, so the copy covers this
//! present's box ∪ the previous present's box.
//!
//! This unit writes no register other than the active pipe's plane group, so a
//! present can never disturb the pipe, transcoder, or eDP link.

use core::ffi::c_int;
use slopos_ostd::lock_class;

use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_mm::mmio::MmioRegion;
use slopos_ostd::arch::x86_64::mem_fence::sfence;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::util::ptr_buf;

use super::plane::PlaneProgram;

/// An inclusive, surface-clamped damage bounding box `(x0, y0, x1, y1)`.
type Box = (u32, u32, u32, u32);

/// Buffer pointers are retained as `usize` virtual addresses rather than raw
/// pointers so the state stays `Send` for the static below without any `unsafe`
/// impl. They are turned back into pointers only locally, inside [`present`].
#[derive(Clone)]
struct PresentState {
    mmio: MmioRegion,
    /// The plane-group program re-issued on every flip (its width/height/pitch is
    /// the shared geometry of all three buffers).
    program: PlaneProgram,
    /// Virtual address of the compositor's installed draw buffer.
    draw_virt: usize,
    /// Virtual addresses of the two GGTT-mapped scanout buffers.
    scan_virt: [usize; 2],
    /// GGTT byte addresses the plane can scan; `scan_ggtt[front]` is armed now.
    scan_ggtt: [u32; 2],
    /// Index (0/1) of the scanout buffer the plane is currently scanning.
    front: usize,
    /// Damage box copied on the previous present; `None` before the first.
    prev_damage: Option<Box>,
}

/// `None` until xe owns the double-buffered scanout.
static PRESENT_STATE: SpinLock<Option<PresentState>> =
    SpinLock::new(None, lock_class!("PRESENT_STATE", LOCK_LEVEL_RESOURCE));

/// The caller has already allocated both scanout buffers, GGTT-mapped them
/// strictly above every other surface, seeded both from the draw buffer, and
/// armed the plane on `scan0_ggtt` (so `front` starts at 0). Writes no hardware
/// register.
pub fn install(
    mmio: &MmioRegion,
    program: PlaneProgram,
    draw_virt: *const u8,
    scan0_virt: *mut u8,
    scan1_virt: *mut u8,
    scan0_ggtt: u32,
    scan1_ggtt: u32,
) {
    *PRESENT_STATE.lock() = Some(PresentState {
        mmio: mmio.clone(),
        program,
        draw_virt: draw_virt as usize,
        scan_virt: [scan0_virt as usize, scan1_virt as usize],
        scan_ggtt: [scan0_ggtt, scan1_ggtt],
        front: 0,
        prev_damage: None,
    });
}

/// Present the damaged region to the panel, tear-free. A null `damage` pointer
/// or zero `count` presents the whole surface. The committed state is snapshotted
/// under the lock, which is dropped before any copy or MMIO.
///
/// Returns `0` once a present has been issued (including a no-op when the damage
/// clamps to nothing), or `-1` when xe owns no scanout.
pub fn present(damage: *const DamageRect, count: u32) -> c_int {
    let state = {
        let guard = PRESENT_STATE.lock();
        match guard.as_ref() {
            Some(state) => state.clone(),
            None => return -1,
        }
    };

    let Some(curr) = coalesce_damage(damage, count, state.program.width, state.program.height)
    else {
        return 0;
    };

    // The back buffer is the one the plane is NOT scanning, and is two presents
    // stale — hence the union with the previous box.
    let back = 1 - state.front;
    let (x0, y0, x1, y1) = union_box(curr, state.prev_damage);

    // Draw and scanout buffers share pitch and pixel layout, so a row lands at
    // the same byte offset in each; every offset is bounded by `pitch * height`
    // because the box is clamped inside the surface and `pitch >= width * 4`.
    let span_bytes = (x1 - x0 + 1) as usize * 4;
    let pitch = state.program.pitch_bytes as usize;
    let draw_virt = state.draw_virt;
    let back_virt = state.scan_virt[back];
    for y in y0..=y1 {
        let row_off = pitch * y as usize + x0 as usize * 4;
        let src = (draw_virt + row_off) as *const u8;
        let dst = (back_virt + row_off) as *mut u8;
        ptr_buf::copy_bytes(dst, src, span_bytes);
    }

    // The display engine reads system RAM directly, not the CPU cache: without
    // this drain the plane can latch a partially-written buffer.
    sfence();

    state.program.flip(&state.mmio, state.scan_ggtt[back]);

    // A second present that raced the snapshot only risks re-rendering a buffer,
    // never a crash, and self-corrects on the following present.
    if let Some(live) = PRESENT_STATE.lock().as_mut() {
        live.front = back;
        live.prev_damage = Some(curr);
    }
    0
}

fn union_box(curr: Box, prev: Option<Box>) -> Box {
    match prev {
        None => curr,
        Some((px0, py0, px1, py1)) => (
            curr.0.min(px0),
            curr.1.min(py0),
            curr.2.max(px1),
            curr.3.max(py1),
        ),
    }
}

/// Coalesce the `count` damage rects at `damage` into one inclusive bounding box
/// `(x0, y0, x1, y1)` clamped to `[0, width) x [0, height)`.
///
/// A null pointer or zero count yields the full surface. An all-invalid damage
/// list, or a degenerate surface, yields `None` (nothing to present). The rect
/// count is capped at `MAX_DAMAGE_REGIONS` so a hostile `count` cannot drive an
/// unbounded borrow.
fn coalesce_damage(damage: *const DamageRect, count: u32, width: u32, height: u32) -> Option<Box> {
    if width == 0 || height == 0 {
        return None;
    }
    let max_x = width as i32 - 1;
    let max_y = height as i32 - 1;

    if damage.is_null() || count == 0 {
        return Some((0, 0, max_x as u32, max_y as u32));
    }

    let len = (count as usize).min(MAX_DAMAGE_REGIONS);
    let (any, min_x, min_y, max_seen_x, max_seen_y) = ptr_buf::with_buf(damage, len, |regions| {
        let (mut min_x, mut min_y, mut max_seen_x, mut max_seen_y) =
            (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut any = false;
        for r in regions {
            if !r.is_valid() {
                continue;
            }
            any = true;
            min_x = min_x.min(r.x0);
            min_y = min_y.min(r.y0);
            max_seen_x = max_seen_x.max(r.x1);
            max_seen_y = max_seen_y.max(r.y1);
        }
        (any, min_x, min_y, max_seen_x, max_seen_y)
    });
    if !any {
        return None;
    }

    let min_x = min_x.clamp(0, max_x);
    let min_y = min_y.clamp(0, max_y);
    let max_seen_x = max_seen_x.clamp(0, max_x);
    let max_seen_y = max_seen_y.clamp(0, max_y);
    if max_seen_x < min_x || max_seen_y < min_y {
        return None;
    }
    Some((
        min_x as u32,
        min_y as u32,
        max_seen_x as u32,
        max_seen_y as u32,
    ))
}
