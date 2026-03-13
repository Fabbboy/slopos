//! Syscall number definitions (kernel-userland ABI).
//!
//! This module is the **single source of truth** for all syscall numbers.
//! Both kernel and userland import from here to ensure ABI consistency.
//!
//! # Adding New Syscalls
//!
//! 1. Add the constant here with the next available number
//! 2. Use the `SYSCALL_` prefix for consistency
//! 3. Group with related syscalls under the appropriate section
//! 4. Update the dispatch table in `core/src/syscall/handlers.rs`
//!
//! # Number Allocation
//!
//! Numbers are not required to be contiguous. Gaps exist from removed or
//! reserved syscalls. New syscalls should use the next highest number
//! to avoid ABI breakage with existing userland binaries.

// =============================================================================
// TTY types
// =============================================================================

/// Strongly-typed index into the global TTY table.
///
/// Wrapping the raw `u8` slot number prevents accidental mix-ups with other
/// small integer types (task IDs, pgrp IDs, etc.) at API boundaries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct TtyIndex(pub u8);

// =============================================================================
// Core syscalls
// =============================================================================

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_FB_INFO: u64 = 6;

// =============================================================================
// Random / Roulette
// =============================================================================

pub const SYSCALL_RANDOM_NEXT: u64 = 12;
pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
pub const SYSCALL_ROULETTE_DRAW: u64 = 24;

// =============================================================================
// Filesystem
// =============================================================================

pub const SYSCALL_FS_OPEN: u64 = 14;
pub const SYSCALL_FS_CLOSE: u64 = 15;
pub const SYSCALL_FS_READ: u64 = 16;
pub const SYSCALL_FS_WRITE: u64 = 17;
pub const SYSCALL_FS_STAT: u64 = 18;
pub const SYSCALL_FS_MKDIR: u64 = 19;
pub const SYSCALL_FS_UNLINK: u64 = 20;
pub const SYSCALL_FS_LIST: u64 = 21;

// =============================================================================
// System
// =============================================================================

pub const SYSCALL_SYS_INFO: u64 = 22;
pub const SYSCALL_HALT: u64 = 23;
pub const SYSCALL_NET_SCAN: u64 = 120;
pub const SYSCALL_NET_INFO: u64 = 123;
pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
pub const SYSCALL_GET_TIME_MS: u64 = 39;
pub const SYSCALL_REBOOT: u64 = 85;

/// Query a high-resolution clock.
///
/// # Arguments (via registers)
/// * rdi (arg0): clock ID (`CLOCK_MONOTONIC` = 0)
/// * rsi (arg1): pointer to [`Timespec`] output struct
///
/// # Returns
/// * 0 on success
/// * -EINVAL: unknown clock ID
/// * -EFAULT: invalid pointer
pub const SYSCALL_CLOCK_GETTIME: u64 = 125;

// =============================================================================
// Clock constants
// =============================================================================

/// Monotonic clock — nanoseconds since boot, never adjusted.
pub const CLOCK_MONOTONIC: u64 = 0;

// =============================================================================
// Window management
// =============================================================================

pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;
pub const SYSCALL_SET_WINDOW_POSITION: u64 = 31;
pub const SYSCALL_SET_WINDOW_STATE: u64 = 32;
pub const SYSCALL_RAISE_WINDOW: u64 = 33;
pub const SYSCALL_SET_CURSOR_SHAPE: u64 = 118;

// =============================================================================
// Input events
// =============================================================================

pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_INPUT_POLL: u64 = 60;
pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;
pub const SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET: u64 = 65;
pub const SYSCALL_INPUT_GET_POINTER_POS: u64 = 66;
pub const SYSCALL_INPUT_GET_BUTTON_STATE: u64 = 67;
pub const SYSCALL_INPUT_REQUEST_CLOSE: u64 = 84;
pub const SYSCALL_CLIPBOARD_COPY: u64 = 116;
pub const SYSCALL_CLIPBOARD_PASTE: u64 = 117;

// =============================================================================
// Surface / Compositor
// =============================================================================

pub const SYSCALL_SURFACE_COMMIT: u64 = 38;
pub const SYSCALL_SURFACE_ATTACH: u64 = 44;
pub const SYSCALL_SURFACE_FRAME: u64 = 50;
pub const SYSCALL_POLL_FRAME_DONE: u64 = 51;
pub const SYSCALL_MARK_FRAMES_DONE: u64 = 52;
pub const SYSCALL_SURFACE_DAMAGE: u64 = 55;
pub const SYSCALL_BUFFER_AGE: u64 = 56;
pub const SYSCALL_SURFACE_SET_ROLE: u64 = 57;
pub const SYSCALL_SURFACE_SET_PARENT: u64 = 58;
pub const SYSCALL_SURFACE_SET_REL_POS: u64 = 59;
pub const SYSCALL_SURFACE_SET_TITLE: u64 = 63;

// =============================================================================
// Shared memory
// =============================================================================

pub const SYSCALL_SHM_CREATE: u64 = 40;
pub const SYSCALL_SHM_MAP: u64 = 41;
pub const SYSCALL_SHM_UNMAP: u64 = 42;
pub const SYSCALL_SHM_DESTROY: u64 = 43;
pub const SYSCALL_FB_FLIP: u64 = 45;
pub const SYSCALL_DRAIN_QUEUE: u64 = 46;
pub const SYSCALL_SHM_ACQUIRE: u64 = 47;
pub const SYSCALL_SHM_RELEASE: u64 = 48;
pub const SYSCALL_SHM_POLL_RELEASED: u64 = 49;
pub const SYSCALL_SHM_GET_FORMATS: u64 = 53;
pub const SYSCALL_SHM_CREATE_WITH_FORMAT: u64 = 54;

// =============================================================================
// Task management
// =============================================================================

/// Spawn a new userspace task by absolute executable path.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to path bytes (NUL-terminated or explicit length)
/// * rsi (arg1): path length in bytes
/// * rdx (arg2): task priority (`u8`)
/// * r10 (arg3): task flags (`u16`, kernel enforces user-mode bit)
/// * r8  (arg4): argv pointer (null-terminated array of null-terminated string pointers, or 0)
/// * r9  (arg5): argc count (number of args, or 0)
///
/// # Returns
/// * positive task ID on success
/// * negative `ExecError` code on failure
pub const SYSCALL_SPAWN_PATH: u64 = 64;
pub const SYSCALL_WAITPID: u64 = 68;
pub const SYSCALL_TERMINATE_TASK: u64 = 69;

// =============================================================================
// Process execution
// =============================================================================

/// Execute an ELF binary from the filesystem, replacing the current process.
///
/// # Arguments (via registers)
/// * rdi (arg0): Pointer to null-terminated path string
/// * rsi (arg1): argv pointer -- null-terminated array of null-terminated string pointers.
///               0 means no argv and preserves legacy behavior.
/// * rdx (arg2): envp pointer -- null-terminated array of null-terminated `KEY=VALUE\0`
///               string pointers. 0 means no envp and preserves legacy behavior.
///
/// # Returns
/// * Does not return on success (process image is replaced)
/// * -ENOENT: File not found
/// * -ENOEXEC: Not a valid ELF executable
/// * -ENOMEM: Insufficient memory
/// * -EFAULT: Invalid pointer
pub const SYSCALL_EXEC: u64 = 70;

// =============================================================================
// Memory management
// =============================================================================

pub const SYSCALL_BRK: u64 = 71;

// =============================================================================
// Process management
// =============================================================================

/// Fork the current process, creating a child with copy-on-write address space.
///
/// # Returns
/// * In parent: child's task ID (positive)
/// * In child: 0
/// * On error: negative error code
pub const SYSCALL_FORK: u64 = 72;

// =============================================================================
// SMP / CPU Affinity
// =============================================================================

pub const SYSCALL_GET_CPU_COUNT: u64 = 80;
pub const SYSCALL_GET_CURRENT_CPU: u64 = 81;
pub const SYSCALL_SET_CPU_AFFINITY: u64 = 82;
pub const SYSCALL_GET_CPU_AFFINITY: u64 = 83;

// =============================================================================
// Process identity
// =============================================================================

pub const SYSCALL_GETPID: u64 = 86;
pub const SYSCALL_GETPPID: u64 = 87;
pub const SYSCALL_GETUID: u64 = 88;
pub const SYSCALL_GETGID: u64 = 89;
pub const SYSCALL_GETEUID: u64 = 90;
pub const SYSCALL_GETEGID: u64 = 91;

// =============================================================================
// Filesystem process context
// =============================================================================

/// Change the current working directory of the calling task.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to null-terminated path string
///
/// # Returns
/// * 0 on success
/// * -ENOENT: path not found
/// * -ENOTDIR: path is not a directory
/// * -EFAULT: invalid pointer
pub const SYSCALL_CHDIR: u64 = 124;

/// Get the current working directory of the calling task.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to user buffer
/// * rsi (arg1): buffer size in bytes
///
/// # Returns
/// * Length of cwd (including null terminator) on success
/// * -ERANGE: buffer too small
/// * -EFAULT: invalid pointer
pub const SYSCALL_GETCWD: u64 = 121;

/// Atomically rename/move a file or directory.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to null-terminated old path string
/// * rsi (arg1): pointer to null-terminated new path string
///
/// # Returns
/// * 0 on success
/// * -ENOENT: source not found
/// * -EXDEV: cross-device rename not supported
/// * -ENOTSUP: filesystem doesn't support rename
/// * -EFAULT: invalid pointer
pub const SYSCALL_RENAME: u64 = 122;

// =============================================================================
// Socket operations
// =============================================================================

/// Create a socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): domain (AF_INET = 2)
/// * rsi (arg1): type (SOCK_STREAM = 1, SOCK_DGRAM = 2)
/// * rdx (arg2): protocol (0 = auto-select)
///
/// # Returns
/// * File descriptor on success
/// * Negative errno on failure
pub const SYSCALL_SOCKET: u64 = 126;

/// Bind a socket to a local address.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to SockAddrIn struct
/// * rdx (arg2): address length
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_BIND: u64 = 127;

/// Mark a socket as listening for incoming connections.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): backlog (ignored for now)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_LISTEN: u64 = 128;

/// Accept an incoming connection on a listening socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): listening socket file descriptor
/// * rsi (arg1): pointer to SockAddrIn for peer address (or 0)
/// * rdx (arg2): pointer to address length (or 0)
///
/// # Returns
/// * New file descriptor for accepted connection on success
/// * Negative errno on failure
pub const SYSCALL_ACCEPT: u64 = 129;

/// Initiate a TCP connection.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to SockAddrIn with remote address
/// * rdx (arg2): address length
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_CONNECT: u64 = 130;

/// Send data on a connected socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to data buffer
/// * rdx (arg2): data length
/// * r10 (arg3): flags (0 for now)
///
/// # Returns
/// * Number of bytes sent on success
/// * Negative errno on failure
pub const SYSCALL_SEND: u64 = 131;

/// Receive data from a connected socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to receive buffer
/// * rdx (arg2): buffer length
/// * r10 (arg3): flags (0 for now)
///
/// # Returns
/// * Number of bytes received on success (0 = connection closed)
/// * Negative errno on failure
pub const SYSCALL_RECV: u64 = 132;
pub const SYSCALL_SENDTO: u64 = 133;
pub const SYSCALL_RECVFROM: u64 = 134;

/// Resolve a hostname to an IPv4 address via the in-kernel DNS client.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to hostname bytes (not NUL-terminated)
/// * rsi (arg1): hostname length in bytes
/// * rdx (arg2): pointer to `[u8; 4]` output for resolved address
///
/// # Returns
/// * 0 on success
/// * -EHOSTUNREACH: DNS resolution failed
/// * -ETIMEDOUT: DNS server did not respond
/// * -EFAULT: invalid pointer
/// * -EINVAL: hostname too long (>253 bytes)
pub const SYSCALL_RESOLVE: u64 = 135;

/// Set a socket option.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): option level (SOL_SOCKET, IPPROTO_TCP)
/// * rdx (arg2): option name (SO_REUSEADDR, SO_RCVBUF, etc.)
/// * r10 (arg3): pointer to option value
/// * r8  (arg4): option value length in bytes
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_SETSOCKOPT: u64 = 136;

/// Get a socket option.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): option level
/// * rdx (arg2): option name
/// * r10 (arg3): pointer to output buffer for option value
/// * r8  (arg4): pointer to u32 containing buffer length (updated on return)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_GETSOCKOPT: u64 = 137;

/// Shut down part of a full-duplex connection.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): how (SHUT_RD=0, SHUT_WR=1, SHUT_RDWR=2)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_SHUTDOWN: u64 = 138;

/// Revoke access to the calling process's controlling terminal.
///
/// All other file descriptors referencing this TTY become invalid —
/// subsequent I/O returns `EIO`.  Only callable by a process that holds
/// a controlling terminal; returns `-EPERM` if the caller has no ctty.
///
/// # Returns
/// * 0 on success
/// * -EPERM: caller has no controlling terminal
pub const SYSCALL_VHANGUP: u64 = 139;

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
// Memory management (POSIX)
// =============================================================================

/// Map anonymous memory into the process address space.
///
/// # Arguments (via registers)
/// * rdi (arg0): requested address (hint, or 0 for kernel-chosen)
/// * rsi (arg1): length in bytes (must be > 0, rounded up to page size)
/// * rdx (arg2): protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
/// * r10 (arg3): mapping flags (MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED)
/// * r8  (arg4): file descriptor (must be -1 for MAP_ANONYMOUS)
/// * r9  (arg5): offset (must be 0 for MAP_ANONYMOUS)
///
/// # Returns
/// * Virtual address of the mapping on success
/// * Negative errno on failure (-EINVAL, -ENOMEM)
pub const SYSCALL_MMAP: u64 = 92;

/// Unmap a previously mapped memory region.
///
/// # Arguments (via registers)
/// * rdi (arg0): start address (must be page-aligned)
/// * rsi (arg1): length in bytes (rounded up to page size)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL)
pub const SYSCALL_MUNMAP: u64 = 93;

/// Change protection on a memory region.
///
/// # Arguments (via registers)
/// * rdi (arg0): start address (must be page-aligned)
/// * rsi (arg1): length in bytes (rounded up to page size)
/// * rdx (arg2): new protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -ENOMEM)
pub const SYSCALL_MPROTECT: u64 = 94;

// =============================================================================
// File descriptor operations
// =============================================================================

pub const SYSCALL_DUP: u64 = 95;
pub const SYSCALL_DUP2: u64 = 96;
pub const SYSCALL_DUP3: u64 = 97;
pub const SYSCALL_FCNTL: u64 = 98;
pub const SYSCALL_LSEEK: u64 = 99;
pub const SYSCALL_FSTAT: u64 = 100;
pub const SYSCALL_POLL: u64 = 108;
pub const SYSCALL_SELECT: u64 = 109;
pub const SYSCALL_PIPE: u64 = 110;
pub const SYSCALL_PIPE2: u64 = 111;
pub const SYSCALL_IOCTL: u64 = 112;
pub const SYSCALL_SETPGID: u64 = 113;
pub const SYSCALL_GETPGID: u64 = 114;
pub const SYSCALL_SETSID: u64 = 115;

// =============================================================================
// mmap constants
// =============================================================================

/// Protection flags for mmap/mprotect
pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

/// Mapping flags for mmap
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

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserPollFd {
    pub fd: i32,
    pub events: u16,
    pub revents: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
// Missing ioctls.
pub const TCSBRK: u64 = 0x5409;
pub const TCXONC: u64 = 0x540A;
pub const TCFLSH: u64 = 0x540B;
// tcflush() queue selectors.
pub const TCIFLUSH: i32 = 0;
pub const TCOFLUSH: i32 = 1;
pub const TCIOFLUSH: i32 = 2;
// tcflow() action selectors.
pub const TCOOFF: i32 = 0;
pub const TCOON: i32 = 1;
pub const TCIOFF: i32 = 2;
pub const TCION: i32 = 3;
pub const TIOCGPGRP: u64 = 0x540F;
pub const TIOCSPGRP: u64 = 0x5410;
pub const TIOCGPTN: u64 = 0x8004_5430;
pub const TIOCSETD: u64 = 0x5423;
pub const TIOCGETD: u64 = 0x5424;
pub const TIOCGSID: u64 = 0x5429;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;
pub const TIOCSCTTY: u64 = 0x540E;
/// Detach the calling process from its controlling terminal.
/// Linux value: 0x5422.
pub const TIOCNOTTY: u64 = 0x5422;
/// Get the number of bytes available for reading.
/// Linux value: 0x541B (same as TIOCINQ).
pub const FIONREAD: u64 = 0x541B;
/// Get the number of bytes in the output queue.
/// Linux value: 0x5411.
pub const TIOCOUTQ: u64 = 0x5411;
/// Set PTY slave lock state (0=unlock, 1=lock). Master FD only.
/// Linux value: 0x40045431.
pub const TIOCSPTLCK: u64 = 0x4004_5431;
/// Get PTY slave lock state. Returns 0 (unlocked) or 1 (locked).
/// Linux value: 0x80045439.
pub const TIOCGPTLCK: u64 = 0x8004_5439;
/// Enable/disable PTY packet mode on a master FD.
/// Linux value: 0x5420.
pub const TIOCPKT: u64 = 0x5420;

/// Packet mode control byte constants.
/// Normal data follows — no special event.
pub const TIOCPKT_DATA: u8 = 0x00;
/// Slave input queue was flushed.
pub const TIOCPKT_FLUSHREAD: u8 = 0x01;
/// Slave output queue was flushed.
pub const TIOCPKT_FLUSHWRITE: u8 = 0x02;
/// Slave output stopped (XOFF received).
pub const TIOCPKT_STOP: u8 = 0x04;
/// Slave output started (XON received).
pub const TIOCPKT_START: u8 = 0x08;
/// `IXON` cleared on slave termios.
pub const TIOCPKT_NOSTOP: u8 = 0x10;
/// `IXON` set on slave termios.
pub const TIOCPKT_DOSTOP: u8 = 0x20;

pub const N_TTY: u32 = 0;
pub const N_RAW: u32 = 1;

pub const NCCS: usize = 19;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserTermios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

pub const ISIG: u32 = 0x01;
pub const ICANON: u32 = 0x02;
pub const ECHO: u32 = 0x08;
pub const ECHOE: u32 = 0x10;
pub const ECHOK: u32 = 0x20;
pub const ECHONL: u32 = 0x40;

// c_iflag bits — input processing flags
pub const IGNBRK: u32 = 0x001;
pub const BRKINT: u32 = 0x002;
pub const IGNPAR: u32 = 0x004;
pub const PARMRK: u32 = 0x008;
pub const INPCK: u32 = 0x010;
pub const ISTRIP: u32 = 0x020;
pub const INLCR: u32 = 0x040;
pub const IGNCR: u32 = 0x080;
pub const ICRNL: u32 = 0x100;
pub const IXON: u32 = 0x400;
pub const IXOFF: u32 = 0x1000;
pub const IUTF8: u32 = 0x4000;
pub const IUCLC: u32 = 0x200;
pub const IMAXBEL: u32 = 0x2000;

// c_oflag bits — output processing flags
pub const OPOST: u32 = 0x01;
pub const ONLCR: u32 = 0x04;
pub const OCRNL: u32 = 0x08;
pub const ONOCR: u32 = 0x10;
pub const ONLRET: u32 = 0x20;
pub const OLCUC: u32 = 0x02;

// Tab delay output flags
pub const TABDLY: u32 = 0x1800;
pub const TAB0: u32 = 0x0000;
pub const TAB3: u32 = 0x1800;
pub const XTABS: u32 = 0x1800;

// c_lflag bits (additional — see ISIG..ECHONL above)
pub const ECHOCTL: u32 = 0x200;
pub const ECHOPRT: u32 = 0x400;
pub const ECHOKE: u32 = 0x800;
pub const NOFLSH: u32 = 0x80;
pub const TOSTOP: u32 = 0x100;
pub const IEXTEN: u32 = 0x8000;
pub const PENDIN: u32 = 0x4000;
pub const EXTPROC: u32 = 0x10000;

// c_cflag bits — control (hardware) flags
pub const CSIZE: u32 = 0o000060;
pub const CS5: u32 = 0o000000;
pub const CS6: u32 = 0o000020;
pub const CS7: u32 = 0o000040;
pub const CS8: u32 = 0o000060;
pub const CSTOPB: u32 = 0o000100;
pub const CREAD: u32 = 0o000200;
pub const PARENB: u32 = 0o000400;
pub const PARODD: u32 = 0o001000;
pub const HUPCL: u32 = 0o002000;
pub const CLOCAL: u32 = 0o004000;
pub const CBAUD: u32 = 0o010017;
pub const B0: u32 = 0o000000;
pub const B50: u32 = 0o000001;
pub const B75: u32 = 0o000002;
pub const B110: u32 = 0o000003;
pub const B134: u32 = 0o000004;
pub const B150: u32 = 0o000005;
pub const B200: u32 = 0o000006;
pub const B300: u32 = 0o000007;
pub const B600: u32 = 0o000010;
pub const B1200: u32 = 0o000011;
pub const B1800: u32 = 0o000012;
pub const B2400: u32 = 0o000013;
pub const B4800: u32 = 0o000014;
pub const B9600: u32 = 0o000015;
pub const B19200: u32 = 0o000016;
pub const B38400: u32 = 0o000017;
pub const CBAUDEX: u32 = 0o010000;
pub const B57600: u32 = 0o010001;
pub const B115200: u32 = 0o010002;
pub const B230400: u32 = 0o010003;
pub const B460800: u32 = 0o010004;
pub const B500000: u32 = 0o010005;
pub const B576000: u32 = 0o010006;
pub const B921600: u32 = 0o010007;
pub const B1000000: u32 = 0o010010;
pub const B1152000: u32 = 0o010011;
pub const B1500000: u32 = 0o010012;
pub const B2000000: u32 = 0o010013;
pub const B2500000: u32 = 0o010014;
pub const B3000000: u32 = 0o010015;
pub const B3500000: u32 = 0o010016;
pub const B4000000: u32 = 0o010017;
pub const CRTSCTS: u32 = 0o020000000;

pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VEOL: usize = 11;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VREPRINT: usize = 12;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;
pub const VEOL2: usize = 16;

// =============================================================================
// Type-safe termios flag types
// =============================================================================

bitflags::bitflags! {
    /// Type-safe wrapper for `c_iflag` — input processing flags.
    ///
    /// Constructed from the raw `u32` via `InputFlags::from_bits_truncate()`.
    /// Convert back with `.bits()`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct InputFlags: u32 {
        const IGNBRK = 0x001;
        const BRKINT = 0x002;
        const IGNPAR = 0x004;
        const PARMRK = 0x008;
        const INPCK  = 0x010;
        const ISTRIP = 0x020;
        const INLCR  = 0x040;
        const IGNCR  = 0x080;
        const ICRNL  = 0x100;
        const IXON   = 0x400;
        const IXOFF  = 0x1000;
        const IUTF8  = 0x4000;
        const IMAXBEL = 0x2000;
        const IUCLC  = 0x200;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_oflag` — output processing flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OutputFlags: u32 {
        const OPOST  = 0x01;
        const ONLCR  = 0x04;
        const OCRNL  = 0x08;
        const ONOCR  = 0x10;
        const ONLRET = 0x20;
        const OLCUC  = 0x02;

        // Tab delay flags (TABDLY/XTABS).
        // TABDLY is a 2-bit mask; only TAB0 (no expansion) and
        // TAB3/XTABS (expand to spaces) are implemented.
        const TABDLY = 0x1800;
        const TAB0   = 0x0000;
        const TAB3   = 0x1800;
        const XTABS  = 0x1800;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_lflag` — local (line discipline) flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LocalFlags: u32 {
        const ISIG    = 0x01;
        const ICANON  = 0x02;
        const ECHO    = 0x08;
        const ECHOE   = 0x10;
        const ECHOK   = 0x20;
        const ECHONL  = 0x40;
        const NOFLSH  = 0x80;
        const TOSTOP  = 0x100;
        const ECHOCTL = 0x200;
        const ECHOPRT = 0x400;
        const ECHOKE  = 0x800;
        const PENDIN  = 0x4000;
        const IEXTEN  = 0x8000;
        const EXTPROC = 0x10000;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_cflag` — control (hardware) flags.
    ///
    /// Full c_cflag ABI with character size, parity,
    /// stop bits, modem control, baud rates, and hardware flow control.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ControlFlags: u32 {
        const CSIZE   = 0o000060;
        const CS5     = 0o000000;
        const CS6     = 0o000020;
        const CS7     = 0o000040;
        const CS8     = 0o000060;
        const CSTOPB  = 0o000100;
        const CREAD   = 0o000200;
        const PARENB  = 0o000400;
        const PARODD  = 0o001000;
        const HUPCL   = 0o002000;
        const CLOCAL  = 0o004000;
        const CRTSCTS = 0o020000000;
    }
}

// =============================================================================
// Strongly-typed c_cc index enum
// =============================================================================

/// Strongly-typed index into the `c_cc` control character array.
///
/// Replaces raw `usize` constants (`VINTR`, `VQUIT`, …) with a closed enum
/// so that invalid indices are compile-time errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CcIndex {
    Vintr = 0,
    Vquit = 1,
    Verase = 2,
    Vkill = 3,
    Veof = 4,
    Vtime = 5,
    Vmin = 6,
    Vstart = 8,
    Vstop = 9,
    Vsusp = 10,
    Veol = 11,
    Vreprint = 12,
    Vwerase = 14,
    Vlnext = 15,
    Veol2 = 16,
}

impl CcIndex {
    /// Convert to the underlying `usize` for array indexing.
    #[inline]
    pub const fn as_usize(self) -> usize {
        self as usize
    }
}

/// POSIX `_POSIX_VDISABLE` — value indicating a disabled control character.
pub const POSIX_VDISABLE: u8 = 0;

// =============================================================================
// UserTermios typed accessors
// =============================================================================

impl UserTermios {
    /// Get the typed input flags.
    #[inline]
    pub fn input_flags(&self) -> InputFlags {
        InputFlags::from_bits_truncate(self.c_iflag)
    }

    /// Get the typed output flags.
    #[inline]
    pub fn output_flags(&self) -> OutputFlags {
        OutputFlags::from_bits_truncate(self.c_oflag)
    }

    /// Get the typed local flags.
    #[inline]
    pub fn local_flags(&self) -> LocalFlags {
        LocalFlags::from_bits_truncate(self.c_lflag)
    }

    /// Get the typed control flags.
    #[inline]
    pub fn control_flags(&self) -> ControlFlags {
        ControlFlags::from_bits_truncate(self.c_cflag)
    }

    /// Look up a control character by typed index.
    #[inline]
    pub fn cc(&self, idx: CcIndex) -> u8 {
        self.c_cc[idx.as_usize()]
    }

    /// Set a control character by typed index.
    #[inline]
    pub fn set_cc(&mut self, idx: CcIndex, val: u8) {
        self.c_cc[idx.as_usize()] = val;
    }
}

impl Default for UserTermios {
    fn default() -> Self {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03; // Ctrl+C
        cc[VQUIT] = 0x1C; // Ctrl+\
        cc[VERASE] = 0x7F; // DEL
        cc[VKILL] = 0x15; // Ctrl+U
        cc[VEOF] = 0x04; // Ctrl+D
        cc[VMIN] = 1;
        cc[VSTART] = 0x11; // Ctrl+Q
        cc[VSTOP] = 0x13; // Ctrl+S
        cc[VSUSP] = 0x1A; // Ctrl+Z
        cc[VREPRINT] = 0x12; // Ctrl+R
        cc[VWERASE] = 0x17; // Ctrl+W
        cc[VLNEXT] = 0x16; // Ctrl+V
        Self {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: ICANON | ECHO | ISIG | ECHOE,
            c_line: 0,
            c_cc: cc,
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserWinsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

// =============================================================================
// Thread / clone
// =============================================================================

/// Create a new thread or process via clone.
///
/// # Arguments (via registers)
/// * rdi (arg0): clone flags (CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD etc.)
/// * rsi (arg1): child stack pointer (0 = share parent stack, i.e. fork-like)
/// * rdx (arg2): parent_tid pointer (written if CLONE_PARENT_SETTID)
/// * r10 (arg3): child_tid pointer  (written/cleared per CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID)
/// * r8  (arg4): tls value           (new FS_BASE if CLONE_SETTLS)
///
/// # Returns
/// * child task ID to parent on success
/// * 0 to child on success
/// * Negative errno on failure (-EINVAL, -ENOMEM, -EAGAIN)
pub const SYSCALL_CLONE: u64 = 101;

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

// =============================================================================
// Signals
// =============================================================================

/// Install or query a signal handler for a given signal.
///
/// # Arguments (via registers)
/// * rdi (arg0): signal number (1-31)
/// * rsi (arg1): pointer to new `UserSigaction` (or 0 to query only)
/// * rdx (arg2): pointer to old `UserSigaction` output (or 0 to skip)
/// * r10 (arg3): size of signal set (must be 8)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_RT_SIGACTION: u64 = 102;

/// Examine and change blocked signal mask.
///
/// # Arguments (via registers)
/// * rdi (arg0): how (SIG_BLOCK=0, SIG_UNBLOCK=1, SIG_SETMASK=2)
/// * rsi (arg1): pointer to new signal set (or 0 to query only)
/// * rdx (arg2): pointer to old signal set output (or 0 to skip)
/// * r10 (arg3): size of signal set (must be 8)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_RT_SIGPROCMASK: u64 = 103;

/// Send a signal to a process or task.
///
/// # Arguments (via registers)
/// * rdi (arg0): target task ID (or 0 for self)
/// * rsi (arg1): signal number (1-31, or 0 to check task existence)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -ESRCH, -EPERM)
pub const SYSCALL_KILL: u64 = 104;

/// Restore execution state after a signal handler completes.
///
/// # Arguments
/// * The signal frame is on the user stack (set up by the kernel during
///   signal delivery). No explicit register arguments needed.
///
/// # Returns
/// * Does not return to caller -- restores saved execution context.
pub const SYSCALL_RT_SIGRETURN: u64 = 105;

// =============================================================================
// Futex
// =============================================================================

/// Futex system call -- fast userspace locking primitive.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to the futex word (u32, must be 4-byte aligned)
/// * rsi (arg1): futex operation (FUTEX_WAIT, FUTEX_WAKE)
/// * rdx (arg2): value (expected value for WAIT, max waiters for WAKE)
/// * r10 (arg3): timeout in milliseconds (0 = no timeout; only for FUTEX_WAIT)
///
/// # Returns
/// * FUTEX_WAIT: 0 on success, -EAGAIN if value mismatch, -ETIMEDOUT on timeout
/// * FUTEX_WAKE: number of waiters woken
/// * -ENOSYS for unsupported operations
/// * -EINVAL for bad arguments
pub const SYSCALL_FUTEX: u64 = 106;

/// Futex operations
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;

// =============================================================================
// TLS / arch_prctl
// =============================================================================

/// Set or get architecture-specific thread state (TLS base).
///
/// # Arguments (via registers)
/// * rdi (arg0): sub-command (ARCH_SET_FS, ARCH_GET_FS)
/// * rsi (arg1): for SET_FS: new FS_BASE value; for GET_FS: pointer to u64 output
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (-EINVAL, -EFAULT)
pub const SYSCALL_ARCH_PRCTL: u64 = 107;

/// arch_prctl sub-commands (Linux-compatible values)
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;

// =============================================================================
// Errno constants (Linux-compatible negative values)
// =============================================================================

pub const ERRNO_EINVAL: u64 = (-22i64) as u64;
pub const ERRNO_ENOMEM: u64 = (-12i64) as u64;
pub const ERRNO_EAGAIN: u64 = (-11i64) as u64;
pub const ERRNO_ESRCH: u64 = (-3i64) as u64;
pub const ERRNO_EFAULT: u64 = (-14i64) as u64;
pub const ERRNO_ENOENT: u64 = (-2i64) as u64;
pub const ERRNO_ENOTDIR: u64 = (-20i64) as u64;
pub const ERRNO_ERANGE: u64 = (-34i64) as u64;
pub const ERRNO_ETIMEDOUT: u64 = (-110i64) as u64;
pub const ERRNO_EADDRINUSE: u64 = (-98i64) as u64;
pub const ERRNO_ECONNREFUSED: u64 = (-111i64) as u64;
pub const ERRNO_ENOTCONN: u64 = (-107i64) as u64;
pub const ERRNO_EISCONN: u64 = (-106i64) as u64;
pub const ERRNO_ENOTSOCK: u64 = (-88i64) as u64;
pub const ERRNO_EAFNOSUPPORT: u64 = (-97i64) as u64;
pub const ERRNO_EPROTONOSUPPORT: u64 = (-93i64) as u64;
pub const ERRNO_EDESTADDRREQ: u64 = (-89i64) as u64;
pub const ERRNO_ENETUNREACH: u64 = (-101i64) as u64;
pub const ERRNO_EHOSTUNREACH: u64 = (-113i64) as u64;
pub const ERRNO_ECONNRESET: u64 = (-104i64) as u64;
pub const ERRNO_ECONNABORTED: u64 = (-103i64) as u64;
pub const ERRNO_EADDRNOTAVAIL: u64 = (-99i64) as u64;
pub const ERRNO_ENOBUFS: u64 = (-105i64) as u64;
pub const ERRNO_EINPROGRESS: u64 = (-115i64) as u64;
pub const ERRNO_EOPNOTSUPP: u64 = (-95i64) as u64;
pub const ERRNO_EPIPE: u64 = (-32i64) as u64;
pub const ERRNO_EPERM: u64 = (-1i64) as u64;
pub const ERRNO_EINTR: u64 = (-4i64) as u64;
pub const ERRNO_EIO: u64 = (-5i64) as u64;
pub const ERRNO_ENXIO: u64 = (-6i64) as u64;

/// Internal-only error code for restartable syscalls.  MUST NEVER reach
/// userland — the syscall return path converts it to `ERRNO_EINTR` or
/// transparently restarts the syscall based on `SA_RESTART`.
pub const ERRNO_ERESTARTSYS: u64 = (-512i64) as u64;

// =============================================================================
// Syscall ABI stability
// =============================================================================

/// Total size of the dispatch table. All syscall numbers must be below this.
pub const SYSCALL_TABLE_SIZE: usize = 140;

/// Standard return value for unimplemented syscalls: -ENOSYS (negated errno 38).
pub const ENOSYS_RETURN: u64 = (-38i64) as u64;

// =============================================================================
// Syscall data structures
// =============================================================================

/// System information returned by SYSCALL_SYS_INFO
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
    pub wl_balance: i64,
}

/// POSIX-style timespec returned by `SYSCALL_CLOCK_GETTIME`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct Timespec {
    pub tv_sec: u64,
    pub tv_nsec: u64,
}
