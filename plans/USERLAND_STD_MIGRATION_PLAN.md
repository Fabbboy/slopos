# SlopOS Userland `no_std` → `std` Migration Plan

> **Status**: Not Started
> **Target**: Migrate all userland code from `no_std`-era patterns to idiomatic Rust `std`, leveraging the completed slibc PAL
> **Scope**: Userland only (`userland/`). No kernel changes. slibc is assumed complete (Phase 6 of SLIBC_PLAN.md).
> **Prerequisite**: [SLIBC_PLAN.md](./SLIBC_PLAN.md) — all phases complete

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [What Stays as Raw Syscalls](#3-what-stays-as-raw-syscalls)
4. [What Gets Deleted After Migration](#4-what-gets-deleted-after-migration)
5. [Phase 0: Foundation — Entry Points & Feature Gates](#5-phase-0-foundation--entry-points--feature-gates)
6. [Phase 1: I/O Layer — println and std::io](#6-phase-1-io-layer--println-and-stdio)
7. [Phase 2: String & Number Formatting — Kill Manual Formatters](#7-phase-2-string--number-formatting--kill-manual-formatters)
8. [Phase 3: File Operations — std::fs](#8-phase-3-file-operations--stdfs)
9. [Phase 4: Process Management — std::process and std::env](#9-phase-4-process-management--stdprocess-and-stdenv)
10. [Phase 5: App-by-App Modernization](#10-phase-5-app-by-app-modernization)
11. [Phase 6: Cleanup & Deletion](#11-phase-6-cleanup--deletion)
12. [Dependency Graph](#12-dependency-graph)
13. [Verification Strategy](#13-verification-strategy)
14. [Progress Tracking](#14-progress-tracking)

---

## 1. Executive Summary

slibc is done. The Wheel of Fate has granted SlopOS a fully functional Rust `std` in userland — `std::fs`, `std::process`, `std::io`, `std::net`, `std::thread`, `std::time`, `std::env`, `std::path`, `std::alloc` — all wired through the slibc PAL to SlopOS syscalls.

But the userland code was born in the `no_std` darkness. Every app still writes `tty::write(b"hello\n")` instead of `println!("hello")`. Every number is formatted by hand with `write_u8_dec()` loops copied across 6 files. Every file operation goes through raw `fs::open_path()` / `fs::close_fd()` syscall wrappers. Paths live in `[u8; 256]` null-terminated byte arrays. Entry points are naked assembly trampolines.

This plan migrates **every line of userland code** to idiomatic `std` Rust. The goal is not just "make it compile with std" — it's to fundamentally change how the code leverages normal Rust: `String` instead of `&[u8]`, `format!()` instead of manual digit loops, `File::open()` instead of raw fd juggling, `fn main()` instead of naked `_start`.

| Metric | Before | After |
|---|---|---|
| **Entry points** | 10 naked `_start` trampolines | 10 `fn main()` functions |
| **I/O calls** | `tty::write(b"text\n")` | `println!("text")` |
| **Number formatting** | ~200 lines of manual `write_u8_dec` / `write_u16_dec` / `write_u32_dec` duplicated across 6 files | `format!()` / `write!()` |
| **File operations** | `fs::open_path()` + `fs::close_fd()` + `FdGuard` | `File::open()` / `File::create()` |
| **Path handling** | `[u8; 256]` null-terminated | `PathBuf` / `&Path` |
| **Process spawning** | `process::spawn_path_with_argv()` | `std::process::Command` (where possible) |
| **String type** | `&[u8]` byte slices | `&str` / `String` |
| **Error handling** | `SyscallError` / `SyscallResult` | `std::io::Error` / `io::Result` |
| **Sleep** | `sys_core::sleep_ms(100)` | `thread::sleep(Duration::from_millis(100))` |
| **Estimated LOC removed** | ~1500 lines of manual formatting, raw wrappers, entry plumbing | — |

This plan has **7 phases** (0–6), **~85 tasks**, ordered by dependency chain.

---

## 2. Current State Assessment

### 2.1 Build Pipeline

| Component | Current Value |
|---|---|
| Target spec | `targets/x86_64-slos-userland.json` |
| OS field | `"os": "slopos"` |
| Env field | `"env": "slibc"` |
| Linker | `rust-lld` via `userland/userland.ld` |
| Entry point | `ENTRY(_start)` at `0x400000` |
| Panic strategy | `abort` |
| Feature gate | `#![feature(restricted_std)]` in lib.rs and all bins |

### 2.2 File Inventory

| File | Lines | Migration Scope |
|---|---|---|
| `lib.rs` | 15 | Feature gate removal, re-exports |
| `runtime.rs` | 3 | Delete entirely |
| `syscall/mod.rs` | 62 | Gut — keep SlopOS-specific only |
| `syscall/raw.rs` | 1 | Keep (re-export from slibc) |
| `syscall/core.rs` | 88 | Partial delete — keep `random_next`, `sys_info`, `get_cpu_count`, `get_current_cpu` |
| `syscall/fs.rs` | 298 | Delete entirely — replaced by `std::fs` |
| `syscall/tty.rs` | 19 | Delete entirely — replaced by `std::io` |
| `syscall/process.rs` | 164 | Partial delete — keep `spawn_path_with_attrs`, `terminate_task`, `halt`, `reboot` |
| `syscall/memory.rs` | 80 | Partial delete — keep SHM ops, delete brk/sbrk |
| `syscall/net.rs` | 241 | Delete entirely — replaced by `std::net` |
| `syscall/input.rs` | 102 | Keep entirely (SlopOS-specific) |
| `syscall/window.rs` | 153 | Keep entirely (SlopOS-specific) |
| `syscall/roulette.rs` | 21 | Keep entirely (SlopOS-specific) |
| `syscall/error.rs` | 1 | Delete — replaced by `std::io::Error` |
| `syscall/numbers.rs` | 6 | Keep (ABI re-exports) |
| `syscall/wrappers/fd.rs` | 49 | Delete — replaced by `std::fs::File` |
| `syscall/wrappers/shm.rs` | 180 | Keep (SlopOS-specific) |
| `appkit/mod.rs` | 91 | Delete `entry!()` macro (lines 41–91) |
| `bin/shell.rs` | 3 | Rewrite to `fn main()` |
| `bin/init.rs` | 3 | Rewrite to `fn main()` |
| `bin/sysinfo.rs` | 3 | Rewrite to `fn main()` |
| `bin/roulette.rs` | 3 | Rewrite to `fn main()` |
| `bin/compositor.rs` | 3 | Rewrite to `fn main()` |
| `bin/file_manager.rs` | 3 | Rewrite to `fn main()` |
| `bin/nmap.rs` | 3 | Rewrite to `fn main()` |
| `bin/ifconfig.rs` | 3 | Rewrite to `fn main()` |
| `bin/nc.rs` | 23 | Rewrite to `fn main()` with `std::env::args()` |
| `bin/tests/fork_test.rs` | 72 | Rewrite to `fn main()` |
| `apps/init_process.rs` | 34 | Replace `tty::write` with `eprintln!` |
| `apps/roulette.rs` | 57 | Replace `tty::write` with `println!` |
| `apps/sysinfo.rs` | 178 | Replace `format_line`/`copy_bytes` with `format!()` |
| `apps/ifconfig.rs` | 145 | Full rewrite — ~70 lines of manual formatting → ~20 lines with `println!` |
| `apps/nmap.rs` | 146 | Full rewrite — same pattern as ifconfig |
| `apps/file_manager.rs` | 194 | Replace `fs::list_dir` with `std::fs::read_dir`, byte paths with `PathBuf` |
| `apps/nc/mod.rs` | 915 | Major cleanup — delete ~200 lines of manual formatters, use `std::env::args` |
| `apps/nc/tcp.rs` | ~200 | Replace raw socket syscalls with `std::net::TcpStream`/`TcpListener` |
| `apps/nc/udp.rs` | ~150 | Replace raw socket syscalls with `std::net::UdpSocket` |
| `apps/shell/mod.rs` | 296 | Replace `SyncUnsafeCell` statics, byte literals with `&str` |
| `apps/shell/builtins/*.rs` | ~500 | Replace `tty::write`/`shell_write` patterns, use `format!()`, `std::fs` |
| `apps/shell/exec.rs` | 931 | Partial — keep fork/pipe/dup2 for pipelines, use `std::process` for simple spawns |
| `apps/shell/display.rs` | ~200 | Replace byte-level output with `write!()` |
| `apps/shell/env.rs` | ~100 | Consider migrating to `std::env` backing |
| `apps/shell/jobs.rs` | ~150 | Replace manual number formatting |
| `apps/shell/parser.rs` | ~200 | Replace `u_strlen`/`u_streq_slice` with `str` methods |
| `apps/shell/input.rs` | ~200 | Replace raw TTY reads with `std::io::stdin()` |
| `apps/shell/banner.rs` | ~50 | Replace `tty::write` with `print!()` |
| `apps/shell/buffers.rs` | ~100 | Replace byte buffers with `String`/`Vec<u8>` |
| `program_registry.rs` | ~100 | Replace `&[u8]` program names with `&str` |
| `theme.rs` | ~50 | No change needed (color constants) |
| `ui_utils.rs` | ~50 | Evaluate if still needed |
| `gfx/mod.rs` | ~200 | No change (pixel-level rendering, not I/O) |
| `gfx/font.rs` | ~200 | No change (bitmap font rendering) |

### 2.3 Duplicated Code Inventory

Manual number formatters duplicated across the codebase — all deleted in Phase 2:

| Function | Files Where Duplicated |
|---|---|
| `write_u8_dec()` | `ifconfig.rs`, `nmap.rs`, `nc/mod.rs` |
| `write_u16_dec()` | `ifconfig.rs`, `nc/mod.rs` |
| `write_u32_dec()` | `nc/mod.rs` |
| `write_hex_byte()` | `ifconfig.rs`, `nmap.rs` |
| `write_ipv4()` | `ifconfig.rs`, `nmap.rs`, `nc/mod.rs` |
| `write_out()` | `ifconfig.rs`, `nmap.rs`, `nc/mod.rs` |
| `append_bytes()` | `nc/mod.rs` |
| `bytes_eq()` | `nc/mod.rs` |
| `format_line()` / `copy_bytes()` | `sysinfo.rs` |
| `print_kv()` | `shell/builtins/mod.rs` |

---

## 3. What Stays as Raw Syscalls

These are SlopOS-specific kernel interfaces with **no POSIX or `std` equivalent**. They remain in `userland::syscall::` after migration:

### 3.1 Window/Surface Management (`syscall::window`)
- `fb_info()`, `fb_flip()`, `fb_flip_damage()`
- `surface_attach()`, `surface_commit()`, `surface_frame()`, `surface_damage()`
- `surface_set_role()`, `surface_set_parent()`, `surface_set_relative_position()`, `surface_set_title()`
- `enumerate_windows()`, `set_window_position()`, `set_window_state()`, `raise_window()`
- `poll_frame_done()`, `mark_frames_done()`, `buffer_age()`
- `set_cursor_shape()`

### 3.2 Input Events (`syscall::input`)
- `poll()`, `poll_batch()`, `has_events()`, `drain_queue()`
- `set_focus()`, `set_keyboard_focus()`, `set_pointer_focus()`, `set_pointer_focus_with_offset()`
- `request_close()`
- `get_pointer_pos()`, `get_button_state()`
- `clipboard_copy()`, `clipboard_paste()`

### 3.3 Shared Memory (`syscall::memory` — SHM subset)
- `shm_create()`, `shm_create_with_format()`
- `shm_map()`, `shm_unmap()`
- `shm_destroy()`, `shm_acquire()`, `shm_release()`, `shm_poll_released()`
- `shm_get_formats()`

### 3.4 Roulette / Wheel of Fate (`syscall::roulette`)
- `spin()`, `result()`, `draw()`

### 3.5 SlopOS-Specific Process Operations
- `spawn_path_with_attrs()` — priority + flags not expressible via `std::process::Command`
- `spawn_path_with_argv()` — internal registry-based spawn
- `terminate_task()` — force-kill by task ID
- `halt()` — power off machine
- `reboot()` — restart machine

### 3.6 SlopOS-Specific System Queries
- `get_cpu_count()`, `get_current_cpu()`
- `sys_info()` — kernel memory/scheduler stats
- `random_next()` — kernel entropy source
- `yield_now()` — scheduler yield

### 3.7 TTY Focus (`syscall::tty`)
- `set_focus()` — compositor-specific TTY routing

---

## 4. What Gets Deleted After Migration

| Redundant Module | Replaced By | Phase |
|---|---|---|
| `syscall::fs` (all 298 lines) | `std::fs::File`, `std::fs::OpenOptions`, `std::fs::metadata`, `std::fs::read_dir`, `std::fs::create_dir`, `std::fs::remove_file`, `std::fs::rename` | 3, 6 |
| `syscall::tty::read` / `tty::write` | `std::io::stdin()` / `std::io::stdout()` | 1, 6 |
| `syscall::net` (all 241 lines) | `std::net::TcpStream`, `TcpListener`, `UdpSocket` | 5, 6 |
| `syscall::core::exit` / `exit_with_code` | `std::process::exit()` | 0, 4 |
| `syscall::core::sleep_ms` | `std::thread::sleep(Duration::from_millis())` | 4 |
| `syscall::core::get_time_ms` / `clock_gettime` | `std::time::Instant::now()` | 4 |
| `syscall::process::getpid` | `std::process::id()` | 4 |
| `syscall::process::chdir` / `getcwd` | `std::env::set_current_dir()` / `current_dir()` | 4 |
| `syscall::process::fork` / `execve` / `waitpid` | `std::process::Command` (where possible) | 4 |
| `syscall::process::setsid` / `setpgid` / `getpgid` / `kill` / `ignore_signal` | Keep for shell job control only | 4 |
| `syscall::error` (SyscallError / SyscallResult) | `std::io::Error` / `io::Result` | 1, 6 |
| `syscall::wrappers::fd` (FdGuard) | `std::fs::File` (RAII by default) | 3, 6 |
| `runtime.rs` (`u_strlen`, `u_memcpy`, etc.) | Standard `str`/`String` methods | 2, 6 |
| `entry!()` macro | Standard `fn main()` | 0, 6 |
| All `write_u8_dec` / `write_u16_dec` / `write_u32_dec` / `write_hex_byte` / `write_ipv4` / `write_out` / `append_bytes` / `bytes_eq` duplicates | `format!()` / `write!()` / `println!()` | 2 |
| `slopos-lib::numfmt` dependency (in sysinfo) | `format!()` | 2 |

---

## 5. Phase 0: Foundation — Entry Points & Feature Gates

> **Goal**: Make `std` available and switch all binaries to `fn main()`.
> **Risk**: Low — mechanical changes, no logic changes.
> **Files touched**: 12

### Tasks

- [ ] **0A**: `lib.rs` — Remove `#![feature(restricted_std)]`, remove `#![allow(unsafe_op_in_unsafe_fn)]`
- [ ] **0B**: `bin/shell.rs` — Replace `#![feature(restricted_std)]` + `#![no_main]` + `entry!()` with `fn main()`
- [ ] **0C**: `bin/init.rs` — Same transformation as 0B
- [ ] **0D**: `bin/sysinfo.rs` — Same transformation as 0B
- [ ] **0E**: `bin/roulette.rs` — Same transformation as 0B
- [ ] **0F**: `bin/compositor.rs` — Same transformation as 0B
- [ ] **0G**: `bin/file_manager.rs` — Same transformation as 0B
- [ ] **0H**: `bin/nmap.rs` — Same transformation as 0B
- [ ] **0I**: `bin/ifconfig.rs` — Same transformation as 0B
- [ ] **0J**: `bin/nc.rs` — Replace naked `_start` + raw argc/argv with `fn main()` using `std::env::args()`
- [ ] **0K**: `bin/tests/fork_test.rs` — Same transformation as 0B
- [ ] **0L**: Update all app function signatures — `fn xxx_main(_arg: *mut c_void)` → `fn xxx_main()` (drop unused raw pointer parameter)
- [ ] **0M**: Update `appkit/mod.rs` — Keep `entry!()` macro temporarily as deprecated, add new `fn main()` compatible run path

### Transformation Template

Every binary follows this exact pattern:

```rust
// BEFORE (current):
#![feature(restricted_std)]
#![no_main]
slopos_userland::entry!(slopos_userland::apps::some_app::some_main);

// AFTER (Phase 0):
fn main() {
    slopos_userland::apps::some_app::some_main();
}
```

Special case — `nc.rs`:

```rust
// BEFORE (current):
#![feature(restricted_std)]
#![no_main]

#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "mov rdi, [rsp]",
        "lea rsi, [rsp + 8]",
        "and rsp, -16",
        "call {entry}",
        "ud2",
        entry = sym nc_entry,
    );
}

extern "C" fn nc_entry(argc: usize, argv: *const *const u8) -> ! {
    slopos_userland::apps::nc::nc_main_args(argc, argv);
    slopos_userland::syscall::core::exit();
}

// AFTER (Phase 0):
fn main() {
    let args: Vec<String> = std::env::args().collect();
    slopos_userland::apps::nc::nc_main(args);
}
```

### Verification Gate

```bash
just build    # must compile
just test     # QEMU boot + harness pass
just boot-log # serial output unchanged
```

---

## 6. Phase 1: I/O Layer — `println!` and `std::io`

> **Goal**: Replace all `tty::write(b"...")` and `fs::write_slice(1, ...)` with `std::io` / `print!` / `println!`.
> **Risk**: Low — output behavior identical, just different plumbing.
> **Impact**: Highest bang-for-buck — touches nearly every file, removes the most pervasive `no_std` pattern.

### Tasks

- [ ] **1A**: `apps/init_process.rs` — Replace `tty::write(b"init: ...")` with `eprintln!("init: ...")`
- [ ] **1B**: `apps/roulette.rs` — Replace `tty::write(&MSG_START)` etc. with `println!()`. Remove static byte-array messages.
- [ ] **1C**: `apps/ifconfig.rs` — Replace `write_out(buf)` with `print!()` / `println!()` (full formatting rewrite in Phase 2)
- [ ] **1D**: `apps/nmap.rs` — Same as 1C
- [ ] **1E**: `apps/nc/mod.rs` — Replace `write_out(buf)` with `print!()` (formatting cleanup in Phase 2)
- [ ] **1F**: `apps/nc/tcp.rs` — Replace raw fd writes with `std::io::Write`
- [ ] **1G**: `apps/nc/udp.rs` — Replace raw fd writes with `std::io::Write`
- [ ] **1H**: `apps/shell/banner.rs` — Replace `tty::write` with `print!()`
- [ ] **1I**: `apps/shell/display.rs` — Replace `shell_write` internals to use `std::io::stdout()` where possible. Note: the shell surface rendering path stays raw (it writes to a SHM framebuffer, not stdout).
- [ ] **1J**: `apps/shell/builtins/system.rs` — Replace `tty::write` / `shell_write` with `write!()` / `println!()`
- [ ] **1K**: `apps/shell/builtins/fs.rs` — Replace output patterns with `write!()` / `println!()`
- [ ] **1L**: `apps/shell/builtins/process.rs` — Same
- [ ] **1M**: `apps/shell/builtins/env.rs` — Same
- [ ] **1N**: `apps/shell/builtins/utils.rs` — Same
- [ ] **1O**: `bin/tests/fork_test.rs` — Replace `tty::write(b"fork_test: ...")` with `println!()`

### Key Insight: Shell Output Duality

The shell has two output paths:
1. **Serial/TTY** — `tty::write()` for raw kernel console (used for logging, debug)
2. **Surface** — `shell_console_write_colored()` for the graphical shell framebuffer

Phase 1 replaces path (1) with `println!()` / `stdout()`. Path (2) stays as-is because it renders to a pixel framebuffer via SHM, not a file descriptor.

### Verification Gate

```bash
just build && just test && just boot-log
# Compare test_output.log before/after — serial output should be identical
```

---

## 7. Phase 2: String & Number Formatting — Kill Manual Formatters

> **Goal**: Delete all manual number-to-string conversion functions. Replace with `format!()` / `write!()`.
> **Risk**: Low — pure output formatting, no behavioral change.
> **LOC deleted**: ~250 lines of duplicated formatter code

### Tasks

- [ ] **2A**: `apps/ifconfig.rs` — Full rewrite. Replace `write_u8_dec` / `write_u16_dec` / `write_hex_byte` / `write_ipv4` / manual buffer assembly (~100 lines) with `format!()` / `println!()` (~25 lines)
- [ ] **2B**: `apps/nmap.rs` — Same transformation as 2A. Replace `write_u8_dec` / `write_hex_byte` / `print_member` manual formatting with `format!()`
- [ ] **2C**: `apps/nc/mod.rs` — Delete `write_u8_dec`, `write_u16_dec`, `write_u32_dec`, `write_hex_byte`, `write_ipv4`, `append_bytes`, `bytes_eq`. Replace `verbose_msg`, `verbose_addr`, `verbose_bytes`, `verbose_recv` with `eprintln!`-based implementations. Replace `parse_port` / `parse_ipv4` with `str::parse::<u16>()` / `Ipv4Addr::from_str()`
- [ ] **2D**: `apps/sysinfo.rs` — Delete `format_line()`, `copy_bytes()`, `pages_to_mib()` helper. Replace with `format!("{} MiB", pages * PAGE_SIZE / (1024 * 1024))`. Remove `slopos_lib::numfmt` dependency.
- [ ] **2E**: `apps/shell/builtins/mod.rs` — Delete `print_kv()` manual number formatting. Replace with `write!()`
- [ ] **2F**: `apps/shell/jobs.rs` — Replace `write_u64()` manual formatter with `write!()` / `format!()`
- [ ] **2G**: `apps/shell/mod.rs` — Replace byte-literal statics (`static NL: &[u8] = b"\n"`, `static UNKNOWN_CMD: &[u8] = ...`) with `&str` constants
- [ ] **2H**: `apps/shell/builtins/mod.rs` — Convert `BuiltinEntry` fields from `&'static [u8]` to `&'static str`. Update `BuiltinCategory::label()` return type.
- [ ] **2I**: `program_registry.rs` — Convert `ProgramSpec` path/name fields from `&[u8]` to `&str`

### Example Transformation: `ifconfig.rs`

```rust
// BEFORE (~100 lines of manual formatting):
fn write_u8_dec(mut value: u8, out: &mut [u8], idx: &mut usize) { ... }
fn write_u16_dec(mut value: u16, out: &mut [u8], idx: &mut usize) { ... }
fn write_hex_byte(value: u8, out: &mut [u8], idx: &mut usize) { ... }
fn write_ipv4(ip: [u8; 4], out: &mut [u8], idx: &mut usize) { ... }

pub fn ifconfig_main(_arg: *mut c_void) -> ! {
    let mut line = [0u8; 196];
    let mut i = 0usize;
    line[i..i + 8].copy_from_slice(b"virtio0:");
    i += 8;
    // ... 60 more lines of byte shuffling ...
    write_out(&line[..i]);
    crate::syscall::core::exit();
}

// AFTER (~25 lines):
pub fn ifconfig_main() {
    let mut info = UserNetInfo::default();
    if net_info(&mut info) != 0 {
        eprintln!("ifconfig: net_info syscall failed");
        std::process::exit(1);
    }
    if info.nic_ready == 0 {
        eprintln!("ifconfig: no network interface");
        std::process::exit(1);
    }

    let status = if info.link_up != 0 { "UP" } else { "DOWN" };
    let ip = |a: [u8; 4]| format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
    let mac = info.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");

    println!("virtio0: flags=<{status}>  mtu {}", info.mtu);
    println!("           inet {}  netmask {}  gateway {}", ip(info.ipv4), ip(info.subnet_mask), ip(info.gateway));
    println!("           ether {mac}");
    println!("           dns {}", ip(info.dns));
}
```

### Verification Gate

```bash
just build && just test && just boot-log
# Run ifconfig, nmap, sysinfo in QEMU — verify output matches previous format
```

---

## 8. Phase 3: File Operations — `std::fs`

> **Goal**: Replace raw `syscall::fs` calls with `std::fs` / `std::io` types.
> **Risk**: Medium — behavioral changes in error handling (panics vs. silent failures).
> **Files**: `shell/builtins/fs.rs`, `fork_test.rs`, `file_manager.rs`

### Tasks

- [ ] **3A**: `apps/shell/builtins/fs.rs` — `cmd_ls`: Replace `fs::list_dir()` + `UserFsList` with `std::fs::read_dir()`. Replace manual entry formatting with `format!()`.
- [ ] **3B**: `apps/shell/builtins/fs.rs` — `cmd_cat`: Replace `fs::open_path()` + `fs::read_slice()` + `fs::close_fd()` with `std::fs::read_to_string()` or `File::open()` + `Read::read_to_string()`.
- [ ] **3C**: `apps/shell/builtins/fs.rs` — `cmd_write`: Replace raw open+write+close with `std::fs::write()`.
- [ ] **3D**: `apps/shell/builtins/fs.rs` — `cmd_mkdir`: Replace `fs::mkdir_path()` with `std::fs::create_dir()`.
- [ ] **3E**: `apps/shell/builtins/fs.rs` — `cmd_rm`: Replace `fs::unlink_path()` with `std::fs::remove_file()`.
- [ ] **3F**: `apps/shell/builtins/fs.rs` — `cmd_stat`: Replace `fs::stat_path()` with `std::fs::metadata()`.
- [ ] **3G**: `apps/shell/builtins/fs.rs` — `cmd_cd`: Replace `process::chdir()` with `std::env::set_current_dir()`.
- [ ] **3H**: `apps/shell/builtins/fs.rs` — `cmd_pwd`: Replace `process::getcwd()` with `std::env::current_dir()`.
- [ ] **3I**: `apps/shell/builtins/fs.rs` — `cmd_cp`: Replace raw open+read+write+close with `std::fs::copy()`.
- [ ] **3J**: `apps/shell/builtins/fs.rs` — `cmd_mv`: Replace raw copy+unlink with `std::fs::rename()`.
- [ ] **3K**: `apps/shell/builtins/fs.rs` — `cmd_head` / `cmd_tail`: Replace raw fd reads with `BufReader`.
- [ ] **3L**: `apps/shell/builtins/fs.rs` — `cmd_wc`: Replace raw fd reads with `BufReader::lines()`.
- [ ] **3M**: `apps/shell/builtins/fs.rs` — `cmd_hexdump`: Replace raw fd reads with `File::open()` + `Read::read()`.
- [ ] **3N**: `apps/shell/builtins/fs.rs` — `cmd_diff`: Replace raw fd reads with `std::fs::read_to_string()`.
- [ ] **3O**: `apps/shell/builtins/fs.rs` — `cmd_tee`: Replace raw fd reads/writes with `std::io::stdin()` / `File::create()`.
- [ ] **3P**: `apps/shell/builtins/fs.rs` — `cmd_touch`: Replace raw open+close with `OpenOptions::new().create(true).write(true).open()`.
- [ ] **3Q**: `apps/file_manager.rs` — Replace `fs::list_dir()` + `UserFsList` + `UserFsEntry` with `std::fs::read_dir()`. Replace `[u8; 128]` path with `PathBuf`.
- [ ] **3R**: `bin/tests/fork_test.rs` — Replace `fs::open_path()` / `fs::read_slice()` / `fs::close_fd()` file verification with `std::fs::read_to_string()`.

### Important Caveat: Shell exec.rs Pipeline Plumbing

`shell/exec.rs` uses `fs::pipe()`, `fs::dup2()`, `fs::poll()`, `fs::read_slice()` extensively for pipeline I/O. These are **low-level POSIX primitives** that `std::fs` does not expose. These calls stay as raw syscalls until SlopOS has `std::os::unix::io` extensions or an equivalent. **Do not migrate exec.rs file descriptor operations in this phase.**

### Verification Gate

```bash
just build && just test && just boot-log
# In QEMU shell: ls, cat, write, mkdir, rm, stat, cd, pwd, cp, mv, head, tail, wc, hexdump, diff, touch
# Verify each command produces identical output
```

---

## 9. Phase 4: Process Management — `std::process` and `std::env`

> **Goal**: Replace raw process syscalls with `std` equivalents where possible.
> **Risk**: Medium — process lifecycle is the most sensitive area.
> **Caveat**: Shell job control (fork/exec/setpgid/tcsetpgrp) stays raw.

### Tasks

- [ ] **4A**: Global — Replace all `syscall::core::exit()` / `exit_with_code(n)` with `std::process::exit(n)`
- [ ] **4B**: Global — Replace all `sys_core::sleep_ms(n)` with `std::thread::sleep(Duration::from_millis(n))`
- [ ] **4C**: Global — Replace `sys_core::get_time_ms()` with `std::time::Instant::now()` and `.elapsed()` where used for timing
- [ ] **4D**: Global — Replace `process::getpid()` with `std::process::id()`
- [ ] **4E**: `apps/shell/builtins/fs.rs` — `cmd_cd`: Replace `process::chdir()` with `std::env::set_current_dir()` (if not done in Phase 3)
- [ ] **4F**: `apps/shell/builtins/fs.rs` — `cmd_pwd`: Replace `process::getcwd()` with `std::env::current_dir()` (if not done in Phase 3)
- [ ] **4G**: `apps/shell/mod.rs` — Replace `cwd_bytes()` / `cwd_set()` byte-array CWD tracking with `std::env::current_dir()` / `std::env::set_current_dir()`. Delete `CWD` static.
- [ ] **4H**: `apps/shell/exec.rs` — For **simple single-command spawns** (non-pipeline, non-background), evaluate replacing `fork()` + `execve()` with `std::process::Command::new(path).status()`. Keep raw fork/exec for pipelines.
- [ ] **4I**: `apps/shell/mod.rs` — Replace `SyncUnsafeCell` statics (`LAST_EXIT_CODE`, `LAST_BG_PID`, `SHELL_PID`) with `std::sync::atomic::AtomicI32` / `AtomicU32` or `std::cell::Cell` (single-threaded shell).
- [ ] **4J**: `apps/nc/mod.rs` — Replace `nc_main_args(argc, argv)` raw pointer parsing with `std::env::args()`-based parsing. Delete `parse_args()` raw pointer function, keep `parse_args_from_slices()` as the core.

### What Stays Raw (Shell Job Control)

The following in `shell/exec.rs` **cannot** use `std::process` because they require POSIX job control primitives:

- `process::fork()` — pipeline child creation
- `process::execve()` — in-child program replacement
- `process::setpgid()` / `process::getpgid()` — process group management
- `fs::tcsetpgrp()` / `fs::tcgetpgrp()` — foreground group control
- `process::setsid()` — session leader
- `process::waitpid()` / `waitpid_nohang()` — child reaping with NOHANG
- `process::ignore_signal()` — SIGINT/SIGTTOU/SIGTTIN suppression
- `fs::pipe()` / `fs::dup2()` — pipeline plumbing
- `fs::poll()` — non-blocking pipe drain

These stay as raw syscalls. They are fundamentally shell-specific and operate below `std::process::Command`'s abstraction level.

### Verification Gate

```bash
just build && just test && just boot-log
# In QEMU: run shell commands, background jobs, pipelines, nc, Ctrl+C
# Verify process lifecycle unchanged
```

---

## 10. Phase 5: App-by-App Modernization

> **Goal**: Full idiomatic rewrite of each application, applying all prior phases.
> **Risk**: Varies per app — simple apps are trivial, shell is complex.
> **Order**: Simplest → most complex

### 5.1 Tier 1: Trivial Apps (1–2 hours each)

- [ ] **5A**: `apps/sysinfo.rs` — Replace `format_line()` with `format!()`. Replace `core::ffi::c_void` parameter. Keep appkit windowed pattern. ~20 lines changed.
- [ ] **5B**: `apps/ifconfig.rs` — Full rewrite from ~145 lines to ~40 lines. Replace all manual formatting. Use `std::process::exit()`. (Builds on Phase 2A)
- [ ] **5C**: `apps/nmap.rs` — Same pattern as ifconfig. ~146 lines → ~50 lines. (Builds on Phase 2B)
- [ ] **5D**: `apps/roulette.rs` — Replace `tty::write` with `println!`. Replace `sys_core::sleep_ms` with `thread::sleep`. Keep roulette syscalls. ~10 lines changed.
- [ ] **5E**: `apps/init_process.rs` — Replace `tty::write` with `eprintln!`. Keep `spawn_path_with_attrs` (SlopOS-specific). Replace `sys_core::yield_now` loop with `thread::yield_now`. ~5 lines changed.

### 5.2 Tier 2: Medium Apps (2–4 hours each)

- [ ] **5F**: `apps/file_manager.rs` — Replace `[u8; 128]` path with `PathBuf`. Replace `fs::list_dir()` with `std::fs::read_dir()`. Replace `navigate()` byte manipulation with `PathBuf::push()` / `PathBuf::pop()`. Replace `core::str::from_utf8` fallbacks with native `&str`. Keep appkit windowed pattern.
- [ ] **5G**: `bin/tests/fork_test.rs` — Replace all raw syscall file operations with `std::fs`. Replace `tty::write` with `println!`. Keep shell execution path testing.

### 5.3 Tier 3: Major Apps (4–8 hours each)

- [ ] **5H**: `apps/nc` (full crate) — Major cleanup:
  - Delete ~200 lines of manual formatters (Phase 2C)
  - Replace `nc_main_args(argc, argv)` with `std::env::args()` entry (Phase 4J)
  - Replace raw socket syscalls in `tcp.rs` with `std::net::TcpStream` / `TcpListener`
  - Replace raw socket syscalls in `udp.rs` with `std::net::UdpSocket`
  - Replace `fs::tcgetattr` / `fs::tcsetattr` raw termios — keep as raw (no std equivalent)
  - Replace `process::ignore_signal` — keep as raw (no std equivalent)
  - Replace `verbose_msg` / `verbose_addr` / `verbose_bytes` / `verbose_recv` with `eprintln!`
  - Replace `parse_port` / `parse_ipv4` with `str::parse()` / `std::net::Ipv4Addr`
  - Total: ~915 lines → ~400 lines

- [ ] **5I**: `apps/shell` (full module) — Largest migration. Per-submodule:
  - `mod.rs`: Replace `SyncUnsafeCell` statics, byte constants → `&str`, `build_prompt` → `format!`
  - `builtins/mod.rs`: `BuiltinEntry` fields `&[u8]` → `&str`, delete `print_kv()`
  - `builtins/system.rs`: All output via `write!()` / `format!()`
  - `builtins/fs.rs`: All file ops via `std::fs` (Phase 3 tasks)
  - `builtins/process.rs`: Output formatting via `write!()`
  - `builtins/env.rs`: Output formatting via `write!()`
  - `builtins/utils.rs`: Output formatting via `write!()`
  - `display.rs`: Replace byte-level TTY writes with `std::io::stdout().write_all()`
  - `env.rs`: Evaluate backing with `std::env` (currently custom `[u8; N]` KV store)
  - `jobs.rs`: Replace manual number formatting with `write!()`
  - `parser.rs`: Replace `u_strlen` / `u_streq_slice` with `str` / `CStr` methods
  - `input.rs`: Replace raw TTY reads with `std::io::stdin().read()`
  - `banner.rs`: Replace `tty::write` with `print!()`
  - `buffers.rs`: Replace fixed-size byte buffers with `Vec<u8>` / `String`
  - `exec.rs`: Partial — keep fork/pipe/dup2 pipeline plumbing, modernize everything else
  - `completion.rs`: Replace byte-level path matching with `std::fs::read_dir()`
  - `history.rs`: Replace byte buffers with `Vec<String>`
  - `surface.rs`: No change (SHM framebuffer rendering)

### Verification Gate

Each app verified individually:
```bash
just build && just test
# Per-app: boot QEMU, exercise the specific app, check output
```

---

## 11. Phase 6: Cleanup & Deletion

> **Goal**: Remove all dead code left over from migration.
> **Risk**: Low — deletion only, no new behavior.

### Tasks

- [ ] **6A**: Delete `entry!()` macro from `appkit/mod.rs` (lines 41–91)
- [ ] **6B**: Delete `runtime.rs` entirely (3 lines — `u_strlen`, `u_memcpy`, etc. re-exports)
- [ ] **6C**: Delete `syscall::fs` module entirely (298 lines)
- [ ] **6D**: Delete `syscall::tty::read` and `syscall::tty::write` (keep `tty::set_focus` only)
- [ ] **6E**: Delete `syscall::net` module entirely (241 lines)
- [ ] **6F**: Delete `syscall::error` module (1 line re-export)
- [ ] **6G**: Delete `syscall::wrappers::fd` module (49 lines — `FdGuard`)
- [ ] **6H**: Gut `syscall::core` — delete `exit`, `exit_with_code`, `sleep_ms`, `get_time_ms`, `clock_gettime`, `clock_gettime_ns`. Keep `yield_now`, `get_cpu_count`, `get_current_cpu`, `random_next`, `sys_info`.
- [ ] **6I**: Gut `syscall::process` — delete `getpid`, `getuid`, `chdir`, `getcwd`, `fork`, `exec`, `exec_ptr`, `execve`, `waitpid`, `waitpid_nohang`, `setsid`, `setpgid`, `getpgid`, `kill`, `kill_pid`, `ignore_signal`. Keep `spawn_path`, `spawn_path_with_attrs`, `spawn_path_with_argv`, `terminate_task`, `halt`, `reboot`.
- [ ] **6J**: Gut `syscall::memory` — delete `brk`, `sbrk`. Keep all SHM operations.
- [ ] **6K**: Update `syscall::mod.rs` — Remove deleted module declarations, clean up re-exports
- [ ] **6L**: Delete `syscall::wrappers/mod.rs` fd re-export, keep only SHM
- [ ] **6M**: Evaluate `slopos-lib` dependency — if `numfmt` was the only usage, consider removing
- [ ] **6N**: Clean up `Cargo.toml` — remove any dependencies that are no longer needed
- [ ] **6O**: Final audit — `grep` for any remaining `tty::write`, `fs::open_path`, `core::ffi::c_void` parameters, `#![no_main]`, `entry!()` usage

### Verification Gate

```bash
just build && just test && just boot-log
# Full regression: all apps, all shell builtins, pipelines, background jobs, nc, roulette
```

---

## 12. Dependency Graph

```
Phase 0 (Foundation)
  │
  ├──→ Phase 1 (I/O Layer)
  │      │
  │      ├──→ Phase 2 (String & Formatting)
  │      │      │
  │      │      └──→ Phase 5 Tier 1 (Trivial Apps)
  │      │
  │      └──→ Phase 3 (File Operations)
  │             │
  │             └──→ Phase 5 Tier 2 (Medium Apps)
  │
  └──→ Phase 4 (Process Management)
         │
         └──→ Phase 5 Tier 3 (Major Apps: nc, shell)
                │
                └──→ Phase 6 (Cleanup & Deletion)
```

**Key constraints:**
- Phase 0 must be first — nothing works without `fn main()` and `std`
- Phase 1 before Phase 2 — formatting changes need `println!()` available
- Phase 3 before Phase 5 Tier 2 — file_manager needs `std::fs`
- Phase 4 before Phase 5 Tier 3 — shell/nc need `std::process`, `std::env`
- Phase 6 must be last — can only delete after all consumers are migrated

**Parallelism opportunities:**
- Phase 1 + Phase 4 can run in parallel (independent concerns)
- Phase 5 Tier 1 apps can all be done in parallel
- Phase 5 shell submodules can be done in parallel (mostly independent)

---

## 13. Verification Strategy

### Per-Phase Verification

After **every phase**:

1. **`just build`** — Must compile clean with zero warnings
2. **`just test`** — QEMU boot + test harness pass
3. **`just boot-log`** — Non-interactive boot, capture `test_output.log`
4. **LSP diagnostics** — Clean on all changed files

### Per-App Verification (Phase 5)

| App | Verification Steps |
|---|---|
| `sysinfo` | Boot → launch sysinfo → verify CPU/memory stats render correctly |
| `ifconfig` | Boot → run `ifconfig` in shell → verify output format matches |
| `nmap` | Boot → run `nmap` → verify host discovery output |
| `roulette` | Boot → verify wheel spins, fate number displays |
| `init` | Boot → verify compositor + shell spawn, no panic |
| `file_manager` | Boot → launch file manager → navigate directories |
| `nc` | Boot → `nc -l 8080` and `nc host 8080` → verify TCP/UDP I/O |
| `shell` | Boot → exercise every builtin (`help`, `ls`, `cat`, `cd`, `pwd`, etc.) → verify pipelines (`ls | wc`), background jobs, redirections (`echo hi > /tmp/test`) |
| `fork_test` | `just test` — included in test harness |

### Regression Checklist

- [ ] Roulette wheel animation renders
- [ ] Shell prompt displays correctly
- [ ] Shell builtins all functional (run `help` to list, exercise each)
- [ ] Pipelines work: `echo hello | wc`
- [ ] Redirections work: `echo test > /tmp/x && cat /tmp/x`
- [ ] Background jobs work: `sleep 5000 &` then `jobs`
- [ ] File manager navigates directories
- [ ] Sysinfo shows memory/CPU stats
- [ ] nc TCP client/server works
- [ ] nc UDP client/server works
- [ ] `ifconfig` displays network info
- [ ] `nmap` discovers hosts
- [ ] Ctrl+C interrupts foreground process
- [ ] `halt` and `reboot` still work

---

## 14. Progress Tracking

### Phase Summary

| Phase | Tasks | Status | Description |
|---|---|---|---|
| **0** | 13 | Not Started | Foundation — entry points, feature gates |
| **1** | 15 | Not Started | I/O layer — println, std::io |
| **2** | 9 | Not Started | String & number formatting |
| **3** | 18 | Not Started | File operations — std::fs |
| **4** | 10 | Not Started | Process management — std::process, std::env |
| **5** | 9 | Not Started | App-by-app modernization |
| **6** | 15 | Not Started | Cleanup & deletion |
| **Total** | **89** | | |

### LOC Impact Estimate

| Category | Lines Deleted | Lines Added | Net |
|---|---|---|---|
| Manual formatters | ~250 | ~50 | -200 |
| Entry point boilerplate | ~120 | ~30 | -90 |
| Raw syscall wrappers (deleted) | ~700 | 0 | -700 |
| FdGuard / error types | ~100 | 0 | -100 |
| runtime.rs / entry macro | ~95 | 0 | -95 |
| Modernized app code | ~200 | ~300 | +100 |
| **Total** | **~1465** | **~380** | **~-1085** |
