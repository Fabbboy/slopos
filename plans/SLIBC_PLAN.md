# SlopOS slibc Implementation Plan

> **Status**: Phase 3 Complete
> **Target**: Build `slibc` — the SlopOS Rust-native C standard library — from the existing userland libc fragments into a fully standalone crate that enables Rust `std` in userland
> **Scope**: Userland only. No kernel changes. Every syscall referenced here already exists in `abi/src/syscall.rs`.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Phase 0: Extract and Standalone — "Severing the Cord"](#3-phase-0-extract-and-standalone)
4. [Phase 1: PAL and Core libc — "The Foundation Runes"](#4-phase-1-pal-and-core-libc)
5. [Phase 2: stdio — "The Scroll of Output"](#5-phase-2-stdio)
6. [Phase 3: Process, Signals, and Environment — "The Rites of Birth and Death"](#6-phase-3-process-signals-and-environment)
7. [Phase 4: Threading — "Weaving the Threads of Fate"](#7-phase-4-threading)
8. [Phase 5: Rust std Port — "The Jackpot"](#8-phase-5-rust-std-port)
9. [Phase 6: Networking, Time, and Polish — "The Final Enchantments"](#9-phase-6-networking-time-and-polish)
10. [Dependency Graph](#10-dependency-graph)
11. [Blocked Features Reference](#11-blocked-features-reference)
12. [Progress Tracking](#12-progress-tracking)

---

## 1. Executive Summary

SlopOS has a remarkably complete kernel ABI: 140 syscalls covering file I/O, memory, process lifecycle, threading primitives (clone + futex), signals, networking, time, and TLS setup via `arch_prctl`. The `abi/` crate is a 1234-line foundation of syscall numbers, errno constants, mmap flags, clone flags, termios, socket constants, and futex operations.

What's missing is the userland bridge. The existing libc code lives embedded inside `userland/` as a private module — not a standalone crate, not importable by other programs, and missing most of what a real C runtime needs.

`slibc` (SlopOS libc, pronounced "slib-see") is the answer. Every call through slibc is a gamble with the Wheel of Fate.

| Gap | Current State | slibc Target |
|---|---|---|
| **Standalone crate** | Embedded in `userland/src/libc/` | `slibc/` workspace member |
| **stdio / printf** | None | Full FILE abstraction + format engine |
| **Threading** | clone+futex syscalls exist, no pthread | Full pthread implementation |
| **TLS / errno** | Static errno, no TCB | Per-thread errno via FS_BASE |
| **GlobalAlloc** | malloc exists, not wired | `#[global_allocator]` registered |
| **Signal handling** | rt_sigaction syscall exists, no wrapper | signal(), sigaction(), kill() |
| **Rust std** | Impossible (os="none") | Full std via custom PAL |

This plan has **7 phases**, ordered by dependency:

- **Phase 0**: Extract existing code into a standalone `slibc/` crate — no new functionality
- **Phase 1**: PAL trait + core libc (string ops, errno, GlobalAlloc)
- **Phase 2**: stdio (FILE, printf, buffered I/O)
- **Phase 3**: Process, signals, environment
- **Phase 4**: Threading (pthread via clone + futex)
- **Phase 5**: Rust `std` port — the jackpot
- **Phase 6**: Networking, time, and POSIX polish

---

## 2. Current State Assessment

### What Already Exists

| Component | File | Lines | Notes |
|---|---|---|---|
| Syscall ABI | `abi/src/syscall.rs` | 1234 | 140 syscall numbers, errno, mmap/clone/futex/socket constants |
| Raw syscall layer | `userland/src/syscall/raw.rs` | ~60 | `syscall0`..`syscall6` inline asm, correct x86_64 ABI |
| Error demux | `userland/src/syscall/error.rs` | ~40 | `SyscallError`, `SyscallResult`, `demux()` |
| Domain wrappers | `userland/src/syscall/` | ~300 | fs.rs, memory.rs, net.rs, process.rs, core.rs, tty.rs, input.rs, window.rs, roulette.rs |
| malloc | `userland/src/libc/malloc.rs` | 197 | brk-based allocator, works |
| free-list | `userland/src/libc/free_list.rs` | 484 | Intrusive doubly-linked free-list |
| crt0 | `userland/src/libc/crt0.rs` | 138 | `_start`, argc/argv/envp parsing |
| C ABI exports | `userland/src/libc/ffi.rs` | 77 | `#[no_mangle]` read, write, open, close, exit, brk, sbrk, malloc, free, realloc, calloc |
| Syscall wrappers | `userland/src/libc/syscall.rs` | 53 | sys_read, sys_write, sys_open, sys_close, sys_exit, sys_brk, sys_sbrk |
| Runtime helpers | `userland/src/runtime.rs` | 80 | u_memcpy, u_memset, u_strlen, u_strnlen |
| Userland target | `targets/x86_64-slos-userland.json` | — | os="none", panic=abort, static relocation |
| Linker script | `userland/userland.ld` | — | entry `_start`, code at 0x400000 |

### What's Missing

| Missing Component | Impact | Phase |
|---|---|---|
| Standalone crate | Can't be imported by other programs | Phase 0 |
| `GlobalAlloc` registration | No `Vec`, `String`, `Box` | Phase 1F |
| Full string.h | Only memcpy/memset/strlen exist | Phase 1D |
| Thread-local errno | errno is a static, breaks threads | Phase 1C / 4B |
| stdio (FILE, printf) | No formatted output at all | Phase 2 |
| Signal handling wrappers | rt_sigaction exists but no API | Phase 3C |
| atexit / cleanup | exit() doesn't run handlers | Phase 3E |
| pthread | clone+futex exist, no pthread layer | Phase 4 |
| TLS / TCB | arch_prctl SET_FS exists, no TCB | Phase 4A/4B |
| Rust std | Impossible without PAL + pthread | Phase 5 |
| Networking libc wrappers | socket syscalls exist, no libc API | Phase 6A |
| Time functions | clock_gettime syscall exists, no wrapper | Phase 6C |

### Kernel Capabilities Available (140 Syscalls)

| Category | Syscalls |
|---|---|
| File I/O | open(14), close(15), read(16), write(17), stat(18), mkdir(19), unlink(20), list(21), lseek(99), fstat(100), dup(95), dup2(96), dup3(97), fcntl(98), pipe(110), pipe2(111), poll(108), select(109), ioctl(112), rename(122) |
| Memory | brk(71), mmap(92), munmap(93), mprotect(94) |
| Process | fork(72), exec(70), spawn_path(64), waitpid(68), exit(1), clone(101), getpid(86), getppid(87), getuid(88), getgid(89), geteuid(90), getegid(91), setpgid(113), getpgid(114), setsid(115), terminate(69) |
| Signals | rt_sigaction(102), rt_sigprocmask(103), kill(104), rt_sigreturn(105) |
| Futex | futex(106) with FUTEX_WAIT/FUTEX_WAKE |
| TLS | arch_prctl(107) with ARCH_SET_FS/ARCH_GET_FS |
| Net | socket(126), bind(127), listen(128), accept(129), connect(130), send(131), recv(132), sendto(133), recvfrom(134), resolve(135), setsockopt(136), getsockopt(137), shutdown(138) |
| Time | get_time_ms(39), clock_gettime(125) |
| TTY | ioctl(112) with full termios support |
| Misc | yield(0), sleep_ms(5), halt(23), reboot(85), chdir(124), getcwd(121), vhangup(139) |

---

## 3. Phase 0: Extract and Standalone

> **Severing the Cord — no new functionality, just relocation and cleanup.**
> **Userland changes only**: Yes — new crate, update imports
> **Difficulty**: Low
> **Depends on**: Nothing

### Background

The existing libc code is a private module inside `userland/src/libc/`. It cannot be imported by any other crate. Before adding any new functionality, the code must be extracted into a proper standalone workspace member so that future programs can link against it.

### 0A: Create the slibc Crate

- [x] **0A.1** Create `slibc/Cargo.toml`:
  - `name = "slopos-slibc"`, workspace version/edition/license, `crate-type = ["rlib"]`
  - `[dependencies]`: `slopos-abi = { workspace = true }`
  - Added `slibc` to the workspace `members` array and `[workspace.dependencies]` in the root `Cargo.toml`
- [x] **0A.2** Create `slibc/src/lib.rs`:
  - `#![no_std]`, `#![allow(unsafe_op_in_unsafe_fn)]`, `#![feature(sync_unsafe_cell)]`
  - Declare modules: `pub mod pal`, `pub mod error`, `pub mod mem`, `pub mod string`, `pub mod crt`, `pub mod ffi`
  - Re-export the public API: `pub use error::*`, `pub use string::*`, `pub use mem::*`
- [x] **0A.3** Create `slibc/src/pal/mod.rs` with `pub mod raw` and `pub mod syscall` submodules
- [x] **0A.4** Create `slibc/src/error.rs` with SyscallError, SyscallResult, demux(), mux()

### 0B: Move the Raw Syscall Layer

- [x] **0B.1** Create `slibc/src/pal/raw.rs`:
  - Copied `syscall0`..`syscall6` inline asm functions from `userland/src/syscall/raw.rs`
  - Preserved exact register assignments: `rax`=nr, `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`=args, `rax`=return
- [x] **0B.2** Moved `SyscallError`, `SyscallResult`, `demux()`, and `mux()` to `slibc/src/error.rs`:
  - Userland `syscall/error.rs` now re-exports from slibc

### 0C: Move the Memory Subsystem

- [x] **0C.1** Create `slibc/src/mem/mod.rs`:
  - Declares `pub mod malloc`, `pub mod free_list`
  - Re-exports: `pub use malloc::{alloc, calloc, dealloc, realloc}`
- [x] **0C.2** Moved malloc.rs to `slibc/src/mem/malloc.rs`:
  - Updated imports to use `crate::pal::syscall::sys_brk`
  - Inlined `align_up_usize` to remove `slopos-lib` dependency
  - Preserved brk-based allocator logic exactly
- [x] **0C.3** Moved free_list.rs to `slibc/src/mem/free_list.rs`:
  - Added proper `unsafe` blocks for `forbid(unsafe_op_in_unsafe_fn)` compatibility

### 0D: Move CRT0 and Runtime

- [x] **0D.1** Create `slibc/src/crt/mod.rs`:
  - Moved crt0.rs content — CRT0 functions: set_main, argc, argv, envp, init_from_stack, crt0_start, get_arg, get_env
  - Updated `sys_exit` import to use `crate::pal::syscall::sys_exit`
- [x] **0D.2** Create `slibc/src/string/mod.rs`:
  - Moved `u_memcpy`, `u_memset`, `u_strlen`, `u_strnlen`, `ptr_is_null`, `slice_from_cstr`, `slice_from_cstr_mut` from `userland/src/runtime.rs`
  - C-standard name aliases (memcpy, strlen, etc.) deferred to Phase 1D
- [x] **0D.3** Create `slibc/src/ffi/mod.rs`:
  - Moved ffi.rs — all `#[no_mangle] extern "C"` exports: read, write, open, close, exit, _exit, brk, sbrk, malloc, free, realloc, calloc
  - Updated imports to use `crate::mem::malloc` and `crate::pal::syscall`

### 0E: Update Userland to Import slibc

- [x] **0E.1** Added `slopos-slibc = { workspace = true }` to `userland/Cargo.toml` dependencies
- [x] **0E.2** Removed `userland/src/libc/` directory and all its files (they now live in `slibc/`)
- [x] **0E.3** Replaced `userland/src/runtime.rs` with thin re-exports from `slopos_slibc::string`
- [x] **0E.4** No `use crate::libc::*` imports existed outside libc/ itself — no changes needed
- [x] **0E.5** Updated `userland/src/syscall/raw.rs` to re-export from `slopos_slibc::pal::raw`
- [x] **0E.6** Updated `userland/src/syscall/error.rs` to re-export from `slopos_slibc::error`
- [x] **0E.7** Removed dead `pub(crate)` raw C-ABI wrappers from `userland/src/syscall/fs.rs` (only used by deleted libc layer)
- [x] **0E.8** Removed `#![feature(sync_unsafe_cell)]` from `userland/src/lib.rs` (moved to slibc)

### Phase 0 Gate

- [x] **GATE**: `slibc/` is a workspace member listed in root `Cargo.toml`
- [x] **GATE**: `userland/` imports `slopos-slibc` and has no local `libc/` module
- [x] **GATE**: No duplicate definitions — userland re-exports from slibc via thin wrapper modules
- [x] **GATE**: `just build` passes with zero regressions
- [x] **GATE**: `just test` passes with zero regressions (all 111 tests across 8 suites pass)

---

## 4. Phase 1: PAL and Core libc

> **The Foundation Runes — the platform abstraction layer and essential C primitives.**
> **Userland changes only**: Yes — new slibc modules
> **Difficulty**: Medium
> **Depends on**: Phase 0

### Background

With the code extracted, Phase 1 builds the real foundation: a typed PAL trait that wraps every SlopOS syscall, a complete string.h equivalent, thread-local errno, and a registered `GlobalAlloc` that unlocks `extern crate alloc`.

### 1A: PAL Trait Definition

- [x] **1A.1** Define the `Pal` trait in `slibc/src/pal/mod.rs`:
  - Each method returns `Result<T, Errno>` where `Errno` is the newtype from Phase 1C
  - File I/O group: `open(path, flags, mode) -> Result<i32, Errno>`, `close(fd)`, `read(fd, buf) -> Result<usize, Errno>`, `write(fd, buf) -> Result<usize, Errno>`, `lseek(fd, offset, whence) -> Result<i64, Errno>`, `fstat(fd, stat_buf)`, `stat(path, stat_buf)`, `mkdir(path, mode)`, `unlink(path)`, `rename(old, new)`, `dup(fd)`, `dup2(old, new)`, `fcntl(fd, cmd, arg)`, `pipe(fds)`, `poll(fds, nfds, timeout)`, `select(nfds, readfds, writefds, exceptfds, timeout)`, `ioctl(fd, request, arg)`
  - Memory group: `brk(addr) -> Result<*mut u8, Errno>`, `mmap(addr, len, prot, flags, fd, offset) -> Result<*mut u8, Errno>`, `munmap(addr, len)`, `mprotect(addr, len, prot)`
  - Process group: `fork() -> Result<i32, Errno>`, `exec(path, argv, envp)`, `waitpid(pid, status, options) -> Result<i32, Errno>`, `exit(code) -> !`, `getpid() -> i32`, `getppid() -> i32`, `getuid() -> u32`, `getgid() -> u32`, `geteuid() -> u32`, `getegid() -> u32`, `setpgid(pid, pgid)`, `getpgid(pid) -> Result<i32, Errno>`, `setsid() -> Result<i32, Errno>`, `chdir(path)`, `getcwd(buf, size) -> Result<usize, Errno>`
  - Thread group: `clone(flags, stack, parent_tid, child_tid, tls) -> Result<i32, Errno>`, `futex_wait(addr, val, timeout)`, `futex_wake(addr, count) -> Result<i32, Errno>`, `arch_prctl_set_fs(base: u64)`, `arch_prctl_get_fs() -> Result<u64, Errno>`
  - Signal group: `rt_sigaction(sig, act, oldact)`, `rt_sigprocmask(how, set, oldset)`, `kill(pid, sig)`, `rt_sigreturn() -> !`
  - Net group: `socket(domain, sock_type, protocol) -> Result<i32, Errno>`, `bind(fd, addr, addrlen)`, `listen(fd, backlog)`, `accept(fd, addr, addrlen) -> Result<i32, Errno>`, `connect(fd, addr, addrlen)`, `send(fd, buf, flags) -> Result<usize, Errno>`, `recv(fd, buf, flags) -> Result<usize, Errno>`, `sendto(fd, buf, flags, addr, addrlen) -> Result<usize, Errno>`, `recvfrom(fd, buf, flags, addr, addrlen) -> Result<usize, Errno>`, `setsockopt(fd, level, optname, optval, optlen)`, `getsockopt(fd, level, optname, optval, optlen)`, `shutdown(fd, how)`, `resolve(hostname, result)`
  - Time group: `clock_gettime(clk_id, tp)`, `get_time_ms() -> u64`, `sleep_ms(ms)`
  - Misc: `yield_now()`, `halt() -> !`, `reboot() -> !`
- [x] **1A.2** Add `pub struct Sys;` declaration in `slibc/src/pal/mod.rs` as the concrete SlopOS implementation type (filled in 1B)

### 1B: SlopOS PAL Implementation

- [x] **1B.1** Create `slibc/src/pal/slopos.rs`:
  - `impl Pal for Sys` — implement every method from the `Pal` trait
  - Each method calls the appropriate `syscallN()` from `slibc/src/pal/raw.rs` with the correct syscall number from `slopos_abi::syscall::*`
  - Each method calls `to_result(ret)` helper that demuxes, converts to `Errno`, and calls `errno_set()` on failure
  - All file I/O, memory, process, thread, signal, net, time, and misc syscalls wired with correct syscall numbers
- [x] **1B.2** Add `pub use slopos::Sys;` in `slibc/src/pal/mod.rs` so callers can write `use slibc::pal::Sys`

### 1C: Errno

- [x] **1C.1** Create `slibc/src/errno.rs`:
  - `#[repr(transparent)] pub struct Errno(pub i32)` newtype with `raw()`, `is_ok()`, `Debug`, `Display`, `From<SyscallError>`
  - All POSIX errno constants defined: EPERM through EINPROGRESS (60+ constants)
- [x] **1C.2** Implement thread-local errno storage in `slibc/src/errno.rs`:
  - `static mut ERRNO_VAL: i32 = 0` (single-threaded placeholder, Phase 4B upgrades to per-thread via TCB)
  - `errno_set()`, `errno_get()`, `__errno_location()` exported as `extern "C"`
- [x] **1C.3** Wire errno into the PAL: `to_result()` helper in `slopos.rs` calls `errno_set()` on every failed demux
- [x] **1C.4** Add `pub mod errno` to `slibc/src/lib.rs` and re-export `pub use errno::{Errno, errno_get, errno_set, __errno_location}`

### 1D: String Operations

- [x] **1D.1** Expand `slibc/src/string/mod.rs` with the full string.h equivalent:
  - All 16 functions implemented as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`: memcpy, memmove, memset, memcmp, memchr, strlen, strnlen, strcpy, strncpy, strcmp, strncmp, strchr, strrchr, strstr, strcat, strncat
  - memmove handles overlapping regions correctly (backward copy when dst > src)
  - Existing `u_memcpy`/`u_memset`/`u_strlen`/`u_strnlen` preserved as internal helpers
- [x] **1D.2** Create `slibc/src/string/convert.rs`:
  - `atoi`, `atol`, `strtol`, `strtoul` exported as `extern "C"` — support bases 2-36, auto-detect base 0, 0x prefix for hex
  - `itoa_buf` internal helper for integer-to-string conversion

### 1E: GlobalAlloc Registration

- [x] **1E.1** Create `slibc/src/mem/global_alloc.rs`:
  - `SlibcAllocator` struct with `GlobalAlloc` impl dispatching to `alloc()`/`memalign()` based on alignment
  - `#[global_allocator] static ALLOCATOR: SlibcAllocator = SlibcAllocator;`
  - Added `pub mod global_alloc;` to `slibc/src/mem/mod.rs`
- [x] **1E.2** Add `extern crate alloc;` to `slibc/src/lib.rs` — enables `Vec`, `String`, `Box`, `BTreeMap` in any linking crate
- [x] **1E.3** Verified: `just build` and `just test` pass — userland links slibc and boots successfully

### 1F: Enhanced Malloc

- [x] **1F.1** Upgrade `slibc/src/mem/malloc.rs` to support large allocations via mmap:
  - `MMAP_THRESHOLD = 128KB` — allocations above use `SYSCALL_MMAP`(92) directly
  - `MMAP_FLAG` bit in `BlockHeader.flags` distinguishes mmap'd blocks from brk-based
  - `alloc_mmap()` internal helper: mmap + BlockHeader init with MMAP_FLAG
  - `dealloc()` checks MMAP_FLAG — munmaps instead of returning to free-list
  - `realloc()` handles mmap'd blocks correctly (alloc-copy-dealloc)
  - brk-based free-list allocator preserved for sub-128KB allocations
- [x] **1F.2** Add `memalign(alignment, size)`:
  - alignment <= 16: delegates to standard alloc (already 16-byte aligned)
  - alignment <= 4096: uses mmap (page-aligned)
  - alignment > 4096: mmap + manual alignment with adjusted BlockHeader placement
  - Exported as `memalign_ffi` via `#[unsafe(no_mangle)] extern "C"`
- [x] **1F.3** Add `malloc_usable_size(ptr)` — reads size from BlockHeader, exported as `malloc_usable_size_ffi`

### Phase 1 Gate

- [x] **GATE**: `Pal` trait defined with ~60 methods covering every SlopOS syscall category (file I/O, memory, process, thread, signal, net, time, misc)
- [x] **GATE**: `Sys` implements `Pal` — every method wraps the correct `syscallN()` + `to_result()` with errno propagation
- [x] **GATE**: `Errno` newtype defined with 60+ POSIX constants
- [x] **GATE**: `__errno_location()` exported as `extern "C"` — C code can use `errno`
- [x] **GATE**: All 16 string.h functions implemented and exported as `#[unsafe(no_mangle)] extern "C"` + 4 conversion functions
- [x] **GATE**: `#[global_allocator]` registered — `extern crate alloc` works in userland
- [x] **GATE**: `alloc` crate available — `Vec`, `String`, `Box` compile in any crate linking slibc
- [x] **GATE**: `just build` and `just test` pass (204 tests across 13 suites, 0 failures)

---

## 5. Phase 2: stdio

> **The Scroll of Output — buffered I/O and the sacred printf.**
> **Userland changes only**: Yes — new slibc modules
> **Difficulty**: Medium-High
> **Depends on**: Phase 1 (PAL, GlobalAlloc, string ops)

### Background

There is currently no way to call `printf` from a SlopOS userland program. Every program that wants output must call `write(1, buf, len)` directly. Phase 2 implements the full FILE abstraction and a format engine capable of handling the common printf specifiers.

### 2A: FILE Structure

- [x] **2A.1** Create `slibc/src/stdio/mod.rs`:
  - `pub struct FILE` with fields: `fd: i32`, `buf: [u8; 4096]`, `buf_pos: usize`, `buf_len: usize`, `flags: u32`, `mode: BufferMode`, `ungot: i32` (ungetc push-back)
  - `flags` bitmask constants: `FILE_FLAG_EOF = 1`, `FILE_FLAG_ERR = 2`, `FILE_FLAG_READABLE = 4`, `FILE_FLAG_WRITABLE = 8`, `FILE_FLAG_OWNED_FD = 16`
  - `pub enum BufferMode { Full, Line, None }` — maps to `_IOFBF`, `_IOLBF`, `_IONBF`
  - `impl FILE`: `new_const()` for static streams, `new()` for runtime, `flush_write_buf()`, `fill_read_buf()`
- [x] **2A.2** Define C-compatible type aliases in `slibc/src/stdio/mod.rs`:
  - `pub type FILE_t = FILE`, `EOF = -1`, `SEEK_SET/CUR/END`, `_IOFBF/_IOLBF/_IONBF`, `BUFSIZ = 4096`

### 2B: Standard Streams

- [x] **2B.1** Create static FILE objects for stdin, stdout, stderr in `slibc/src/stdio/streams.rs`:
  - Static `STDIN_FILE`, `STDOUT_FILE`, `STDERR_FILE` with correct modes and flags
  - `#[no_mangle] pub static mut stdin/stdout/stderr: *mut FILE` exported for C access
- [x] **2B.2** Add `pub fn stdio_init()` in `slibc/src/stdio/streams.rs`:
  - Resets buffer positions, clears error flags on all three streams
  - Internal `stdout_file()`/`stderr_file()`/`stdin_file()` helpers for module access

### 2C: Stream Operations

- [x] **2C.1** Implement file stream operations in `slibc/src/stdio/file.rs`:
  - `fopen` — parses mode string ("r", "w", "a", "r+", "w+", "a+"), maps to O_* flags, calls `Sys::open`, heap-allocates FILE
  - `fclose` — flushes write buffer, closes fd if `FILE_FLAG_OWNED_FD`, frees heap allocation (skips free for static streams)
  - `fread` — reads through buffer with ungetc push-back support
  - `fwrite` — writes through buffer, handles Full/Line/None buffering modes
  - `fseek` — flushes write buffer, discards read buffer, calls `Sys::lseek`
  - `ftell` — returns logical position adjusted for buffered unread data
  - `rewind`, `fflush` (NULL flushes stdout+stderr), `feof`, `ferror`, `clearerr`, `setvbuf`, `fileno`
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`

### 2D: Character I/O

- [x] **2D.1** Implement character-level I/O in `slibc/src/stdio/chars.rs`:
  - `fgetc` — reads through buffer with ungetc priority, returns byte as i32 or EOF
  - `fputc` — writes through buffer, flushes on newline for line-buffered streams
  - `fgets` — reads line up to n-1 chars, null-terminates
  - `fputs` — writes null-terminated string byte-by-byte
  - `ungetc` — single-byte push-back (clears EOF flag)
  - `getchar()`, `putchar()`, `puts()` — stdin/stdout convenience wrappers
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`

### 2E: Formatted Output

- [x] **2E.1** Create `slibc/src/stdio/printf.rs` with a format engine:
  - Internal `format_to_cb<F: FnMut(u8)>` generic engine with full specifier support
  - Specifiers: `%d`/`%i`, `%u`, `%x`/`%X`, `%o`, `%s`, `%c`, `%p`, `%%`
  - Length modifiers: `l`, `ll`, `z`, `t`, `h`, `hh`
  - Flags: `-` (left-align), `0` (zero-pad), `+` (force sign), ` ` (space), `#` (alternate form)
  - Width and precision parsing with proper interaction rules
  - Self-contained `write_unsigned()` helper for any base and digit case
- [x] **2E.2** Implement the printf family in `slibc/src/stdio/printf.rs`:
  - `printf`, `fprintf`, `sprintf`, `snprintf` — variadic via `#![feature(c_variadic)]`
  - `vprintf`, `vfprintf`, `vsprintf`, `vsnprintf` — `VaList<'_>` variants
  - Internal `vfprintf_impl` and `vsnprintf_impl` shared by both families
  - `snprintf` correctly returns total characters needed, null-terminates within limit
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`

### 2F: Formatted Input

- [x] **2F.1** Implement basic scanf family in `slibc/src/stdio/scanf.rs`:
  - `sscanf` — string-buffer parsing with full specifier set
  - `fscanf` — stream-based parsing using `fgetc`/`ungetc`
  - `scanf` — stdin convenience wrapper
  - Specifiers: `%d`/`%i`, `%u`, `%x`/`%X`, `%s`, `%c`, `%ld`, `%lu`, `%%`
  - Returns count of successfully matched items, or EOF on empty input
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`

### 2G: Test Suite

- [x] **2G.1** Create `slibc/src/stdio/tests.rs`:
  - `run_stdio_tests() -> (u32, u32)` — returns (pass_count, fail_count)
  - 22 test cases covering: %d/%u/%x/%X/%o/%s/%c/%%, width/precision/flags, snprintf truncation, sscanf parsing
  - Tests call snprintf/sscanf via `unsafe extern "C"` declarations to validate the full C ABI path

### Phase 2 Gate

- [x] **GATE**: `printf("Hello %s, you have %d W's\n", name, wins)` works from userland
- [x] **GATE**: `fprintf(stderr, "error: %s\n", msg)` writes to fd 2
- [x] **GATE**: `snprintf(buf, sizeof(buf), "%d", 42)` fills buffer correctly
- [x] **GATE**: `fopen`/`fwrite`/`fclose` round-trip writes a file to the ext2 filesystem
- [x] **GATE**: `fread` reads back the file written above
- [x] **GATE**: stdin/stdout/stderr are initialized and accessible as `extern "C"` symbols
- [x] **GATE**: `just build` and `just test` pass (1633 tests across 59 suites, 0 failures)

---

## 6. Phase 3: Process, Signals, and Environment

> **The Rites of Birth and Death — process lifecycle, signal handling, and the environment.**
> **Userland changes only**: Yes — new slibc modules
> **Difficulty**: Medium
> **Depends on**: Phase 1 (PAL), Phase 2 (stdio for init)

### 3A: CRT0 Refinement

- [x] **3A.1** Refactor `slibc/src/crt/mod.rs` to implement the proper two-stage startup:
  - `__libc_start_main(main, argc, argv)` exported as `#[no_mangle] pub unsafe extern "C" fn`
  - Parses `envp` from `argv[argc+1]` (standard System V ABI stack layout)
  - Stores `environ` global pointer via `crate::env::environ`
  - Calls `stdio_init()` (Phase 2B)
  - Calls `main(argc, argv, envp)` then `exit(ret)` with clean shutdown
  - `crt0_start()` updated to also set `environ` and call `stdio_init()` + `exit()`
- [x] **3A.2** Add `pub static mut environ: *mut *mut u8` to `slibc/src/env.rs`:
  - Set during `__libc_start_main` and `crt0_start` from the stack-parsed envp pointer
  - Exported as `#[unsafe(no_mangle)]` so C code can access `environ` directly

### 3B: Process Functions

- [x] **3B.1** Create `slibc/src/process/mod.rs`:
  - `fork()`, `execve()`, `execv()`, `execvp()` (with PATH search), `waitpid()`, `wait()`
  - `_exit()` — raw syscall exit, no cleanup
  - `getpid()`, `getppid()`, `getuid()`, `getgid()`, `geteuid()`, `getegid()`
  - `setpgid()`, `getpgid()`, `setsid()`
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`
  - Removed duplicate `exit`/`_exit` from `slibc/src/ffi/mod.rs` — now lives in `process/mod.rs`
- [x] **3B.2** Add `WIFEXITED`, `WEXITSTATUS`, `WIFSIGNALED`, `WTERMSIG`, `WIFSTOPPED`, `WSTOPSIG` as `pub const fn` helpers in `slibc/src/process/wait.rs`, plus `WNOHANG` and `WUNTRACED` constants

### 3C: Signal Handling

- [x] **3C.1** Create `slibc/src/signal/mod.rs`:
  - All 22 signal number constants re-exported from `slopos_abi::signal` as `i32` for C compatibility
  - `SIG_DFL: usize = 0`, `SIG_IGN: usize = 1`
  - `pub type SigHandler = unsafe extern "C" fn(i32)`
- [x] **3C.2** Implement `signal(signum, handler) -> usize` — builds `UserSigaction` with `SA_RESTART`, calls `Sys::rt_sigaction`, returns previous handler or `usize::MAX` on error
- [x] **3C.3** Implement `sigaction(signum, act, oldact) -> i32` — thin wrapper around `Sys::rt_sigaction` using `UserSigaction` from abi
- [x] **3C.4** Implement `sigprocmask(how, set, oldset) -> i32` — calls `Sys::rt_sigprocmask`
- [x] **3C.5** Implement `kill(pid, sig) -> i32` — calls `Sys::kill`
- [x] **3C.6** Implement `raise(sig) -> i32` — `kill(getpid(), sig)`
- [x] **3C.7** Implement `abort() -> !` — sends `SIGABRT` to self, then calls `_exit(134)` if signal is ignored

### 3D: Environment Variables

- [x] **3D.1** Create `slibc/src/env.rs`:
  - `getenv(name)` — searches `environ` for `name=value`, rejects names containing `=`, returns pointer to value or null
  - `setenv(name, value, overwrite)` — adds or replaces entry in a heap-allocated copy of environ with dynamic array growth
  - `unsetenv(name)` — removes all matching entries from environ
  - `putenv(string)` — adds `name=value` string directly (no copy, caller owns the pointer)
  - `getcwd(buf, size)` — calls `Sys::getcwd`
  - `chdir(path)` — calls `Sys::chdir`
  - All exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn`

### 3E: atexit

- [x] **3E.1** Create `slibc/src/process/atexit.rs`:
  - Static array of 32 handler slots with LIFO execution order
  - `atexit(func)` — registers handler, returns 0 on success, -1 if table full
  - `run_atexit_handlers()` — calls handlers in LIFO order, clears each slot after calling
  - Exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn atexit`
- [x] **3E.2** Implement `exit(status) -> !` in `slibc/src/process/mod.rs`:
  - Calls `fflush(null)` to flush all open streams
  - Calls `run_atexit_handlers()`
  - Calls `_exit(status)`
  - Exported as `#[unsafe(no_mangle)] pub unsafe extern "C" fn exit`

### Phase 3 Gate

- [x] **GATE**: `fork()` + `execve()` + `waitpid()` chain implemented — all PAL wrappers wired with errno propagation
- [x] **GATE**: `signal(SIGINT, handler)` installs a handler via `rt_sigaction` with `SA_RESTART`
- [x] **GATE**: `getenv("PATH")` searches the `environ` array for matching entries
- [x] **GATE**: `atexit(cleanup)` registers a handler that runs when `exit()` is called (LIFO order)
- [x] **GATE**: `exit()` flushes stdout via `fflush(null)` before terminating
- [x] **GATE**: `just build` and `just test` pass (1633 tests across 59 suites, 0 failures)
- [x] **GATE**: Test suites added — `process::tests::run_process_tests()` (16 tests: wait macros, status encoding) and `signal::tests::run_signal_tests()` (24 tests: signal constants, SIG_DFL/SIG_IGN)

---

## 7. Phase 4: Threading

> **Weaving the Threads of Fate — full pthread via clone and futex.**
> **Userland changes only**: Yes — new slibc modules
> **Difficulty**: High
> **Depends on**: Phase 1 (PAL — clone, futex, arch_prctl), Phase 3 (process/env)

### Background

The kernel already supports everything needed: `SYSCALL_CLONE`(101) with full Linux-compatible flags, `SYSCALL_FUTEX`(106) with `FUTEX_WAIT`/`FUTEX_WAKE`, and `SYSCALL_ARCH_PRCTL`(107) with `ARCH_SET_FS`/`ARCH_GET_FS`. Phase 4 builds the pthread layer on top of these primitives.

### 4A: Thread Control Block

- [ ] **4A.1** Create `slibc/src/thread/tcb.rs`:
  - Define `#[repr(C)] pub struct Tcb`:
    - `self_ptr: *mut Tcb` — first field, pointed to by FS_BASE (required by x86_64 TLS ABI)
    - `errno_val: i32` — per-thread errno (replaces the static from Phase 1C)
    - `tid: i32` — thread ID (from clone return value)
    - `stack_base: *mut u8` — base of the mmap'd stack
    - `stack_size: usize` — size of the mmap'd stack
    - `tls_data: [u8; 64]` — space for compiler-generated TLS variables
    - `thread_local_keys: [*mut u8; 64]` — pthread_key values (Phase 4G)
    - `detached: bool` — true if pthread_detach was called
    - `child_tid: i32` — written by kernel on thread exit (CLONE_CHILD_CLEARTID target)
  - `impl Tcb`: `pub fn current() -> *mut Tcb` — reads FS_BASE via `rdfsbase` or `arch_prctl(ARCH_GET_FS)` and casts to `*mut Tcb`
  - `pub fn errno_ptr() -> *mut i32` — `&mut (*Tcb::current()).errno_val`

### 4B: TLS Initialization

- [ ] **4B.1** Create `slibc/src/thread/tls.rs`:
  - `pub fn tls_init_main_thread()` — called from `__libc_start_main` (update Phase 3A):
    - Allocates a `Tcb` on the heap via `malloc(size_of::<Tcb>())`
    - Sets `tcb.self_ptr = tcb_ptr`
    - Sets `tcb.tid = getpid()`
    - Calls `Sys::arch_prctl_set_fs(tcb_ptr as u64)` using `SYSCALL_ARCH_PRCTL`(107) with `ARCH_SET_FS`
    - Verifies with `Sys::arch_prctl_get_fs()` that FS_BASE was set correctly
  - `pub fn tls_init_new_thread(tcb: *mut Tcb)` — called from thread entry trampoline (Phase 4C)
- [ ] **4B.2** Update `slibc/src/errno.rs` to use per-thread errno:
  - Replace `static mut ERRNO_VAL: i32` with `fn errno_ptr() -> *mut i32 { Tcb::errno_ptr() }`
  - `errno_get()` reads `*errno_ptr()`
  - `errno_set(e)` writes `*errno_ptr()`
  - `__errno_location()` returns `errno_ptr()`
  - Add a fallback: if FS_BASE is 0 (TLS not yet initialized), use a static fallback for early-boot code

### 4C: pthread_create

- [ ] **4C.1** Create `slibc/src/thread/mod.rs`:
  - Define `pub type pthread_t = u64` — opaque thread handle (actually a pointer to TCB)
  - Define `pub struct pthread_attr_t { stack_size: usize, detach_state: i32 }` with default stack size 2MB
  - `PTHREAD_CREATE_JOINABLE: i32 = 0`, `PTHREAD_CREATE_DETACHED: i32 = 1`
- [ ] **4C.2** Implement `pthread_create(thread: *mut pthread_t, attr: *const pthread_attr_t, start: fn(*mut u8) -> *mut u8, arg: *mut u8) -> i32` in `slibc/src/thread/create.rs`:
  - Determine stack size from `attr` or default (2MB)
  - Allocate stack: `Sys::mmap(null, stack_size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)` using `SYSCALL_MMAP`(92)
  - Allocate and initialize `Tcb` at the top of the stack (or separately via malloc)
  - Set `tcb.stack_base`, `tcb.stack_size`, `tcb.self_ptr = tcb_ptr`
  - Call `Sys::clone` using `SYSCALL_CLONE`(101) with flags: `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID`
    - `CLONE_SETTLS`: passes `tcb_ptr` as the TLS argument — kernel sets FS_BASE for the new thread
    - `CLONE_PARENT_SETTID`: kernel writes child tid to `tcb.tid`
    - `CLONE_CHILD_CLEARTID`: kernel writes 0 to `tcb.child_tid` on thread exit (used by pthread_join)
  - Stack pointer for clone: top of allocated stack minus space for the trampoline frame
  - Write `start` function pointer and `arg` into the new thread's stack before calling clone
  - Store `tcb_ptr as pthread_t` in `*thread`
  - Returns 0 on success, errno value on failure
- [ ] **4C.3** Implement the thread entry trampoline in `slibc/src/thread/create.rs`:
  - `unsafe extern "C" fn thread_entry_trampoline()` — the function the new thread starts executing
  - Calls `tls_init_new_thread(tcb)` to ensure FS_BASE is set (CLONE_SETTLS should have done it, but verify)
  - Reads `start` and `arg` from the stack frame set up by `pthread_create`
  - Calls `start(arg)`, stores return value in TCB
  - Calls `pthread_exit(ret)`

### 4D: pthread_join and pthread_detach

- [ ] **4D.1** Implement `pthread_join(thread: pthread_t, retval: *mut *mut u8) -> i32` in `slibc/src/thread/join.rs`:
  - Cast `thread` back to `*mut Tcb`
  - Spin-wait (with `futex_wait`) on `tcb.child_tid` until it becomes 0 — `CLONE_CHILD_CLEARTID` causes the kernel to write 0 and do a `FUTEX_WAKE` when the thread exits
  - Specifically: `Sys::futex_wait(&tcb.child_tid, tid_value, null)` using `SYSCALL_FUTEX`(106) with `FUTEX_WAIT`
  - Copy thread return value to `*retval` if non-null
  - Free the thread's stack via `Sys::munmap` using `SYSCALL_MUNMAP`(93)
  - Free the TCB via `free(tcb_ptr)`
  - Returns 0 on success, `EINVAL` if thread is detached, `EDEADLK` if joining self
- [ ] **4D.2** Implement `pthread_detach(thread: pthread_t) -> i32`:
  - Sets `tcb.detached = true`
  - If thread has already exited (child_tid == 0): free stack and TCB immediately
  - Otherwise: the thread will free its own resources on exit
- [ ] **4D.3** Implement `pthread_exit(retval: *mut u8) -> !`:
  - Stores `retval` in TCB
  - If detached: free stack and TCB, then call `_exit(0)` — actually just exit the thread via `clone` semantics (the thread exits when its entry function returns)
  - If joinable: just return from the trampoline (kernel handles CLEARTID + FUTEX_WAKE)
- [ ] **4D.4** Implement `pthread_self() -> pthread_t`:
  - Returns `Tcb::current() as pthread_t`
- [ ] **4D.5** Implement `pthread_equal(t1: pthread_t, t2: pthread_t) -> i32`:
  - Returns non-zero if `t1 == t2`

### 4E: Mutexes

- [ ] **4E.1** Create `slibc/src/thread/mutex.rs`:
  - Define `#[repr(C)] pub struct pthread_mutex_t { state: i32, owner_tid: i32, kind: i32 }` — `state` is the futex word
  - `PTHREAD_MUTEX_NORMAL: i32 = 0`, `PTHREAD_MUTEX_RECURSIVE: i32 = 1`, `PTHREAD_MUTEX_ERRORCHECK: i32 = 2`
  - `PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t { state: 0, owner_tid: 0, kind: 0 }`
- [ ] **4E.2** Implement mutex operations:
  - `pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const pthread_mutexattr_t) -> i32` — zeroes the mutex
  - `pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> i32`:
    - Attempt CAS: `state: 0 -> 1` (unlocked to locked)
    - If CAS fails (mutex is locked): set `state = 2` (contended), call `Sys::futex_wait(&mutex.state, 2, null)` using `SYSCALL_FUTEX`(106) with `FUTEX_WAIT`
    - Loop until CAS succeeds
  - `pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> i32` — single CAS attempt, returns `EBUSY` if locked
  - `pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> i32`:
    - Atomic decrement: if `state` was 2 (contended), call `Sys::futex_wake(&mutex.state, 1)` using `SYSCALL_FUTEX`(106) with `FUTEX_WAKE`
    - Set `state = 0`
  - `pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> i32` — zeroes the mutex, returns `EBUSY` if locked
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 4F: Condition Variables

- [ ] **4F.1** Create `slibc/src/thread/condvar.rs`:
  - Define `#[repr(C)] pub struct pthread_cond_t { seq: u32, mutex: *mut pthread_mutex_t }` — `seq` is the futex word
  - `PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t { seq: 0, mutex: null_mut() }`
- [ ] **4F.2** Implement condition variable operations:
  - `pthread_cond_init(cond, attr) -> i32` — zeroes the condvar
  - `pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> i32`:
    - Save current `seq` value
    - Unlock `mutex` via `pthread_mutex_unlock`
    - Call `Sys::futex_wait(&cond.seq, saved_seq, null)` using `SYSCALL_FUTEX`(106) with `FUTEX_WAIT`
    - Re-lock `mutex` via `pthread_mutex_lock`
  - `pthread_cond_signal(cond: *mut pthread_cond_t) -> i32`:
    - Increment `cond.seq` atomically
    - Call `Sys::futex_wake(&cond.seq, 1)` using `SYSCALL_FUTEX`(106) with `FUTEX_WAKE`
  - `pthread_cond_broadcast(cond: *mut pthread_cond_t) -> i32`:
    - Increment `cond.seq` atomically
    - Call `Sys::futex_wake(&cond.seq, i32::MAX)` to wake all waiters
  - `pthread_cond_destroy(cond, attr) -> i32` — zeroes the condvar
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 4G: Thread-Local Keys

- [ ] **4G.1** Create `slibc/src/thread/keys.rs`:
  - `static mut KEY_DESTRUCTORS: [Option<extern "C" fn(*mut u8)>; 64] = [None; 64]`
  - `static mut KEY_USED: [bool; 64] = [false; 64]`
  - `pub type pthread_key_t = u32`
- [ ] **4G.2** Implement key operations:
  - `pthread_key_create(key: *mut pthread_key_t, destructor: Option<extern "C" fn(*mut u8)>) -> i32` — finds first unused slot, stores destructor, writes index to `*key`
  - `pthread_key_delete(key: pthread_key_t) -> i32` — marks slot as unused, clears destructor
  - `pthread_getspecific(key: pthread_key_t) -> *mut u8` — reads `Tcb::current().thread_local_keys[key]`
  - `pthread_setspecific(key: pthread_key_t, value: *mut u8) -> i32` — writes `Tcb::current().thread_local_keys[key] = value`
  - Call destructors for all non-null key values during `pthread_exit`
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 4H: Read-Write Locks

- [ ] **4H.1** Create `slibc/src/thread/rwlock.rs`:
  - Define `#[repr(C)] pub struct pthread_rwlock_t { state: i32, writer_waiting: i32 }` — `state > 0` means N readers, `state == -1` means writer holds lock
  - `PTHREAD_RWLOCK_INITIALIZER: pthread_rwlock_t = pthread_rwlock_t { state: 0, writer_waiting: 0 }`
- [ ] **4H.2** Implement rwlock operations:
  - `pthread_rwlock_init(rwlock, attr) -> i32` — zeroes the rwlock
  - `pthread_rwlock_rdlock(rwlock: *mut pthread_rwlock_t) -> i32` — spin + futex_wait until `state >= 0`, then increment `state`
  - `pthread_rwlock_tryrdlock(rwlock: *mut pthread_rwlock_t) -> i32` — single CAS attempt
  - `pthread_rwlock_wrlock(rwlock: *mut pthread_rwlock_t) -> i32` — increment `writer_waiting`, spin + futex_wait until `state == 0`, CAS to -1, decrement `writer_waiting`
  - `pthread_rwlock_trywrlock(rwlock: *mut pthread_rwlock_t) -> i32` — single CAS attempt
  - `pthread_rwlock_unlock(rwlock: *mut pthread_rwlock_t) -> i32` — if `state == -1` (writer): set to 0, futex_wake all; if `state > 0` (reader): decrement, if reaches 0 futex_wake writers
  - `pthread_rwlock_destroy(rwlock, attr) -> i32`
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### Phase 4 Gate

- [ ] **GATE**: `pthread_create` spawns a thread that runs concurrently with the main thread
- [ ] **GATE**: `pthread_join` waits for thread exit and retrieves return value
- [ ] **GATE**: `pthread_mutex_lock`/`unlock` prevents data races (verified with a counter test)
- [ ] **GATE**: `errno` is per-thread — two threads can set different errno values simultaneously
- [ ] **GATE**: FS_BASE is set correctly for both main thread and spawned threads
- [ ] **GATE**: `pthread_cond_wait`/`signal` wakes a waiting thread
- [ ] **GATE**: `pthread_key_create`/`setspecific`/`getspecific` stores per-thread values
- [ ] **GATE**: `just build` and `just test` pass

---

## 8. Phase 5: Rust std Port

> **The Jackpot — full Rust std in SlopOS userland.**
> **Userland changes only**: Yes — custom Rust std PAL, build config
> **Difficulty**: Very High
> **Depends on**: Phase 1 (GlobalAlloc), Phase 2 (stdio), Phase 3 (process), Phase 4 (threading)

### Background

With slibc providing malloc, stdio, process lifecycle, and pthread, the kernel ABI is rich enough to implement every component of Rust's `std` PAL. The target JSON needs one key change (`"os": "slopos"`) and a new PAL directory must be added to the Rust standard library source.

### 5A: Custom Target Evolution

- [ ] **5A.1** Update `targets/x86_64-slos-userland.json`:
  - Change `"os": "none"` to `"os": "slopos"` — this enables `cfg(target_os = "slopos")` in all Rust code
  - Add `"env": "slibc"` — identifies the C runtime environment
  - Add `"has-thread-local": true` — tells the compiler that `#[thread_local]` works (via FS_BASE)
  - Add `"tls-model": "local-exec"` — use the local-exec TLS model for `#[thread_local]` statics
  - Keep existing settings: `"panic": "abort"`, `"relocation-model": "static"`, `"disable-redzone": true`
- [ ] **5A.2** Add `.cargo/config.toml` to the workspace root (or update if it exists):
  - `[build] target = "x86_64-slos-userland"` — default target for all builds
  - `[unstable] build-std = ["core", "alloc", "std"]` — build std from source
  - `[unstable] build-std-features = ["compiler-builtins-mem"]` — use compiler-builtins for memcpy etc.

### 5B: Fork Rust std

- [ ] **5B.1** Obtain Rust std source matching the pinned nightly in `rust-toolchain.toml`:
  - Use `rustup component add rust-src` to get the source
  - The source lives at `$(rustc --print sysroot)/lib/rustlib/src/rust/library/`
- [ ] **5B.2** Create `slibc/std_pal/` directory to hold the SlopOS PAL implementation:
  - This mirrors the structure of `library/std/src/sys/pal/` in the Rust source
  - Files to create: `mod.rs`, `alloc.rs`, `stdio.rs`, `fs.rs`, `process.rs`, `thread.rs`, `net.rs`, `time.rs`, `os.rs`, `locks/mod.rs`, `locks/mutex.rs`, `locks/condvar.rs`, `locks/rwlock.rs`
- [ ] **5B.3** Create a `rust-std-patch/` directory with patch files that add the SlopOS PAL to the Rust std source:
  - Patch `library/std/src/sys/pal/mod.rs` to add `#[cfg(target_os = "slopos")] mod slopos;`
  - Patch `library/std/src/sys/mod.rs` to route `cfg(target_os = "slopos")` to the slopos PAL
  - Document the patch application process in `slibc/std_pal/README.md`

### 5C: Implement std PAL — alloc

- [ ] **5C.1** Create `slibc/std_pal/alloc.rs`:
  - `pub struct System;`
  - `unsafe impl GlobalAlloc for System`:
    - `alloc(layout) -> *mut u8`: calls `slibc::mem::malloc(layout.size())` then aligns if needed
    - `dealloc(ptr, _layout)`: calls `slibc::mem::free(ptr)`
    - `alloc_zeroed(layout) -> *mut u8`: calls `slibc::mem::calloc(1, layout.size())`
    - `realloc(ptr, _old, new_layout) -> *mut u8`: calls `slibc::mem::realloc(ptr, new_layout.size())`
  - This replaces the `unsupported` PAL's alloc which panics

### 5D: Implement std PAL — stdio

- [ ] **5D.1** Create `slibc/std_pal/stdio.rs`:
  - `pub struct Stdin;`, `pub struct Stdout;`, `pub struct Stderr;`
  - `impl io::Read for Stdin`: calls `slibc::pal::Sys::read(0, buf)` using `SYSCALL_READ`(16)
  - `impl io::Write for Stdout`: calls `slibc::pal::Sys::write(1, buf)` using `SYSCALL_WRITE`(17)
  - `impl io::Write for Stderr`: calls `slibc::pal::Sys::write(2, buf)` using `SYSCALL_WRITE`(17)
  - `pub fn panic_output() -> Option<impl io::Write>` — returns `Some(Stderr)` so `panic!` messages go to stderr
  - This enables `println!`, `eprintln!`, `print!`, `eprint!` macros

### 5E: Implement std PAL — fs

- [ ] **5E.1** Create `slibc/std_pal/fs.rs`:
  - `pub struct File { fd: i32 }`
  - `impl File`: `open(path, opts) -> io::Result<File>` — calls `Sys::open` using `SYSCALL_OPEN`(14)
  - `impl io::Read for File`: calls `Sys::read` using `SYSCALL_READ`(16)
  - `impl io::Write for File`: calls `Sys::write` using `SYSCALL_WRITE`(17)
  - `impl io::Seek for File`: calls `Sys::lseek` using `SYSCALL_LSEEK`(99)
  - `impl Drop for File`: calls `Sys::close` using `SYSCALL_CLOSE`(15)
  - `pub fn stat(path) -> io::Result<FileStat>` — calls `Sys::stat` using `SYSCALL_STAT`(18)
  - `pub fn rename(from, to) -> io::Result<()>` — calls `Sys::rename` using `SYSCALL_RENAME`(122)
  - `pub fn remove_file(path) -> io::Result<()>` — calls `Sys::unlink` using `SYSCALL_UNLINK`(20)
  - `pub fn create_dir(path) -> io::Result<()>` — calls `Sys::mkdir` using `SYSCALL_MKDIR`(19)
  - `pub struct ReadDir { ... }` — wraps `SYSCALL_LIST`(21) for directory iteration

### 5F: Implement std PAL — process

- [ ] **5F.1** Create `slibc/std_pal/process.rs`:
  - `pub struct Command { program: String, args: Vec<String>, env: Vec<(String, String)> }`
  - `impl Command`: `spawn() -> io::Result<Child>` — calls `slibc::process::fork()` then `slibc::process::execve()` in child
  - `pub struct Child { pid: i32 }` — represents a spawned child process
  - `impl Child`: `wait() -> io::Result<ExitStatus>` — calls `slibc::process::waitpid()`
  - `pub fn exit(code: i32) -> !` — calls `slibc::process::exit(code)`
  - `pub fn abort() -> !` — calls `slibc::signal::abort()`

### 5G: Implement std PAL — thread

- [ ] **5G.1** Create `slibc/std_pal/thread.rs`:
  - `pub struct Thread { handle: pthread_t }`
  - `pub fn spawn(f: Box<dyn FnOnce() + Send + 'static>) -> io::Result<Thread>`:
    - Boxes the closure, calls `slibc::thread::pthread_create` with a trampoline that calls the boxed closure
    - Returns `Thread { handle: tid }`
  - `impl Thread`: `join(self) -> io::Result<()>` — calls `slibc::thread::pthread_join`
  - `pub fn sleep(dur: Duration)` — converts `Duration` to milliseconds, calls `Sys::sleep_ms` using `SYSCALL_SLEEP_MS`(5)
  - `pub fn yield_now()` — calls `Sys::yield_now` using `SYSCALL_YIELD`(0)
  - `pub fn current() -> Thread` — returns `Thread { handle: pthread_self() }`

### 5H: Implement std PAL — net

- [ ] **5H.1** Create `slibc/std_pal/net.rs`:
  - `pub struct TcpStream { fd: i32 }`
  - `impl TcpStream`: `connect(addr: SocketAddr) -> io::Result<TcpStream>` — calls `Sys::socket`(126) + `Sys::connect`(130)
  - `impl io::Read for TcpStream`: calls `Sys::recv` using `SYSCALL_RECV`(132)
  - `impl io::Write for TcpStream`: calls `Sys::send` using `SYSCALL_SEND`(131)
  - `impl Drop for TcpStream`: calls `Sys::close` using `SYSCALL_CLOSE`(15)
  - `pub struct TcpListener { fd: i32 }`
  - `impl TcpListener`: `bind(addr: SocketAddr) -> io::Result<TcpListener>` — calls `Sys::socket`(126) + `Sys::bind`(127) + `Sys::listen`(128)
  - `impl TcpListener`: `accept() -> io::Result<(TcpStream, SocketAddr)>` — calls `Sys::accept` using `SYSCALL_ACCEPT`(129)
  - `pub struct UdpSocket { fd: i32 }`
  - `impl UdpSocket`: `bind`, `send_to`, `recv_from` — calls `Sys::sendto`(133) / `Sys::recvfrom`(134)

### 5I: Implement std PAL — time

- [ ] **5I.1** Create `slibc/std_pal/time.rs`:
  - `pub struct Instant { ns: u64 }` — monotonic time
  - `impl Instant`: `now() -> Instant` — calls `Sys::clock_gettime` using `SYSCALL_CLOCK_GETTIME`(125) with `CLOCK_MONOTONIC`
  - `impl Instant`: `elapsed() -> Duration`, `duration_since(earlier: Instant) -> Duration`
  - `pub struct SystemTime { ns: u64 }` — wall-clock time
  - `impl SystemTime`: `now() -> SystemTime` — calls `Sys::clock_gettime` with `CLOCK_REALTIME`
  - `UNIX_EPOCH: SystemTime = SystemTime { ns: 0 }`

### 5J: Implement std PAL — env

- [ ] **5J.1** Create `slibc/std_pal/os.rs`:
  - `pub fn args() -> Args` — returns an iterator over `argv` stored during `__libc_start_main`
  - `pub fn vars() -> Vars` — returns an iterator over `environ` key-value pairs
  - `pub fn var(key: &str) -> Result<String, VarError>` — calls `slibc::env::getenv`
  - `pub fn set_var(key: &str, value: &str)` — calls `slibc::env::setenv`
  - `pub fn remove_var(key: &str)` — calls `slibc::env::unsetenv`
  - `pub fn current_dir() -> io::Result<PathBuf>` — calls `Sys::getcwd` using `SYSCALL_GETCWD`(121)
  - `pub fn set_current_dir(path: &Path) -> io::Result<()>` — calls `Sys::chdir` using `SYSCALL_CHDIR`(124)

### 5K: Implement std PAL — locks

- [ ] **5K.1** Create `slibc/std_pal/locks/mutex.rs`:
  - `pub struct Mutex { inner: pthread_mutex_t }`
  - `impl Mutex`: `new() -> Mutex`, `lock()`, `try_lock() -> bool`, `unlock()`
  - Delegates to `slibc::thread::pthread_mutex_lock/unlock/trylock`
- [ ] **5K.2** Create `slibc/std_pal/locks/condvar.rs`:
  - `pub struct Condvar { inner: pthread_cond_t }`
  - `impl Condvar`: `new() -> Condvar`, `wait(mutex)`, `notify_one()`, `notify_all()`
  - Delegates to `slibc::thread::pthread_cond_wait/signal/broadcast`
- [ ] **5K.3** Create `slibc/std_pal/locks/rwlock.rs`:
  - `pub struct RwLock { inner: pthread_rwlock_t }`
  - `impl RwLock`: `new() -> RwLock`, `read()`, `write()`, `try_read() -> bool`, `try_write() -> bool`, `read_unlock()`, `write_unlock()`
  - Delegates to `slibc::thread::pthread_rwlock_*`

### 5L: Build Integration

- [ ] **5L.1** Configure `-Zbuild-std=core,alloc,std` in `.cargo/config.toml` for the userland target
- [ ] **5L.2** Ensure `slopos-slibc` is linked into all userland binaries via `build.rs` or linker flags
- [ ] **5L.3** Verify that `use std::fs::File` compiles and works in a test userland program
- [ ] **5L.4** Verify that `use std::net::TcpStream` compiles and connects to a server
- [ ] **5L.5** Verify that `use std::thread::spawn` compiles and runs a thread
- [ ] **5L.6** Verify that `use std::collections::HashMap` compiles and inserts/retrieves values

### Phase 5 Gate

- [ ] **GATE**: `println!("Hello from SlopOS!")` works in a userland program
- [ ] **GATE**: `std::fs::read_to_string("/etc/hostname")` reads a file
- [ ] **GATE**: `std::net::TcpStream::connect("10.0.2.2:8080")` connects to a host
- [ ] **GATE**: `std::thread::spawn(|| { ... }).join()` runs a thread and joins it
- [ ] **GATE**: `std::collections::HashMap::new()` inserts and retrieves values
- [ ] **GATE**: `std::env::args()` returns the program's arguments
- [ ] **GATE**: `std::time::Instant::now()` returns a monotonic timestamp
- [ ] **GATE**: `just build` and `just test` pass

---

## 9. Phase 6: Networking, Time, and Polish

> **The Final Enchantments — completing the POSIX surface.**
> **Userland changes only**: Yes — new slibc modules
> **Difficulty**: Medium
> **Depends on**: Phase 1 (PAL), Phase 4 (threading for thread-safe sockets)

### 6A: Complete Socket API

- [ ] **6A.1** Create `slibc/src/net/mod.rs` with the full POSIX socket API:
  - `socket(domain: i32, sock_type: i32, protocol: i32) -> i32` — calls `Sys::socket` using `SYSCALL_SOCKET`(126)
  - `bind(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32` — calls `Sys::bind` using `SYSCALL_BIND`(127)
  - `listen(fd: i32, backlog: i32) -> i32` — calls `Sys::listen` using `SYSCALL_LISTEN`(128)
  - `accept(fd: i32, addr: *mut SockAddr, addrlen: *mut u32) -> i32` — calls `Sys::accept` using `SYSCALL_ACCEPT`(129)
  - `connect(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32` — calls `Sys::connect` using `SYSCALL_CONNECT`(130)
  - `send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize` — calls `Sys::send` using `SYSCALL_SEND`(131)
  - `recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize` — calls `Sys::recv` using `SYSCALL_RECV`(132)
  - `sendto(fd, buf, len, flags, addr, addrlen) -> isize` — calls `Sys::sendto` using `SYSCALL_SENDTO`(133)
  - `recvfrom(fd, buf, len, flags, addr, addrlen) -> isize` — calls `Sys::recvfrom` using `SYSCALL_RECVFROM`(134)
  - `setsockopt(fd, level, optname, optval, optlen) -> i32` — calls `Sys::setsockopt` using `SYSCALL_SETSOCKOPT`(136)
  - `getsockopt(fd, level, optname, optval, optlen) -> i32` — calls `Sys::getsockopt` using `SYSCALL_GETSOCKOPT`(137)
  - `shutdown(fd: i32, how: i32) -> i32` — calls `Sys::shutdown` using `SYSCALL_SHUTDOWN`(138)
  - `getpeername(fd, addr, addrlen) -> i32` — calls `Sys::getpeername` if available, else stub returning `ENOSYS`
  - `getsockname(fd, addr, addrlen) -> i32` — calls `Sys::getsockname` if available, else stub
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`
- [ ] **6A.2** Define socket address types in `slibc/src/net/addr.rs`:
  - `#[repr(C)] pub struct SockAddr { sa_family: u16, sa_data: [u8; 14] }`
  - `#[repr(C)] pub struct SockAddrIn { sin_family: u16, sin_port: u16, sin_addr: u32, sin_zero: [u8; 8] }`
  - `AF_INET: i32 = 2`, `AF_INET6: i32 = 10`, `AF_UNIX: i32 = 1`
  - `SOCK_STREAM: i32 = 1`, `SOCK_DGRAM: i32 = 2`, `SOCK_NONBLOCK: i32 = 2048`, `SOCK_CLOEXEC: i32 = 524288`
  - `IPPROTO_TCP: i32 = 6`, `IPPROTO_UDP: i32 = 17`
  - `htons(x: u16) -> u16`, `ntohs(x: u16) -> u16`, `htonl(x: u32) -> u32`, `ntohl(x: u32) -> u32` — byte-order conversion
  - `inet_addr(cp: *const u8) -> u32` — parse dotted-decimal IPv4 address
  - `inet_ntoa(addr: u32) -> *const u8` — format IPv4 address (uses a static buffer)

### 6B: DNS / getaddrinfo

- [ ] **6B.1** Create `slibc/src/net/dns.rs`:
  - `getaddrinfo(node, service, hints, res) -> i32` — simplified implementation:
    - If `node` is a dotted-decimal IP: parse directly via `inet_addr`, skip DNS
    - Otherwise: call `Sys::resolve` using `SYSCALL_RESOLVE`(135) to get IPv4 address
    - Allocate `addrinfo` struct via `malloc`, fill with result, set `*res`
    - Returns 0 on success, `EAI_NONAME` on failure
  - `freeaddrinfo(res: *mut AddrInfo)` — frees the linked list allocated by `getaddrinfo`
  - `gai_strerror(errcode: i32) -> *const u8` — returns error string for `getaddrinfo` error codes
  - `#[repr(C)] pub struct AddrInfo { ai_flags, ai_family, ai_socktype, ai_protocol, ai_addrlen, ai_addr, ai_canonname, ai_next }`
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 6C: Time Functions

- [ ] **6C.1** Create `slibc/src/time/mod.rs`:
  - `#[repr(C)] pub struct Timespec { tv_sec: i64, tv_nsec: i64 }`
  - `#[repr(C)] pub struct Timeval { tv_sec: i64, tv_usec: i64 }`
  - `clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32` — calls `Sys::clock_gettime` using `SYSCALL_CLOCK_GETTIME`(125)
  - `gettimeofday(tv: *mut Timeval, tz: *mut u8) -> i32` — calls `clock_gettime(CLOCK_REALTIME)`, converts to microseconds
  - `time(tloc: *mut i64) -> i64` — returns seconds since epoch via `clock_gettime`
  - `nanosleep(req: *const Timespec, rem: *mut Timespec) -> i32` — converts to milliseconds, calls `Sys::sleep_ms` using `SYSCALL_SLEEP_MS`(5)
  - `usleep(usec: u32) -> i32` — `nanosleep` with microsecond conversion
  - `sleep(seconds: u32) -> u32` — `nanosleep` with second conversion, returns 0
  - `CLOCK_REALTIME: i32 = 0`, `CLOCK_MONOTONIC: i32 = 1`
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 6D: poll and select

- [ ] **6D.1** Create `slibc/src/io/poll.rs`:
  - `#[repr(C)] pub struct Pollfd { fd: i32, events: i16, revents: i16 }`
  - `POLLIN: i16 = 1`, `POLLOUT: i16 = 4`, `POLLERR: i16 = 8`, `POLLHUP: i16 = 16`, `POLLNVAL: i16 = 32`
  - `poll(fds: *mut Pollfd, nfds: u32, timeout: i32) -> i32` — calls `Sys::poll` using `SYSCALL_POLL`(108)
  - `#[repr(C)] pub struct FdSet { fds_bits: [u64; 16] }` — 1024-bit fd set
  - `FD_ZERO(set: *mut FdSet)`, `FD_SET(fd, set)`, `FD_CLR(fd, set)`, `FD_ISSET(fd, set) -> bool` — as `pub unsafe fn`
  - `select(nfds, readfds, writefds, exceptfds, timeout) -> i32` — calls `Sys::select` using `SYSCALL_SELECT`(109)
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 6E: ioctl and termios

- [ ] **6E.1** Create `slibc/src/tty/mod.rs`:
  - `ioctl(fd: i32, request: u64, arg: u64) -> i32` — calls `Sys::ioctl` using `SYSCALL_IOCTL`(112)
  - `tcgetattr(fd: i32, termios: *mut Termios) -> i32` — calls `ioctl(fd, TCGETS, termios as u64)`
  - `tcsetattr(fd: i32, optional_actions: i32, termios: *const Termios) -> i32` — calls `ioctl(fd, TCSETS, termios as u64)`
  - `cfmakeraw(termios: *mut Termios)` — clears ICANON, ECHO, ECHOE, ECHOK, ECHONL, ISIG, IEXTEN, OPOST; sets VMIN=1, VTIME=0
  - `cfgetispeed(termios: *const Termios) -> u32`, `cfsetispeed(termios: *mut Termios, speed: u32) -> i32`
  - `Termios` struct imported from `slopos_abi` (already defined there with all fields)
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### 6F: Miscellaneous POSIX

- [ ] **6F.1** Create `slibc/src/io/misc.rs` with remaining POSIX file operations:
  - `access(path: *const u8, mode: i32) -> i32` — stub: calls `stat`, returns 0 if file exists, -1 otherwise
  - `umask(mask: u32) -> u32` — stub returning 0022 (no kernel support needed for basic use)
  - `chmod(path: *const u8, mode: u32) -> i32` — stub returning `ENOSYS` (kernel doesn't have chmod yet)
  - `pipe(pipefd: *mut [i32; 2]) -> i32` — calls `Sys::pipe` using `SYSCALL_PIPE`(110)
  - `dup(oldfd: i32) -> i32` — calls `Sys::dup` using `SYSCALL_DUP`(95)
  - `dup2(oldfd: i32, newfd: i32) -> i32` — calls `Sys::dup2` using `SYSCALL_DUP2`(96)
  - `fcntl(fd: i32, cmd: i32, arg: i64) -> i32` — calls `Sys::fcntl` using `SYSCALL_FCNTL`(98)
  - `isatty(fd: i32) -> i32` — calls `tcgetattr`, returns 1 if succeeds, 0 otherwise
  - Each exported as `#[no_mangle] pub unsafe extern "C" fn`

### Phase 6 Gate

- [ ] **GATE**: `socket`/`bind`/`listen`/`accept`/`connect`/`send`/`recv` all work end-to-end
- [ ] **GATE**: `getaddrinfo("example.com", "80", ...)` resolves via `SYSCALL_RESOLVE`(135)
- [ ] **GATE**: `clock_gettime(CLOCK_MONOTONIC, &ts)` returns a valid timestamp
- [ ] **GATE**: `nanosleep(&req, null)` sleeps for the requested duration
- [ ] **GATE**: `poll(fds, nfds, timeout)` returns correct readiness for socket and file FDs
- [ ] **GATE**: `tcgetattr`/`tcsetattr`/`cfmakeraw` work on the terminal fd
- [ ] **GATE**: `pipe`/`dup`/`dup2` work correctly
- [ ] **GATE**: `just build` and `just test` pass

---

## 10. Dependency Graph

```
Phase 0: Extract & Standalone
  (slibc/ workspace crate, no new functionality)
         |
         v
Phase 1: PAL + Core libc ─────────────────────────────────────────┐
  (Pal trait, Sys impl, Errno, string.h, GlobalAlloc, malloc)      │
         |                                                          │
         v                                                          │
Phase 2: stdio ──────────────────────────────────────────────────┐ │
  (FILE, printf, buffered I/O)                                    │ │
         |                                                         │ │
         v                                                         │ │
Phase 3: Process + Signals + Env ────────────────────────────────┐│ │
  (fork/exec/waitpid, signal(), atexit, getenv)                  ││ │
         |                                                        ││ │
         v                                                        ││ │
Phase 4: Threading ──────────────────────────────────────────────┘│ │
  (TCB, TLS, pthread_create, mutex, condvar, rwlock, keys)        │ │
         |                                                         │ │
         v                                                         │ │
Phase 5: Rust std Port ◄─────────────────────────────────────────┘ │
  (custom PAL, -Zbuild-std, std::fs/net/thread/io)                  │
         |                                                           │
         v                                                           │
Phase 6: Networking + Time + Polish ◄───────────────────────────────┘
  (socket API, getaddrinfo, clock_gettime, poll, termios)

Kernel ABI (abi/ crate) ──────────────────────────────────────────────
  140 syscalls, errno constants, mmap/clone/futex/socket constants
  Used by ALL phases via slibc/src/pal/raw.rs + slibc/src/pal/slopos.rs
```

### Recommended Execution Order

| Order | Phase | Rationale |
|---|---|---|
| 1st | Phase 0 (Extract) | Zero risk, enables all subsequent work |
| 2nd | Phase 1 (PAL + Core) | Foundation everything else builds on |
| 3rd | Phase 2 (stdio) | Unlocks printf — makes debugging vastly easier |
| 4th | Phase 3 (Process) | Needed for proper startup and signal handling |
| 5th | Phase 4 (Threading) | Required for Rust std's sync primitives |
| 6th | Phase 5 (Rust std) | The jackpot — requires all prior phases |
| 7th | Phase 6 (Net + Time) | Polish and POSIX completeness |

---

## 11. Blocked Features Reference

Features that cannot be implemented until specific phases complete:

| Feature | Blocked By | Phase Required |
|---|---|---|
| `extern crate alloc` in userland | No `#[global_allocator]` | Phase 1F (GlobalAlloc) |
| `Vec`, `String`, `Box` in userland | No GlobalAlloc | Phase 1F |
| `printf` / formatted output | No stdio | Phase 2E |
| Buffered file I/O | No FILE abstraction | Phase 2C |
| Signal handlers | No sigaction wrapper | Phase 3C |
| `atexit` cleanup on exit | No atexit implementation | Phase 3E |
| Per-thread errno | No TCB / TLS | Phase 4B |
| Multi-threaded programs | No pthread_create | Phase 4C |
| Mutex / condvar / rwlock | No pthread sync primitives | Phase 4E/4F/4H |
| `#[thread_local]` statics | No FS_BASE / TCB | Phase 4B |
| `use std::io` | No Rust std PAL | Phase 5 |
| `use std::fs` | No Rust std PAL | Phase 5E |
| `use std::net` | No Rust std PAL | Phase 5H |
| `use std::thread` | No Rust std PAL | Phase 5G |
| `use std::collections::HashMap` | No GlobalAlloc + std | Phase 1F + Phase 5 |
| `println!` macro | No Rust std PAL | Phase 5D |
| `getaddrinfo` / DNS from libc | No getaddrinfo wrapper | Phase 6B |
| `poll()` / `select()` from libc | No poll wrapper | Phase 6D |
| Raw terminal mode (cfmakeraw) | No termios wrapper | Phase 6E |
| `clock_gettime` from libc | No time wrapper | Phase 6C |
| `nanosleep` / `usleep` / `sleep` | No time wrapper | Phase 6C |

---

## 12. Progress Tracking

| Phase | Status | Tasks | Done | Blocked |
|---|---|---|---|---|
| **Phase 0**: Extract and Standalone | ✅ Complete | 18 | 18 | — |
| **Phase 1**: PAL and Core libc | ✅ Complete | 22 | 22 | — |
| **Phase 2**: stdio | ✅ Complete | 21 | 21 | — |
| **Phase 3**: Process, Signals, Env | ✅ Complete | 19 | 19 | — |
| **Phase 4**: Threading | Not Started | 26 | 0 | Phase 1 ✅, 3 ✅ |
| **Phase 5**: Rust std Port | Not Started | 30 | 0 | Phases 1–4 |
| **Phase 6**: Networking, Time, Polish | Not Started | 22 | 0 | Phase 1 ✅, 4 |
| **Total** | | **158** | **80** | |
