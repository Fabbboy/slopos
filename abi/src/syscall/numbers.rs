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

/// Fill a buffer with cryptographically secure random bytes.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to output buffer
/// * rsi (arg1): buffer length in bytes (capped at 256 per call)
/// * rdx (arg2): flags (GRND_NONBLOCK = 0x0001, currently no-op)
///
/// # Returns
/// * Number of bytes written on success
/// * -EFAULT: invalid pointer
/// * -EINVAL: invalid flags
pub const SYSCALL_GETRANDOM: u64 = 12;

/// Flags for SYSCALL_GETRANDOM.
pub const GRND_NONBLOCK: u32 = 0x0001;
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
pub const SYSCALL_OPENPTY: u64 = 145;
pub const SYSCALL_TTY_READ: u64 = 146;
pub const SYSCALL_TTY_WRITE: u64 = 147;
/// Open a file descriptor for a TTY by its index.  Returns the new fd number.
pub const SYSCALL_OPEN_TTY_FD: u64 = 148;
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
// Window management
// =============================================================================

// Slot 30 reserved (enumerate_windows removed — compositor state is userland-only)
pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;

// =============================================================================
// Input events
// =============================================================================

pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_CLIPBOARD_COPY: u64 = 116;
pub const SYSCALL_CLIPBOARD_PASTE: u64 = 117;

// =============================================================================
// Compositor framebuffer
// =============================================================================

pub const SYSCALL_FB_FLIP: u64 = 45;

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

// =============================================================================
// System monitoring
// =============================================================================

/// List all active tasks with their status information.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to UserTaskEntry array (output buffer)
/// * rsi (arg1): maximum number of entries the buffer can hold
///
/// # Returns
/// * Number of entries written on success
/// * Negative errno on failure
pub const SYSCALL_PROCESS_LIST: u64 = 141;

/// Get CPU identification and feature information.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to UserCpuInfo struct (output)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_CPU_INFO: u64 = 142;

/// Get per-CPU scheduler statistics.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to UserPerCpuStats array (output buffer)
/// * rsi (arg1): maximum number of entries the buffer can hold
///
/// # Returns
/// * Number of entries written on success
/// * Negative errno on failure
pub const SYSCALL_PERCPU_STATS: u64 = 143;

// =============================================================================
// Syscall ABI stability
// =============================================================================

/// Total size of the dispatch table. All syscall numbers must be below this.
// =============================================================================
// Font management
// =============================================================================

/// Upload a console font to the kernel.
///
/// # Arguments (via registers)
/// * rdi (arg0): pointer to font data (user-space)
/// * rsi (arg1): font width in pixels (must be 8 for bitmap format)
/// * rdx (arg2): font height in pixels (e.g. 16)
/// * r10 (arg3): glyph count (e.g. 256 for bitmap, 95 for coverage)
/// * r8  (arg4): format — [`FONT_FORMAT_BITMAP`] (0) or
///               [`FONT_FORMAT_COVERAGE`] (1)
///
/// **Bitmap format** (`arg4 = 0`): 1-bit-per-pixel, MSB-first, one byte
/// per row per glyph.  Total payload = `glyph_count × height` bytes.
///
/// **Coverage format** (`arg4 = 1`): 8-bit coverage per pixel, 95 ASCII
/// glyphs (0x20–0x7E) followed by one replacement glyph.  Total payload
/// = `(95 + 1) × width × height` bytes.
///
/// # Returns
/// * `0` on success
/// * negative errno on failure
pub const SYSCALL_FONT_SET: u64 = 144;

/// Font bitmap format: 1-bit-per-pixel MSB-first, like VGA ROM fonts.
pub const FONT_FORMAT_BITMAP: u64 = 0;
/// Pre-rasterized coverage format: 8-bit-per-pixel alpha, 95 glyphs + replacement.
pub const FONT_FORMAT_COVERAGE: u64 = 1;

// =============================================================================
// Memfd / ftruncate
// =============================================================================

/// Create an anonymous memory-backed file descriptor.
///
/// # Arguments (via registers)
/// * rdi (arg0): flags (reserved, must be 0)
///
/// # Returns
/// * File descriptor on success
/// * Negative errno on failure
pub const SYSCALL_MEMFD_CREATE: u64 = 149;

/// Set the size of a memfd.
///
/// # Arguments (via registers)
/// * rdi (arg0): file descriptor (must refer to a memfd)
/// * rsi (arg1): new size in bytes (must be > 0, page-aligned internally)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_FTRUNCATE: u64 = 150;

// =============================================================================
// sendmsg / recvmsg (fd passing)
// =============================================================================

/// Send a message on a socket, optionally with ancillary data (SCM_RIGHTS).
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to MsgHdr struct
/// * rdx (arg2): flags (reserved, must be 0)
///
/// # Returns
/// * Bytes sent on success
/// * Negative errno on failure
pub const SYSCALL_SENDMSG: u64 = 151;

/// Receive a message from a socket, optionally with ancillary data (SCM_RIGHTS).
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to MsgHdr struct (output)
/// * rdx (arg2): flags (reserved, must be 0)
///
/// # Returns
/// * Bytes received on success
/// * Negative errno on failure
pub const SYSCALL_RECVMSG: u64 = 152;

// =============================================================================
// Socket address query
// =============================================================================

/// Get the address of the peer connected to a socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to address buffer (output)
/// * rdx (arg2): pointer to address length (in/out)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_GETPEERNAME: u64 = 153;

/// Get the local address bound to a socket.
///
/// # Arguments (via registers)
/// * rdi (arg0): socket file descriptor
/// * rsi (arg1): pointer to address buffer (output)
/// * rdx (arg2): pointer to address length (in/out)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure
pub const SYSCALL_GETSOCKNAME: u64 = 154;

// =============================================================================
// Userland test harness
// =============================================================================

/// Userland test harness: report a single subtest result to the kernel.
///
/// # Arguments (via registers)
/// * rdi (arg0): status (0=Pass, 1=Fail, 2=Skip)
/// * rsi (arg1): pointer to test name (UTF-8, no NUL)
/// * rdx (arg2): test name length in bytes (truncated at TEST_REPORT_NAME_MAX)
/// * r10 (arg3): pointer to message (UTF-8, no NUL; may be NULL if msg_len=0)
/// * r8  (arg4): message length in bytes (truncated at TEST_REPORT_MSG_MAX)
///
/// # Returns
/// * 0 on success
/// * Negative errno on failure (EINVAL bad pointer/status, ENOMEM ring alloc)
pub const SYSCALL_TEST_REPORT: u64 = 155;

/// Userland test harness: drive the kernel-side userland-test phase.
///
/// Walks the `.test_registry` for `TestKind::Userland` entries, spawns each
/// binary, waits for it to exit, drains its `SYSCALL_TEST_REPORT` ring, emits
/// indented KTAP subtest lines, and rolls up to a parent KTAP outcome per
/// utest. Merges the userland summary with the kernel-phase summary stashed
/// at boot and signals shutdown via the `qemu-exit` mechanism when
/// `tests.shutdown=on`.
///
/// Caller MUST be a real kernel-scheduled task (`/sbin/init` is the
/// canonical caller) — `task_wait_for` requires `current_task != null`.
///
/// # Arguments
/// (none — all configuration comes from the boot command line and the
/// kernel-stashed summary state.)
///
/// # Returns
/// * 0 on success (or harness disabled)
/// * Negative errno on failure
pub const SYSCALL_RUN_USERLAND_TESTS: u64 = 156;

/// SlopRing: create a submission/completion ring (SLOPRING § 6.1).
///
/// `ring_setup(entries: u32, params: *mut RingParams) -> i32`.
/// Allocates the shared `Frame<RingMeta>` region, maps it into the
/// caller's address space, opens a `FileKind::Ring` fd, and writes the
/// populated [`crate::ring::RingParams`] to the user out-pointer.
/// Returns the ring fd (`>= 0`) or a negated errno. Synchronous.
pub const SYSCALL_RING_SETUP: u64 = 157;

/// SlopRing: submit and/or harvest ring completions (SLOPRING § 6.2).
///
/// `ring_enter(ring_fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> i32`.
/// The doorbell + harvest call. Processes up to `to_submit` SQEs, then
/// (when `min_complete > 0`) blocks the *calling task* on the in-flight
/// resource queues until `min_complete` CQEs are available, a signal
/// arrives, or a deadline elapses. Returns the submission count (or a
/// negated errno). Synchronous; no `async fn` anywhere on the kernel
/// side.
pub const SYSCALL_RING_ENTER: u64 = 158;

/// Open a process-exit fd (pidfd) for a target task.
///
/// `pidfd_open(pid: u32) -> i32`. Returns a `FileKind::Pidfd` fd that
/// becomes `POLLIN`-ready once the target task exits — pollable via
/// `poll(2)` / SlopRing `OP_POLL_ADD`, so a waiter need not busy-poll
/// `waitpid`. The fd is not readable (`read` → `-EINVAL`); reap the exit
/// status with the existing `waitpid` syscall once it signals. The target
/// must be a child of the caller (`-EPERM` otherwise, `-ESRCH` if absent).
pub const SYSCALL_PIDFD_OPEN: u64 = 159;

/// Create a signal fd watching a mask of signals.
///
/// `signalfd(mask: u64, flags: u32) -> i32`. Returns a `FileKind::Signalfd`
/// fd that becomes `POLLIN`-ready when any signal in `mask` is pending for
/// the calling task, and whose `read` drains one `SignalfdSiginfo`. Paired
/// with blocking those signals (`rt_sigprocmask`) so they queue as in-band
/// ring/poll events rather than interrupting waits with `EINTR`.
pub const SYSCALL_SIGNALFD: u64 = 160;

/// SlopRing: register provided/fixed buffers with a ring (SLOPRING § 13,
/// ABI v2). `ring_register(ring_fd: i32, op: u32, arg: u64, nr_args: u32)`.
/// [`RING_REGISTER_PBUF_RING`] (provided buffer rings) and
/// [`RING_REGISTER_BUFFERS`] (fixed/registered buffers) are implemented behind
/// [`super::super::ring::SLOPRING_FEAT_REG_BUFFERS`]; an unknown `op` returns
/// `-ENOSYS`.
pub const SYSCALL_RING_REGISTER: u64 = 161;

/// `ring_register` op: register a provided-buffer ring.
pub const RING_REGISTER_PBUF_RING: u32 = 1;
/// `ring_register` op: register fixed/registered buffers.
pub const RING_REGISTER_BUFFERS: u32 = 2;
/// `ring_register` op: unregister a provided-buffer ring.
pub const RING_UNREGISTER_PBUF_RING: u32 = 3;
/// `ring_register` op: unregister the fixed/registered buffer set.
pub const RING_UNREGISTER_BUFFERS: u32 = 4;

/// Upload a 64×64 BGRA hardware-cursor image to the display backend.
/// `cursor_set_image(image_ptr: *const u8, len: usize, hotspot: u32)` where
/// `hotspot` packs `(hot_x << 16) | hot_y`. Compositor-only.
pub const SYSCALL_CURSOR_SET_IMAGE: u64 = 162;

/// Move the hardware cursor to absolute display coords.
/// `cursor_move(pos: u32)` where `pos` packs `(x << 16) | y`. Compositor-only.
pub const SYSCALL_CURSOR_MOVE: u64 = 163;

/// Runtime display mode-set. `set_display_mode(width: u32, height: u32)`.
/// Compositor-only; returns 0 on success.
pub const SYSCALL_SET_DISPLAY_MODE: u64 = 164;

/// Install a keyboard layout. `keymap_load(data_ptr: *const u8, len: usize)`,
/// where the buffer is a serialised `LayoutTable` blob (see
/// `slopos_abi::input::layout`). Console-admin only; returns 0 on success,
/// `EINVAL` if the blob is malformed. The kernel never parses layout text —
/// userland parses `*.layout` files and uploads the validated binary.
pub const SYSCALL_KEYMAP_LOAD: u64 = 165;

/// Query the active layout's short name. `keymap_get_name(buf: *mut u8,
/// buf_len: usize)` copies up to `buf_len` bytes of the name and returns the
/// number written. Unprivileged.
pub const SYSCALL_KEYMAP_GET_NAME: u64 = 166;

/// Deliberately panic in syscall context to exercise the task-scoped
/// panic-recovery boundary end-to-end. Returns `ENOSYS` unless the
/// `panic.recover_smoke` boot flag is set, so production images expose no
/// panic trigger; when armed, the call does not return (the task dies in
/// recovery).
pub const SYSCALL_TEST_PANIC: u64 = 167;

pub const SYSCALL_TABLE_SIZE: usize = 168;

const _: () = assert!((SYSCALL_PIDFD_OPEN as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SIGNALFD as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_RING_REGISTER as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SET_DISPLAY_MODE as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_KEYMAP_LOAD as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_KEYMAP_GET_NAME as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_TEST_PANIC as usize) < SYSCALL_TABLE_SIZE);

/// Standard return value for unimplemented syscalls: -ENOSYS (negated errno 38).
pub const ENOSYS_RETURN: u64 = (-38i64) as u64;
