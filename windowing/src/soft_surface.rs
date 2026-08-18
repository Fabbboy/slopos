//! Software rendering surface backed by shared memory.
//!
//! `SoftSurface` owns a pair of [`MemfdBuffer`]s and implements [`RenderSurface`]
//! as a CPU rendering backend for compositor-managed windows;
//! [`Surface`](crate::surface::Surface) handles the windowing lifecycle.
//!
//! `BufferRelease` events must reach
//! [`release_buffer`](SoftSurface::release_buffer) — [`Window`](crate::window::Window)
//! forwards them, but a consumer polling the socket directly must do so itself,
//! or both buffers stay in flight and the surface falls back to single-buffer
//! in-place updates.

use slopos_abi::pixel::PixelFormat;
use slopos_gfx::{DrawBuffer, RenderError, RenderSurface};
use slopos_protocol::types::SurfaceId;

use crate::connection::ProtocolHandle;
use crate::memfd_buf::MemfdBuffer;
use crate::surface::SurfaceError;

const NUM_BUFFERS: usize = 2;

const AGE_UNDEFINED: u32 = u32::MAX;

/// A CPU/SHM rendering backend for compositor-managed surfaces.
pub struct SoftSurface {
    handle: ProtocolHandle,
    bufs: [MemfdBuffer; NUM_BUFFERS],
    /// Whether the compositor has been sent this slot's fd yet.
    registered: [bool; NUM_BUFFERS],
    /// Whether this slot is committed and awaiting a `BufferRelease`.
    busy: [bool; NUM_BUFFERS],
    /// Frames since each slot last held a presented frame; saturates at `AGE_UNDEFINED`.
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
    /// Nothing is attached or committed until the first
    /// [`present`](RenderSurface::present).
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

    /// Age of the buffer the next [`frame()`](RenderSurface::frame) will hand out,
    /// per the `EGL_EXT_buffer_age` convention: `n > 0` means it still holds the
    /// frame presented `n` frames ago; `0` means undefined contents, repaint in full.
    ///
    /// `frame()` picks the same slot this reads, by the same rule.
    pub fn buffer_age(&self) -> u32 {
        let age = self.age[self.pick_slot()];
        if age >= AGE_UNDEFINED { 0 } else { age }
    }

    /// Mark a buffer slot drawable again after the compositor releases it.
    pub fn release_buffer(&mut self, buffer_id: u32) {
        if let Some(b) = self.busy.get_mut(buffer_id as usize) {
            *b = false;
        }
    }

    /// Choose the slot to draw the next frame into: a free buffer, else the current one.
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

    /// A buffer's fd is sent only on first use; later frames re-select the slot by
    /// id, so the compositor keeps a stable mapping per slot.
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

        // New fds must be re-registered; the compositor evicts the old slot
        // mappings when the new fds become current.
        self.bufs = bufs;
        self.registered = [false; NUM_BUFFERS];
        self.busy = [false; NUM_BUFFERS];
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
