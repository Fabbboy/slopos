//! Compositor output buffer and frame metrics.

use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{DisplayInfo, ShmBuffer, window};

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum RenderMode {
    Full,
    Partial,
}

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

    pub fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        let slice = self.buffer.as_mut_slice();
        DrawBuffer::new(slice, self.width, self.height, self.pitch, self.bytes_pp)
    }

    pub fn present(&self, damage: &[DamageRect]) -> bool {
        window::fb_flip_damage(self.buffer.fd() as u32, damage) == 0
    }
}

use crate::syscall::UserWindowInfo;
use crate::theme::{SHADOW_OFFSET_Y, SHADOW_SPREAD, TITLE_BAR_HEIGHT};

pub const WINDOW_STATE_MINIMIZED: u8 = 1;

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
            width: w.effective_width(),
            height: w.effective_height(),
            visible: w.state != WINDOW_STATE_MINIMIZED,
        }
    }

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
    /// Counters as of the last report, so [`FrameMetrics::take_window`] can
    /// answer in deltas.
    reported_frames: u64,
    reported_bytes: u64,
    /// Gates only whether [`take_window`](FrameMetrics::take_window) answers;
    /// recording stays on either way, so a quiet run accounts identically.
    reporting: bool,
    reported_at_ms: u64,
}

impl FrameMetrics {
    pub fn new(reporting: bool) -> Self {
        Self {
            reporting,
            full_redraw_frames: 0,
            partial_redraw_frames: 0,
            total_bytes_copied: 0,
            late_frames: 0,
            dropped_presents: 0,
            frame_times: [0; FRAME_METRICS_WINDOW],
            frame_times_count: 0,
            frame_times_cursor: 0,
            reported_frames: 0,
            reported_bytes: 0,
            reported_at_ms: 0,
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

    /// Frames drawn and bytes copied since the previous report, once per
    /// `interval_ms`. Deltas rather than totals, because cumulative counters
    /// are dominated by the full redraws of the first second. A frame that
    /// painted nothing never reaches `record`, so `frames=0` means idle.
    pub fn take_window(&mut self, now_ms: u64, interval_ms: u64) -> Option<(u64, u64)> {
        if !self.reporting {
            return None;
        }
        if now_ms.saturating_sub(self.reported_at_ms) < interval_ms {
            return None;
        }
        let frames = self.full_redraw_frames + self.partial_redraw_frames;
        let report = (
            frames.saturating_sub(self.reported_frames),
            self.total_bytes_copied.saturating_sub(self.reported_bytes),
        );
        self.reported_frames = frames;
        self.reported_bytes = self.total_bytes_copied;
        self.reported_at_ms = now_ms;
        Some(report)
    }
}

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
