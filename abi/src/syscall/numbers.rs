//! Syscall number definitions (kernel-userland ABI).
//!
//! Gaps are retired calls, and a new syscall takes the next highest number so
//! existing userland binaries keep working. Arguments arrive in `rdi`, `rsi`,
//! `rdx`, `r10`, `r8`, `r9` (`arg0`..`arg5`); unless noted, the return is a
//! non-negative result or a negated errno.

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_FB_INFO: u64 = 6;

/// `getrandom(buf: *mut u8, len, flags) -> bytes written`, from the CSPRNG.
/// `len` is capped at 256 bytes per call; [`GRND_NONBLOCK`] is accepted as a
/// no-op.
pub const SYSCALL_GETRANDOM: u64 = 12;

pub const GRND_NONBLOCK: u32 = 0x0001;
pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
pub const SYSCALL_ROULETTE_DRAW: u64 = 24;

pub const SYSCALL_FS_OPEN: u64 = 14;
pub const SYSCALL_FS_CLOSE: u64 = 15;
pub const SYSCALL_FS_READ: u64 = 16;
pub const SYSCALL_FS_WRITE: u64 = 17;
pub const SYSCALL_FS_STAT: u64 = 18;
pub const SYSCALL_FS_MKDIR: u64 = 19;
pub const SYSCALL_FS_UNLINK: u64 = 20;
pub const SYSCALL_FS_LIST: u64 = 21;

pub const SYSCALL_SYS_INFO: u64 = 22;
pub const SYSCALL_HALT: u64 = 23;
// 120, 123 retired: `net_scan` and `net_info`, superseded by `net_query`.
pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
pub const SYSCALL_OPENPTY: u64 = 145;
// 146-148 retired: index-addressed TTY I/O; TTY access is fd-only.
pub const SYSCALL_GET_TIME_MS: u64 = 39;
pub const SYSCALL_REBOOT: u64 = 85;

/// `clock_gettime(clock_id, out: *mut Timespec)`. Only `CLOCK_MONOTONIC` (0).
pub const SYSCALL_CLOCK_GETTIME: u64 = 125;

// Retired — compositor state is userland-only; the number stays reserved.
pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;

pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_CLIPBOARD_COPY: u64 = 116;
pub const SYSCALL_CLIPBOARD_PASTE: u64 = 117;

pub const SYSCALL_FB_FLIP: u64 = 45;

/// `spawn_path(path: *const u8, path_len, argv: *const *const u8, argc,
/// attrs: *const SpawnAttrs) -> task id`, or a negative `ExecError`.
///
/// The child begins with an empty fd table; the `SpawnAttrs` action list
/// installs exactly the descriptors it inherits (`posix_spawn` file-actions).
pub const SYSCALL_SPAWN_PATH: u64 = 64;
pub const SYSCALL_WAITPID: u64 = 68;
pub const SYSCALL_TERMINATE_TASK: u64 = 69;

/// `exec(path: *const u8, argv: *const *const u8, envp: *const *const u8)`,
/// replacing the current process image. `argv`/`envp` are NUL-terminated arrays
/// of NUL-terminated strings, or 0 for none. Does not return on success.
pub const SYSCALL_EXEC: u64 = 70;

pub const SYSCALL_BRK: u64 = 71;

/// `fork() -> child task id in the parent, 0 in the child`. Copy-on-write.
pub const SYSCALL_FORK: u64 = 72;

pub const SYSCALL_GET_CPU_COUNT: u64 = 80;
pub const SYSCALL_GET_CURRENT_CPU: u64 = 81;
pub const SYSCALL_SET_CPU_AFFINITY: u64 = 82;
pub const SYSCALL_GET_CPU_AFFINITY: u64 = 83;

pub const SYSCALL_GETPID: u64 = 86;
pub const SYSCALL_GETPPID: u64 = 87;
pub const SYSCALL_GETUID: u64 = 88;
pub const SYSCALL_GETGID: u64 = 89;
pub const SYSCALL_GETEUID: u64 = 90;
pub const SYSCALL_GETEGID: u64 = 91;

pub const SYSCALL_CHDIR: u64 = 124;

/// `getcwd(buf: *mut u8, size) -> length written, including the NUL`.
pub const SYSCALL_GETCWD: u64 = 121;

/// `rename(old: *const u8, new: *const u8)` — atomic, same-device only
/// (`EXDEV`).
pub const SYSCALL_RENAME: u64 = 122;

/// `socket(domain, type, protocol) -> fd`. `protocol` 0 auto-selects.
pub const SYSCALL_SOCKET: u64 = 126;

/// `bind(fd, addr: *const SockAddrIn, addrlen)`.
pub const SYSCALL_BIND: u64 = 127;

/// `listen(fd, backlog)` — `backlog` is ignored.
pub const SYSCALL_LISTEN: u64 = 128;

/// `accept(fd, peer: *mut SockAddrIn, addrlen: *mut u32) -> fd`. `peer` and
/// `addrlen` may be 0.
pub const SYSCALL_ACCEPT: u64 = 129;

/// `connect(fd, addr: *const SockAddrIn, addrlen)`.
pub const SYSCALL_CONNECT: u64 = 130;

/// `send(fd, buf: *const u8, len, flags) -> bytes sent`. `flags` must be 0.
pub const SYSCALL_SEND: u64 = 131;

/// `recv(fd, buf: *mut u8, len, flags) -> bytes received`; 0 means the peer
/// closed. `flags` must be 0.
pub const SYSCALL_RECV: u64 = 132;
pub const SYSCALL_SENDTO: u64 = 133;
pub const SYSCALL_RECVFROM: u64 = 134;

/// `resolve(host: *const u8, host_len, out: *mut [u8; 4])` via the in-kernel
/// DNS client. `host` is not NUL-terminated and must be at most 253 bytes.
pub const SYSCALL_RESOLVE: u64 = 135;

/// `setsockopt(fd, level, optname, optval: *const u8, optlen)`.
pub const SYSCALL_SETSOCKOPT: u64 = 136;

/// `getsockopt(fd, level, optname, optval: *mut u8, optlen: *mut u32)` —
/// `optlen` is updated on return.
pub const SYSCALL_GETSOCKOPT: u64 = 137;

/// `shutdown(fd, how)` — `SHUT_RD` 0, `SHUT_WR` 1, `SHUT_RDWR` 2.
pub const SYSCALL_SHUTDOWN: u64 = 138;

/// `vhangup()` — revoke the caller's controlling terminal; every other fd
/// referencing that TTY then fails with `EIO`. `-EPERM` without a ctty.
pub const SYSCALL_VHANGUP: u64 = 139;

/// `mmap(addr, len, prot, flags, fd, offset) -> mapping address`. Anonymous
/// mappings only: `fd` must be -1 and `offset` 0.
pub const SYSCALL_MMAP: u64 = 92;

/// `munmap(addr, len)` — `addr` must be page-aligned.
pub const SYSCALL_MUNMAP: u64 = 93;

/// `mprotect(addr, len, prot)` — `addr` must be page-aligned.
pub const SYSCALL_MPROTECT: u64 = 94;

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

/// `clone(flags, child_stack, parent_tid: *mut, child_tid: *mut, tls) -> child
/// task id in the parent, 0 in the child`. A `child_stack` of 0 shares the
/// parent's, i.e. fork-like; `tls` is the new FS_BASE under `CLONE_SETTLS`.
pub const SYSCALL_CLONE: u64 = 101;

/// `rt_sigaction(signum, new: *const UserSigaction, old: *mut UserSigaction,
/// sigsetsize)` — `signum` is `1..=NSIG`, `sigsetsize` must be 8, and either
/// pointer may be 0.
pub const SYSCALL_RT_SIGACTION: u64 = 102;

/// `rt_sigprocmask(how, new: *const SigSet, old: *mut SigSet, sigsetsize)` —
/// `SIG_BLOCK` 0, `SIG_UNBLOCK` 1, `SIG_SETMASK` 2; `sigsetsize` must be 8.
pub const SYSCALL_RT_SIGPROCMASK: u64 = 103;

/// `kill(tid, signum)` — `tid` 0 targets self; `signum` 0 only probes for the
/// task's existence.
pub const SYSCALL_KILL: u64 = 104;

/// `rt_sigreturn()` — no register arguments; the signal frame is on the user
/// stack. Does not return to the caller.
pub const SYSCALL_RT_SIGRETURN: u64 = 105;

/// `sigdefault(mask: SigSet)` — force every signal in the mask to `SIG_DFL`,
/// overriding a caught handler or `SIG_IGN`, in one call.
pub const SYSCALL_SIGDEFAULT: u64 = 118;

/// `futex(uaddr: *mut u32, op, val, timeout_ms)`, returning 0 for `FUTEX_WAIT`
/// and the number of waiters woken for `FUTEX_WAKE`. `uaddr` must be 4-byte
/// aligned; `timeout_ms` 0 means no timeout.
pub const SYSCALL_FUTEX: u64 = 106;

/// `arch_prctl(code, addr)` — `ARCH_SET_FS` takes the new FS_BASE value,
/// `ARCH_GET_FS` a `*mut u64` output.
pub const SYSCALL_ARCH_PRCTL: u64 = 107;

/// `process_list(out: *mut UserTaskEntry, max) -> entries written`.
pub const SYSCALL_PROCESS_LIST: u64 = 141;

/// `cpu_info(out: *mut UserCpuInfo)`.
pub const SYSCALL_CPU_INFO: u64 = 142;

/// `percpu_stats(out: *mut UserPerCpuStats, max) -> entries written`.
pub const SYSCALL_PERCPU_STATS: u64 = 143;

/// `font_set(data: *const u8, width, height, glyph_count, format)`.
///
/// Bitmap format (0): 1bpp MSB-first, one byte per row per glyph,
/// `glyph_count × height` bytes; width must be 8. Coverage format (1): 8-bit
/// alpha for glyphs 0x20–0x7E plus one replacement, `96 × width × height`
/// bytes.
pub const SYSCALL_FONT_SET: u64 = 144;

pub const FONT_FORMAT_BITMAP: u64 = 0;
pub const FONT_FORMAT_COVERAGE: u64 = 1;

/// `memfd_create(flags) -> fd`. `flags` is reserved and must be 0.
pub const SYSCALL_MEMFD_CREATE: u64 = 149;

/// `ftruncate(fd, size)` — memfd only; `size` must be > 0 and is rounded up to
/// a page internally.
pub const SYSCALL_FTRUNCATE: u64 = 150;

/// `sendmsg(fd, msg: *const MsgHdr, flags) -> bytes sent`, optionally carrying
/// `SCM_RIGHTS` ancillary data. `flags` is reserved and must be 0.
pub const SYSCALL_SENDMSG: u64 = 151;

/// `recvmsg(fd, msg: *mut MsgHdr, flags) -> bytes received`, optionally
/// carrying `SCM_RIGHTS` ancillary data. `flags` is reserved and must be 0.
pub const SYSCALL_RECVMSG: u64 = 152;

/// `getpeername(fd, addr: *mut u8, addrlen: *mut u32)`.
pub const SYSCALL_GETPEERNAME: u64 = 153;

/// `getsockname(fd, addr: *mut u8, addrlen: *mut u32)`.
pub const SYSCALL_GETSOCKNAME: u64 = 154;

/// `test_report(status, name: *const u8, name_len, msg: *const u8, msg_len)` —
/// one userland subtest result; `status` is 0 Pass / 1 Fail / 2 Skip. Name and
/// message are UTF-8 without NUL, truncated at `TEST_REPORT_NAME_MAX` /
/// `TEST_REPORT_MSG_MAX`.
pub const SYSCALL_TEST_REPORT: u64 = 155;

/// Drive the kernel-side userland-test phase: spawn each `TestKind::Userland`
/// entry of the `.test_registry`, drain its `SYSCALL_TEST_REPORT` ring, emit
/// KTAP, and merge with the kernel-phase summary.
///
/// Caller must be a real kernel-scheduled task (`/sbin/init` is the canonical
/// one) — `task_wait_for` requires a non-null `current_task`.
pub const SYSCALL_RUN_USERLAND_TESTS: u64 = 156;

/// SlopRing: create a submission/completion ring (SLOPRING § 6.1).
/// `ring_setup(entries: u32, params: *mut RingParams) -> ring fd`. Maps the
/// shared ring region into the caller and writes [`crate::ring::RingParams`].
pub const SYSCALL_RING_SETUP: u64 = 157;

/// SlopRing: submit and/or harvest ring completions (SLOPRING § 6.2).
/// `ring_enter(ring_fd, to_submit, min_complete, flags) -> submissions`.
/// With `min_complete > 0` the calling task blocks on the in-flight resource
/// queues until that many CQEs are ready, a signal arrives, or the deadline
/// elapses.
pub const SYSCALL_RING_ENTER: u64 = 158;

/// `pidfd_open(pid: u32) -> fd` that becomes `POLLIN`-ready once the target
/// task exits. Not readable (`read` → `-EINVAL`); reap the status with
/// `waitpid`. The target must be a child of the caller.
pub const SYSCALL_PIDFD_OPEN: u64 = 159;

/// `signalfd(mask: u64, flags: u32) -> fd` that becomes `POLLIN`-ready while a
/// signal in `mask` is pending for the calling task, and whose `read` drains
/// one `SignalfdSiginfo`. Pair with blocking those signals so they queue as
/// in-band ring/poll events instead of interrupting waits with `EINTR`.
pub const SYSCALL_SIGNALFD: u64 = 160;

/// SlopRing: register provided/fixed buffers with a ring (SLOPRING § 13, ABI
/// v2). `ring_register(ring_fd, op, arg: u64, nr_args)`; the implemented ops
/// sit behind [`super::super::ring::SLOPRING_FEAT_REG_BUFFERS`] and an unknown
/// one is `-ENOSYS`.
pub const SYSCALL_RING_REGISTER: u64 = 161;

pub const RING_REGISTER_PBUF_RING: u32 = 1;
pub const RING_REGISTER_BUFFERS: u32 = 2;
pub const RING_UNREGISTER_PBUF_RING: u32 = 3;
pub const RING_UNREGISTER_BUFFERS: u32 = 4;

/// `cursor_set_image(image: *const u8, len, hotspot)` — a 64×64 BGRA image,
/// `hotspot` packing `(hot_x << 16) | hot_y`. Compositor-only.
pub const SYSCALL_CURSOR_SET_IMAGE: u64 = 162;

/// `cursor_move(pos)` in absolute display coords, `pos` packing
/// `(x << 16) | y`. Compositor-only.
pub const SYSCALL_CURSOR_MOVE: u64 = 163;

/// `set_display_mode(width: u32, height: u32)`. Compositor-only.
pub const SYSCALL_SET_DISPLAY_MODE: u64 = 164;

/// `keymap_load(data: *const u8, len)` — a serialised `LayoutTable` blob (see
/// `slopos_abi::input::layout`), `EINVAL` if malformed. Unprivileged: the
/// kernel-side binary validator is the safety boundary, and the kernel never
/// parses layout text.
pub const SYSCALL_KEYMAP_LOAD: u64 = 165;

/// `keymap_get_name(buf: *mut u8, buf_len) -> bytes written` of the active
/// layout's short name. Unprivileged.
pub const SYSCALL_KEYMAP_GET_NAME: u64 = 166;

/// Deliberately panic in syscall context to exercise the task-scoped
/// panic-recovery boundary. `ENOSYS` unless the `panic.recover_smoke` boot flag
/// is set, so production images expose no panic trigger; when armed the call
/// does not return.
pub const SYSCALL_TEST_PANIC: u64 = 167;

/// `net_query(what, ifindex, buf, len) -> bytes written`, as a
/// [`UserNetQueryHdr`](crate::net::UserNetQueryHdr) followed by `record_count`
/// fixed-stride records. Truncation is read from the header
/// (`total_count > record_count`), not the return value, so a header-sized
/// buffer is the sizing query and anything smaller is `EINVAL`. Unprivileged,
/// but `NET_Q_SOCKETS` names `owner_pid` only for the caller's own sockets
/// unless it holds `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_QUERY: u64 = 168;

/// `net_iface_ctl(ifindex, op, arg)` — admin up/down, MTU, DHCP lifecycle,
/// neighbour and address flushes, plus the global operations addressed to
/// `NET_IFINDEX_GLOBAL`. Multiplexed where the three calls below are not,
/// because every operand fits in a scalar and there is no user memory to
/// reinterpret. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_IFACE_CTL: u64 = 169;

/// `net_addr_ctl(op, ptr, len)`, where `op` is `NET_ADDROP_ADD`/`_DEL` and
/// `ptr` points at exactly one [`UserAddrReq`](crate::net::UserAddrReq) whose
/// size `len` must equal. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_ADDR_CTL: u64 = 170;

/// `net_route_ctl(op, ptr, len)`, where `op` is `NET_ROUTEOP_ADD`/`_DEL` and
/// `ptr` points at exactly one [`UserRouteReq`](crate::net::UserRouteReq) whose
/// size `len` must equal. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_ROUTE_CTL: u64 = 171;

/// `net_resolver_set(ptr, len)` — exactly one
/// [`UserResolverReq`](crate::net::UserResolverReq) whose size `len` must
/// equal. No op code: clearing the static override is a request naming zero
/// servers. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_RESOLVER_SET: u64 = 172;

/// `net_monitor(mask, flags) -> fd` that becomes `POLLIN`-ready whenever the
/// stack's configuration changes and whose `read` drains whole
/// [`NetEvent`](crate::net::NetEvent) records. A dropped record is reported in
/// band, as a `NET_EV_OVERFLOW` ordered before the records that followed the
/// drop, so a reader never loses its position. Unprivileged.
pub const SYSCALL_NET_MONITOR: u64 = 173;

/// `prlimit64(pid, resource, new: *const RLimit64, old: *mut RLimit64)` —
/// `pid` must be 0 or the caller's own id.
///
/// The limits reported are the ones the kernel actually enforces, not
/// `RLIM64_INFINITY` placeholders: a caller that cannot query a real bound
/// cannot back off gracefully.
pub const SYSCALL_PRLIMIT64: u64 = 174;

/// `screen_acquire(seat_id)` — take the framebuffer seat, returning a
/// non-duplicable descriptor naming it.
///
/// `seat_id` is `slopos_ostd::seat::SeatId`: 0 compositor-primary, 1 virtcon.
/// `EBUSY` when a seat of equal or higher rank is held. Ownership is announced
/// here and never conferred by presenting a frame.
pub const SYSCALL_SCREEN_ACQUIRE: u64 = 175;

/// `input_sink_acquire(seat_id)` — as [`SYSCALL_SCREEN_ACQUIRE`], for the raw
/// input event stream that `input_poll_batch` drains.
pub const SYSCALL_INPUT_SINK_ACQUIRE: u64 = 176;

/// `fsync(fd)` — commits the fd's whole filesystem, not the inode; `EINVAL`
/// on a descriptor with no backing store.
pub const SYSCALL_FSYNC: u64 = 177;

/// `fdatasync(fd)` — identical to [`SYSCALL_FSYNC`] until per-inode writeback
/// exists; separate now so the split needs no userland rebuild.
pub const SYSCALL_FDATASYNC: u64 = 178;

pub const SYSCALL_SYNC: u64 = 179;

/// Size of the dispatch table; every syscall number must be below this.
pub const SYSCALL_TABLE_SIZE: usize = 180;

const _: () = assert!((SYSCALL_PIDFD_OPEN as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SIGNALFD as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_RING_REGISTER as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SET_DISPLAY_MODE as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_KEYMAP_LOAD as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_KEYMAP_GET_NAME as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_TEST_PANIC as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_QUERY as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_IFACE_CTL as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_ADDR_CTL as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_ROUTE_CTL as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_RESOLVER_SET as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_NET_MONITOR as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_PRLIMIT64 as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SCREEN_ACQUIRE as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_INPUT_SINK_ACQUIRE as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_FSYNC as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_FDATASYNC as usize) < SYSCALL_TABLE_SIZE);
const _: () = assert!((SYSCALL_SYNC as usize) < SYSCALL_TABLE_SIZE);

/// Standard return value for unimplemented syscalls: -ENOSYS (negated errno 38).
pub const ENOSYS_RETURN: u64 = (-38i64) as u64;
