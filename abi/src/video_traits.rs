use core::ffi::c_int;

#[repr(C)]
#[derive(Clone, Copy, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoError {
    NoFramebuffer,
    OutOfBounds,
    Invalid,
}

pub type VideoResult = Result<(), VideoError>;

#[inline]
pub fn video_result_from_code(rc: c_int) -> VideoResult {
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}
