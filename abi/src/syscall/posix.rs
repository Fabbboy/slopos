// =============================================================================
// Clock constants
// =============================================================================

/// Monotonic clock — nanoseconds since boot, never adjusted.
pub const CLOCK_MONOTONIC: u64 = 0;

/// Realtime clock — currently aliases [`CLOCK_MONOTONIC`] (no RTC source yet).
pub const CLOCK_REALTIME: u64 = 1;

// =============================================================================
// Socket option constants
// =============================================================================

/// Socket option level: generic socket options.
pub const SOL_SOCKET: i32 = 1;
/// Socket option level: TCP protocol options.
pub const IPPROTO_TCP: i32 = 6;

/// Allow local address reuse.
pub const SO_REUSEADDR: i32 = 2;
/// Retrieve and clear pending socket error.
pub const SO_ERROR: i32 = 4;
/// Send buffer size in bytes.
pub const SO_SNDBUF: i32 = 7;
/// Receive buffer size in bytes.
pub const SO_RCVBUF: i32 = 8;
/// Enable keepalive probes.
pub const SO_KEEPALIVE: i32 = 9;
/// Receive timeout in milliseconds (as u64).
pub const SO_RCVTIMEO: i32 = 20;
/// Send timeout in milliseconds (as u64).
pub const SO_SNDTIMEO: i32 = 21;

/// Disable Nagle's algorithm (TCP only).
pub const TCP_NODELAY: i32 = 1;

// =============================================================================
// Shutdown constants
// =============================================================================

/// Disallow further receives.
pub const SHUT_RD: i32 = 0;
/// Disallow further sends.
pub const SHUT_WR: i32 = 1;
/// Disallow further sends and receives.
pub const SHUT_RDWR: i32 = 2;
// =============================================================================
// mmap constants
// =============================================================================

/// Protection flags for mmap/mprotect
pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

/// Mapping flags for mmap
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED: u64 = 0x10;

// =============================================================================
// fcntl constants
// =============================================================================

pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const FD_CLOEXEC: u64 = 1;

pub const O_NONBLOCK: u64 = 0x800;
pub const O_NOCTTY: u64 = 0x100;
pub const O_CLOEXEC: u64 = 0x80_000;

// =============================================================================
// SCM_RIGHTS — fd passing over Unix sockets
// =============================================================================

/// Ancillary data type: pass file descriptors.
pub const SCM_RIGHTS: u32 = 1;

/// Maximum number of file descriptors in a single sendmsg ancillary payload.
pub const SCM_MAX_FDS: usize = 4;

/// User-space message header for sendmsg/recvmsg.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MsgHdr {
    /// Pointer to data buffer.
    pub iov_base: u64,
    /// Data buffer length.
    pub iov_len: u64,
    /// Pointer to ancillary (control) data buffer.
    pub control: u64,
    /// Ancillary data buffer length (input: capacity, output: actual).
    pub control_len: u64,
}

/// Ancillary data header (simplified POSIX cmsghdr).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CmsgHdr {
    /// Total length including this header and data.
    pub cmsg_len: u32,
    /// Originating protocol (SOL_SOCKET).
    pub cmsg_level: u32,
    /// Protocol-specific type (SCM_RIGHTS).
    pub cmsg_type: u32,
    // Followed by i32[] of fd numbers (up to SCM_MAX_FDS).
}

// =============================================================================
// lseek whence constants
// =============================================================================

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

pub const POLLIN: u16 = 0x0001;
pub const POLLPRI: u16 = 0x0002;
pub const POLLOUT: u16 = 0x0004;
pub const POLLERR: u16 = 0x0008;
pub const POLLHUP: u16 = 0x0010;
pub const POLLNVAL: u16 = 0x0020;

pub const FDSET_WORD_BITS: usize = 64;

// =============================================================================
// clone flags — Linux-compatible values
// =============================================================================

/// Child and parent share the same virtual address space.
pub const CLONE_VM: u64 = 0x0000_0100;
/// Child and parent share the same filesystem information (cwd, root).
pub const CLONE_FS: u64 = 0x0000_0200;
/// Child and parent share the same file descriptor table.
pub const CLONE_FILES: u64 = 0x0000_0400;
/// Child and parent share the same signal handler table.
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
/// Write the child's TID into the parent's memory at `parent_tid`.
pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
/// Write the child's TID into the child's memory at `child_tid`.
pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
/// Clear the child's TID at `child_tid` on exit (for futex-based join).
pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
/// Set the TLS (FS_BASE) for the new thread.
pub const CLONE_SETTLS: u64 = 0x0008_0000;
/// New thread shares the parent's thread group (POSIX thread semantics).
pub const CLONE_THREAD: u64 = 0x0001_0000;

/// Mask of all clone flags that SlopOS currently recognises.
pub const CLONE_SUPPORTED_MASK: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_SETTLS
    | CLONE_THREAD;

/// Futex operations
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;
/// arch_prctl sub-commands (Linux-compatible values)
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;
