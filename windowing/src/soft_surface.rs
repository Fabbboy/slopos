//! Software rendering surface backed by shared memory.
//!
//! `SoftSurface` owns a pair of [`MemfdBuffer`]s and presents frames to the
//! compositor via the display protocol. It implements [`RenderSurface`] so it
//! can be used as a drop-in CPU rendering backend for any compositor-managed
//! window.
//!
//! # Double buffering
//!
//! The surface keeps two buffers and never draws into one the compositor is
//! still compositing from. Each frame is drawn into a free buffer, attached, and
//! committed; the compositor then returns the previous buffer with a
//! `BufferRelease` event ([`release_buffer`](SoftSurface::release_buffer)),
//! marking it drawable again. Each buffer's fd is sent only on first use; later
//! frames re-select the slot by id, so the compositor keeps a stable mapping per
//! slot.
//!
//! Tear-free operation requires `BufferRelease` events to reach
//! [`release_buffer`](SoftSurface::release_buffer). [`Window`](crate::window::Window)
//! forwards them automatically; a consumer polling the socket directly must
//! forward them itself, or both buffers stay in flight and the surface falls back
//! to single-buffer in-place updates.
//!
//! This type is the rendering half of the windowing/rendering split.
//! [`Surface`](crate::surface::Surface) handles the windowing lifecycle.

use slopos_abi::pixel::PixelFormat;
use slopos_gfx::{DrawBuffer, RenderError, RenderSurface};
use slopos_protocol::types::SurfaceId;

use crate::connection::ProtocolHandle;
use crate::memfd_buf::MemfdBuffer;
use crate::surface::SurfaceError;

/// Number of buffers cycled by the surface (double buffering).
const NUM_BUFFERS: usize = 2;

/// Age at or past which a slot's contents are treated as undefined.
const AGE_UNDEFINED: u32 = u32::MAX;

/// A CPU/SHM rendering backend for compositor-managed surfaces.
pub struct SoftSurface {
    handle: ProtocolHandle,
    bufs: [MemfdBuffer; NUM_BUFFERS],
    /// Whether the compositor has been sent this slot's fd yet.
    registered: [bool; NUM_BUFFERS],
    /// Whether this slot is committed and awaiting a `BufferRelease`.
    busy: [bool; NUM_BUFFERS],
    /// Frames presented since each slot last held a presented frame, saturating
    /// at `AGE_UNDEFINED`. Backs [`buffer_age`](SoftSurface::buffer_age).
    age: [u32; NUM_BUFFERS],
    /// The slot `frame()` last handed out / `present()` will commit.
    current: usize,
    surface_id: SurfaceId,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

impl SoftSurface {
    /// Create a new software rendering surface for a compositor `surface_id`.
    ///
    /// Allocates the double-buffer pair. Nothing is attached or committed yet —
    /// the first [`present`](RenderSurface::present) registers buffer 0 and makes
    /// the surface visible.
    pub fn new(
        handle: ProtocolHandle,
        surface_id: SurfaceId,
        pixel_format: PixelFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::BadSize);
        }

        let bytes_pp = pixel_format.bytes_per_pixel();
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(SurfaceError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(SurfaceError::BadSize)?;

        let bufs = [
            MemfdBuffer::create(buffer_size).map_err(|_| SurfaceError::ShmFailed)?,
            MemfdBuffer::create(buffer_size).map_err(|_| SurfaceError::ShmFailed)?,
        ];

        Ok(Self {
            handle,
            bufs,
            registered: [false; NUM_BUFFERS],
            busy: [false; NUM_BUFFERS],
            age: [AGE_UNDEFINED; NUM_BUFFERS],
            current: 0,
            surface_id,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
        })
    }

    /// Age of the buffer the next [`frame()`](RenderSurface::frame) will hand
    /// out, following the `EGL_EXT_buffer_age` convention: `n > 0` means it
    /// still holds the frame presented `n` frames ago, so the caller may
    /// repaint only the damage accumulated since then; `0` means its contents
    /// are undefined and the caller must repaint it in full.
    ///
    /// Queried before `frame()` rather than after, so a caller can decide how
    /// much to paint before it holds the buffer borrow. `frame()` picks the
    /// same slot this reads, by the same rule.
    pub fn buffer_age(&self) -> u32 {
        let age = self.age[self.pick_slot()];
        if age >= AGE_UNDEFINED { 0 } else { age }
    }

    /// Mark a buffer slot drawable again after the compositor releases it.
    /// Called by [`Window`](crate::window::Window) on a `BufferRelease` event.
    pub fn release_buffer(&mut self, buffer_id: u32) {
        if let Some(b) = self.busy.get_mut(buffer_id as usize) {
            *b = false;
        }
    }

    /// Choose the slot to draw the next frame into: a free buffer if one exists,
    /// otherwise the current slot (only when both are still in flight — a rare
    /// case that degrades to a single-buffer in-place update for that frame).
    fn pick_slot(&self) -> usize {
        let other = 1 - self.current;
        if !self.busy[other] {
            other
        } else if !self.busy[self.current] {
            self.current
        } else {
            self.current
        }
    }

    /// Attach the current buffer (register its fd the first time, else re-select
    /// the slot), then damage `(x, y, w, h)` and commit. Marks the slot in
    /// flight until the compositor releases it.
    fn attach_and_commit(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let slot = self.current;
        let first_use = !self.registered[slot];
        {
            let mut client = self.handle.borrow_client();
            if first_use {
                let _ = client.surface_attach_buffer(
                    self.surface_id,
                    slot as u32,
                    self.bufs[slot].fd(),
                    self.width,
                    self.height,
                );
            } else {
                let _ = client.surface_select_buffer(
                    self.surface_id,
                    slot as u32,
                    self.width,
                    self.height,
                );
            }
            let _ = client.surface_damage(self.surface_id, x, y, w, h);
            let _ = client.surface_commit(self.surface_id);
        }
        self.registered[slot] = true;
        self.busy[slot] = true;

        // This slot now holds the frame just presented; every other slot is one
        // frame further from being current.
        for (i, age) in self.age.iter_mut().enumerate() {
            if i == slot {
                *age = 1;
            } else {
                *age = age.saturating_add(1);
            }
        }
    }
}

impl RenderSurface for SoftSurface {
    fn frame(&mut self) -> Option<DrawBuffer<'_>> {
        self.current = self.pick_slot();
        let mut buf = DrawBuffer::new(
            self.bufs[self.current].as_mut_slice(),
            self.width,
            self.height,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    fn present(&mut self) {
        self.attach_and_commit(0, 0, self.width as i32, self.height as i32);
    }

    fn present_region(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.attach_and_commit(x, y, w, h);
    }

    fn resize(&mut self, new_width: u32, new_height: u32) -> Result<(), RenderError> {
        if new_width == 0 || new_height == 0 {
            return Err(RenderError::BadSize);
        }
        if new_width == self.width && new_height == self.height {
            return Ok(());
        }

        let new_pitch = (new_width as usize)
            .checked_mul(self.bytes_pp as usize)
            .ok_or(RenderError::BadSize)?;
        let buffer_size = new_pitch
            .checked_mul(new_height as usize)
            .ok_or(RenderError::BadSize)?;

        let bufs = [
            MemfdBuffer::create(buffer_size).map_err(|_| RenderError::BufferUnavailable)?,
            MemfdBuffer::create(buffer_size).map_err(|_| RenderError::BufferUnavailable)?,
        ];

        // New fds must be re-registered; no commit has used them, so neither is
        // in flight. The compositor evicts the old slot mappings when the new
        // fds become current.
        self.bufs = bufs;
        self.registered = [false; NUM_BUFFERS];
        self.busy = [false; NUM_BUFFERS];
        // Fresh allocations hold nothing a partial repaint could build on.
        self.age = [AGE_UNDEFINED; NUM_BUFFERS];
        self.current = 0;
        self.width = new_width;
        self.height = new_height;
        self.pitch = new_pitch;
        Ok(())
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    fn pitch(&self) -> usize {
        self.pitch
    }
}
