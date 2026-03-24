//! Compositor output buffer and frame metrics.

use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{DisplayInfo, ShmBuffer, window};

// ── Render mode ─────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum RenderMode {
    Full,
    Partial,
}

// ── Output buffer ───────────────────────────────────────────────────────────

/// Compositor output buffer backed by shared memory.
pub struct CompositorOutput {
    buffer: ShmBuffer,
    pub width: u32,
    pub height: u32,
    pub pitch: usize,
    pub bytes_pp: u8,
}

impl CompositorOutput {
    pub fn new(fb: &DisplayInfo) -> Option<Self> {
        let pitch = fb.pitch as usize;
        let bytes_pp = fb.bytes_per_pixel();
        let size = pitch.checked_mul(fb.height as usize)?;

        if size == 0 || bytes_pp < 3 {
            return None;
        }

        let buffer = ShmBuffer::create(size).ok()?;

        Some(Self {
            buffer,
            width: fb.width,
            height: fb.height,
            pitch,
            bytes_pp,
        })
    }

    /// Get a [`DrawBuffer`] for this output.
    pub fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        let slice = self.buffer.as_mut_slice();
        DrawBuffer::new(slice, self.width, self.height, self.pitch, self.bytes_pp)
    }

    /// Present the output buffer to the framebuffer.
    /// When `damage` is empty this falls back to full-buffer present.
    pub fn present(&self, damage: &[DamageRect]) -> bool {
        window::fb_flip_damage(self.buffer.token(), damage) == 0
    }
}

// ── Window bounds ───────────────────────────────────────────────────────────

use crate::syscall::UserWindowInfo;
use crate::theme::{SHADOW_OFFSET_Y, SHADOW_SPREAD, TITLE_BAR_HEIGHT};

pub const WINDOW_STATE_MINIMIZED: u8 = 1;

/// Bounds of a window snapshot (for damage tracking between frames).
#[derive(Copy, Clone, Default)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
}

impl WindowBounds {
    pub fn from_window(w: &UserWindowInfo) -> Self {
        Self {
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.state != WINDOW_STATE_MINIMIZED,
        }
    }

    /// Get the full window rect including title bar.
    pub fn to_damage_rect(&self) -> DamageRect {
        if !self.visible {
            return DamageRect::invalid();
        }
        DamageRect {
            x0: self.x - SHADOW_SPREAD,
            y0: self.y - TITLE_BAR_HEIGHT + SHADOW_OFFSET_Y - SHADOW_SPREAD,
            x1: self.x + self.width as i32 - 1 + SHADOW_SPREAD,
            y1: self.y + self.height as i32 - 1 + SHADOW_OFFSET_Y + SHADOW_SPREAD,
        }
    }
}

// ── Frame metrics ───────────────────────────────────────────────────────────

const FRAME_METRICS_WINDOW: usize = 128;

pub struct FrameMetrics {
    full_redraw_frames: u64,
    partial_redraw_frames: u64,
    total_bytes_copied: u64,
    late_frames: u64,
    dropped_presents: u64,
    frame_times: [u64; FRAME_METRICS_WINDOW],
    frame_times_count: usize,
    frame_times_cursor: usize,
}

impl FrameMetrics {
    pub fn new() -> Self {
        Self {
            full_redraw_frames: 0,
            partial_redraw_frames: 0,
            total_bytes_copied: 0,
            late_frames: 0,
            dropped_presents: 0,
            frame_times: [0; FRAME_METRICS_WINDOW],
            frame_times_count: 0,
            frame_times_cursor: 0,
        }
    }

    pub fn record(
        &mut self,
        mode: RenderMode,
        bytes_copied: usize,
        frame_time_ms: u64,
        target_frame_ms: u64,
        present_ok: bool,
    ) {
        match mode {
            RenderMode::Full => self.full_redraw_frames = self.full_redraw_frames.saturating_add(1),
            RenderMode::Partial => {
                self.partial_redraw_frames = self.partial_redraw_frames.saturating_add(1)
            }
        }
        self.total_bytes_copied = self.total_bytes_copied.saturating_add(bytes_copied as u64);
        if frame_time_ms > target_frame_ms {
            self.late_frames = self.late_frames.saturating_add(1);
        }
        if !present_ok {
            self.dropped_presents = self.dropped_presents.saturating_add(1);
        }

        self.frame_times[self.frame_times_cursor] = frame_time_ms;
        self.frame_times_cursor = (self.frame_times_cursor + 1) % FRAME_METRICS_WINDOW;
        if self.frame_times_count < FRAME_METRICS_WINDOW {
            self.frame_times_count += 1;
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn estimate_present_bytes(
    _width: u32,
    height: u32,
    bytes_pp: u8,
    pitch: usize,
    mode: RenderMode,
    damage: &[DamageRect],
) -> usize {
    if mode == RenderMode::Full || damage.is_empty() {
        return pitch.saturating_mul(height as usize);
    }

    let mut total = 0usize;
    for rect in damage {
        let clipped = rect.clip(_width as i32, height as i32);
        if !clipped.is_valid() {
            continue;
        }
        let w = (clipped.x1 - clipped.x0 + 1) as usize;
        let h = (clipped.y1 - clipped.y0 + 1) as usize;
        total = total.saturating_add(w.saturating_mul(h).saturating_mul(bytes_pp as usize));
    }
    total
}
