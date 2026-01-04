//! Shared memory constants and types

/// Shared memory access flags
pub const SHM_ACCESS_RO: u32 = 0;
pub const SHM_ACCESS_RW: u32 = 1;

/// Maximum number of shared memory buffers system-wide
pub const MAX_SHM_BUFFERS: usize = 256;

/// Maximum mappings per shared memory buffer
pub const MAX_SHM_MAPPINGS: usize = 8;

/// Syscall numbers for shared memory operations
pub mod syscall {
    pub const SHM_CREATE: u64 = 40;
    pub const SHM_MAP: u64 = 41;
    pub const SHM_UNMAP: u64 = 42;
    pub const SHM_DESTROY: u64 = 43;
    pub const SURFACE_ATTACH: u64 = 44;
    pub const FB_FLIP: u64 = 45;
    pub const DRAIN_QUEUE: u64 = 46;
    pub const SHM_ACQUIRE: u64 = 47;
    pub const SHM_RELEASE: u64 = 48;
    pub const SHM_POLL_RELEASED: u64 = 49;
    pub const SURFACE_FRAME: u64 = 50;
    pub const POLL_FRAME_DONE: u64 = 51;
    pub const MARK_FRAMES_DONE: u64 = 52;
    pub const SHM_GET_FORMATS: u64 = 53;
    pub const SHM_CREATE_WITH_FORMAT: u64 = 54;
    pub const SURFACE_DAMAGE: u64 = 55;
    pub const BUFFER_AGE: u64 = 56;
    pub const SURFACE_SET_ROLE: u64 = 57;
    pub const SURFACE_SET_PARENT: u64 = 58;
    pub const SURFACE_SET_REL_POS: u64 = 59;
    pub const INPUT_POLL: u64 = 60;
    pub const INPUT_HAS_EVENTS: u64 = 61;
    pub const INPUT_SET_FOCUS: u64 = 62;
}
