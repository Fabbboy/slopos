use core::ffi::c_int;

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
