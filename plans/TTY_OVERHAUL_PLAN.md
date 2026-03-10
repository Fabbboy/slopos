# SlopOS TTY Overhaul Plan

> **Status**: ✅ Phases 1–28 complete | 📋 Phases 29–42 planned (post-overhaul hardening & POSIX gold-standard completion)
> **Target**: Replace the global singleton TTY with a proper per-terminal TTY subsystem comparable to Linux N_TTY / RedoxOS
> **Current**: `drivers/src/tty/` module directory — clean per-TTY API, PTY support with generation-safe peer handles, per-slot locking, atomic PTY pair lifecycle, compositor focus split from POSIX foreground, real output drain semantics, scalable 32-slot capacity, type-safe termios foundation (`bitflags!` flag types, `CcIndex` enum, refined `TtyError`); Phases 29–42 target dispatch consolidation, `/dev/tty` magic device, SIGTTOU enforcement, UTF-8 editing, PTY namespace, VT100 emulation, and full POSIX termios parity
> **Bugs Addressed**: Double-typing on PS/2 keyboard, nc immediate termination, dual input delivery, blocked-reader wakeup regression (PS/2/TTY reads)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Bug Analysis](#3-bug-analysis)
4. [Target Architecture](#4-target-architecture)
5. [Phase 1: TTY Core Abstraction](#5-phase-1-tty-core-abstraction)
6. [Phase 2: Enhanced Line Discipline](#6-phase-2-enhanced-line-discipline)
7. [Phase 3: Input Pipeline Cleanup](#7-phase-3-input-pipeline-cleanup)
8. [Phase 4: Session & Process Group Management](#8-phase-4-session--process-group-management)
9. [Phase 5: FD Integration](#9-phase-5-fd-integration)
10. [Phase 6: Control-Plane Correctness](#10-phase-6-control-plane-correctness)
11. [Phase 7: Lifecycle & Hangup Semantics](#11-phase-7-lifecycle--hangup-semantics)
12. [Phase 8: Per-TTY Locking & Performance](#12-phase-8-per-tty-locking--performance)
13. [Phase 9: Rust Idioms & Termios Completion](#13-phase-9-rust-idioms--termios-completion)
14. [Phase 10: Job Control Correctness](#14-phase-10-job-control-correctness)
15. [Phase 11: Non-Canonical Timing Fix](#15-phase-11-non-canonical-timing-fix)
16. [Phase 12: Sane Defaults & Output Column Tracking ✅](#16-phase-12-sane-defaults--output-column-tracking)
17. [Phase 13: ABI Signal Constant Unification ✅](#17-phase-13-abi-signal-constant-unification)
18. [Phase 14: Responsibility Split — PTY Foundation ✅](#18-phase-14-responsibility-split--pty-foundation)
19. [Phase 15: POSIX Quick Wins — Line Boundaries, SIGWINCH, SIGHUP, Word Erase ✅](#19-phase-15-posix-quick-wins--line-boundaries-sigwinch-sighup-word-erase)
20. [Phase 16: Termios Drain Semantics & Open Flags ✅](#20-phase-16-termios-drain-semantics--open-flags)
21. [Phase 17: PTY Implementation ✅](#21-phase-17-pty-implementation)
22. [Phase 18: Controlling Terminal Ownership Unification ✅](#22-phase-18-controlling-terminal-ownership-unification)
23. [Phase 19: Strict Session Gates & Foreground Outcomes ✅](#23-phase-19-strict-session-gates--foreground-outcomes)
24. [Phase 20: PTY Pair Atomicity & Lifecycle Hardening ✅](#24-phase-20-pty-pair-atomicity--lifecycle-hardening)
25. [Phase 21: Event-Driven Readiness & IXON Completion ✅](#25-phase-21-event-driven-readiness--ixon-completion)
26. [Phase 22: Operational Console Routing & Real VConsole ✅](#26-phase-22-operational-console-routing--real-vconsole)
27. [Phase 23: Canonical EOF, ISIG Flush & Signal Integrity](#27-phase-23-canonical-eof-isig-flush--signal-integrity)
28. [Phase 24: Job Control & Controlling TTY Hardening ✅](#28-phase-24-job-control--controlling-tty-hardening)
29. [Phase 25: Real TCSETSW/TCSETSF Drain Semantics ✅](#29-phase-25-real-tcsetswtcsetsf-drain-semantics)
30. [Phase 26: PTY Lifetime Safety & Scalable Capacity](#30-phase-26-pty-lifetime-safety--scalable-capacity)
31. [Phase 27: POSIX Completion Set (Rust-Idiomatic)](#31-phase-27-posix-completion-set-rust-idiomatic)
32. [Phase 28: Type-Safe Termios Foundation ✅](#32-phase-28-type-safe-termios-foundation)
33. [Phase 29: LdiscKind Dispatch Consolidation](#33-phase-29-ldisckind-dispatch-consolidation)
34. [Phase 30: `/dev/tty` Controlling Terminal Device](#34-phase-30-devtty-controlling-terminal-device)
35. [Phase 31: Background Write Protection (SIGTTOU on `tcsetattr`)](#35-phase-31-background-write-protection-sigttou-on-tcsetattr)
36. [Phase 32: Controlling Terminal Lifecycle Integrity](#36-phase-32-controlling-terminal-lifecycle-integrity)
37. [Phase 33: Post-Hangup I/O Hardening](#37-phase-33-post-hangup-io-hardening)
38. [Phase 34: Extended Line Boundaries (VEOL, VEOL2)](#38-phase-34-extended-line-boundaries-veol-veol2)
39. [Phase 35: UTF-8 Aware Editing (IUTF8)](#39-phase-35-utf-8-aware-editing-iutf8)
40. [Phase 36: Input Buffer Policy (IMAXBEL, IXOFF, CREAD)](#40-phase-36-input-buffer-policy-imaxbel-ixoff-cread)
41. [Phase 37: Deferred Reprint (PENDIN)](#41-phase-37-deferred-reprint-pendin)
42. [Phase 38: PTY Namespace & Device Nodes](#42-phase-38-pty-namespace--device-nodes)
43. [Phase 39: PTY Packet Mode (TIOCPKT)](#43-phase-39-pty-packet-mode-tiocpkt)
44. [Phase 40: VT100/ANSI Terminal Emulation](#44-phase-40-vt100ansi-terminal-emulation)
45. [Phase 41: Advanced PTY & Session Control (EXTPROC, vhangup)](#45-phase-41-advanced-pty--session-control-extproc-vhangup)
46. [Phase 42: Legacy Termios Completion (ECHOPRT, IUCLC, OLCUC)](#46-phase-42-legacy-termios-completion-echoprt-iuclc-olcuc)
47. [File Inventory](#47-file-inventory)
48. [Appendix: Linux N_TTY Reference](#48-appendix-linux-n_tty-reference)

---

## 1. Executive Summary

The SlopOS TTY subsystem is currently a **global singleton line discipline** behind a single `IrqMutex`, with an ad-hoc focus system using bare atomics. It works for single-terminal use but has architectural issues that cause:

- **Double-typing bug**: Each PS/2 keystroke must be typed twice for one character to register
- **nc immediate termination**: Foreground child processes get killed unexpectedly on Enter
- **No multi-terminal support**: Single global `LINE_DISC`, single `TTY_WAIT_QUEUE`
- **Missing POSIX TTY semantics**: No sessions, no controlling terminal, no job control signals beyond SIGINT

This plan replaces the singleton with a proper **per-terminal TTY subsystem** modeled after Linux's `tty_struct` + `n_tty` line discipline, adapted for SlopOS's `no_std` Rust environment.

### Summary of changes

| Phase | What | Files Modified | New Files | Status |
|-------|------|---------------|-----------|--------|
| 1 | TTY core structs | `drivers/src/tty.rs` (deleted), `drivers/src/line_disc.rs` (deleted), `drivers/src/lib.rs` | `drivers/src/tty/mod.rs`, `tty/driver.rs`, `tty/table.rs`, `tty/ldisc.rs`, `tty/session.rs`, `drivers/src/tty_tests.rs` | **DONE** |
| 1b | Shim removal | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/ps2/keyboard.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `core/src/syscall/core_handlers.rs`, `core/src/syscall/ui_handlers.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `fs/src/fileio.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 2 | Line discipline | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 3 | Input pipeline | `drivers/src/ps2/keyboard.rs`, `drivers/src/input_event.rs` | — | **DONE** |
| 4 | Sessions/pgrps | `drivers/src/tty/session.rs`, `drivers/src/tty/mod.rs`, `abi/src/syscall.rs`, `lib/`, `core/`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 5 | FD integration | `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 6 | Control-plane correctness | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty/ldisc.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `core/src/syscall/ui_handlers.rs`, `core/src/syscall/core_handlers.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 7 | Lifecycle & hangup | `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty/ldisc.rs`, `core/src/scheduler/task.rs`, `core/src/scheduler/task_struct.rs`, `core/src/syscall/process_handlers.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 8 | Per-TTY locking & perf | `drivers/src/tty/table.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 9 | Rust idioms & termios | `drivers/src/tty/mod.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/syscall_services_init.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/driver_runtime.rs`, `core/src/driver_hooks.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 10 | Job control correctness | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 11 | Non-canonical timing fix | `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 12 | Sane defaults & column tracking | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 13 | ABI signal unification | `abi/src/syscall.rs`, `abi/src/signal.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 14 | Responsibility split (PTY prep) | `drivers/src/tty/mod.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty/ldisc.rs` | — | **DONE** |
| 15 | POSIX quick wins (line boundaries, SIGWINCH, SIGHUP, word erase) | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `abi/src/signal.rs`, `core/src/scheduler/task.rs` | — | **DONE** |
| 16 | Termios drain & open flags | `abi/src/syscall.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 17 | PTY implementation | `drivers/src/tty/pty.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty_tests.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `abi/src/syscall.rs` | `drivers/src/tty/pty.rs` | **DONE** |
| 18 | Controlling terminal ownership unification | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/syscall_services_init.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/driver_runtime.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `core/src/driver_hooks.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `core/src/syscall/process_handlers.rs`, `core/src/scheduler/task.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs`, `userland/src/syscall/fs.rs`, `userland/src/apps/shell/exec.rs` | — | **DONE** |
| 19 | Strict session gates & foreground outcomes | `drivers/src/tty/session.rs`, `drivers/src/tty/mod.rs`, `fs/src/fileio.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 20 | PTY pair atomicity & lifecycle hardening | `drivers/src/tty/pty.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty/mod.rs`, `fs/src/fileio.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 21 | Event-driven readiness & IXON completion | `drivers/src/tty/mod.rs`, `drivers/src/tty/ldisc.rs`, `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `lib/src/waitqueue.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 22 | Operational console routing & real VConsole | `drivers/src/tty/vconsole.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/mod.rs`, `fs/src/fileio.rs`, `boot/src/boot_drivers.rs`, `video/src/framebuffer.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | `drivers/src/tty/vconsole.rs` | **DONE** |
| 23 | Canonical EOF, ISIG flush & signal integrity | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 24 | Job control & controlling TTY hardening | `drivers/src/tty/session.rs`, `drivers/src/tty/mod.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `fs/src/fileio.rs`, `core/src/syscall/process_handlers.rs`, `core/src/scheduler/task.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 25 | Real `TCSETSW`/`TCSETSF` drain semantics | `drivers/src/tty/mod.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 26 | PTY lifetime safety & scalable capacity | `drivers/src/tty/pty.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 27 | POSIX completion set (Rust-idiomatic, non-clone) | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `abi/src/syscall.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs` | — | **DONE** |
| 28 | Type-safe termios foundation (`bitflags!`, `CcIndex`, `TtyError` refinement) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 29 | LdiscKind dispatch consolidation (`dispatch_ldisc!` macro, `*_locked()` convention) | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 30 | `/dev/tty` controlling terminal magic device | `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 31 | SIGTTOU on background `tcsetattr`, TOSTOP audit | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 32 | Controlling terminal lifecycle integrity (fork/setsid/O_NOCTTY/TIOCSCTTY chain) | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `fs/src/fileio.rs`, `core/src/syscall/process_handlers.rs`, `core/src/scheduler/task.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 33 | Post-hangup I/O hardening (EOF/EIO/POLLHUP consistency) | `drivers/src/tty/mod.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/pty.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 34 | Extended line boundaries (VEOL, VEOL2) | `drivers/src/tty/ldisc.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 35 | UTF-8 aware editing (IUTF8, multi-byte backspace, char width) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 36 | Input buffer policy (IMAXBEL bell, IXOFF flow control, CREAD gate) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 37 | Deferred reprint (PENDIN flag, VREPRINT integration) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 38 | PTY namespace & device nodes (`/dev/ptmx`, `/dev/pts/N`, lock ioctls) | `drivers/src/tty/pty.rs`, `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 39 | PTY packet mode (TIOCPKT, control byte framing) | `drivers/src/tty/pty.rs`, `drivers/src/tty/mod.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 40 | VT100/ANSI terminal emulation (escape parser, CSI, cursor/color) | `drivers/src/tty/vconsole.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | `drivers/src/tty/vtparser.rs` | **TODO** |
| 41 | Advanced PTY & session control (EXTPROC, `vhangup()`) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `core/src/syscall/process_handlers.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |
| 42 | Legacy termios completion (ECHOPRT, IUCLC, OLCUC) | `abi/src/syscall.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty_tests.rs` | — | **TODO** |

---

## 2. Current State Assessment

### What Exists

| Component | Location | Lines | Description |
|-----------|----------|-------|-------------|
| TTY driver | `drivers/src/tty.rs` | 373 | Global singleton, wait queues, focus system, `tty_read_cooked`, `tty_handle_input_char` |
| Line discipline | `drivers/src/line_disc.rs` | 183 | Basic `LineDisc` struct with edit buffer, cooked ring buffer, ICANON/ECHO/ISIG |
| TTY syscall glue | `lib/src/kernel_services/syscall_services/tty.rs` | ~50 | Thin wrappers calling `drivers::tty::*` |
| Userland TTY syscalls | `userland/src/syscall/tty.rs` | ~30 | `tcgetattr`, `tcsetattr`, `set_foreground_pgrp` |
| ABI types | `abi/src/syscall.rs:567-636` | ~70 | `UserTermios`, `UserWinsize`, TCGETS/TCSETS/TIOCGPGRP/etc, cc indices |
| Keyboard driver | `drivers/src/ps2/keyboard.rs` | 328 | PS/2 scancode handling, calls BOTH `input_route_key_event` AND `tty_handle_input_char` |
| Input event system | `drivers/src/input_event.rs` | 437 | Compositor/Wayland-style per-task input queues |
| Serial driver | `drivers/src/serial.rs` | 260 | Polling-based UART, `INPUT_BUFFER` ring, no IRQ handler |
| FD console routing | `fs/src/fileio.rs:495-543` | ~50 | Console FDs (0,1,2) bootstrap, read/write/poll routing |
| Poll/ioctl handlers | `core/src/syscall/fs/poll_ioctl_handlers.rs` | ~200 | TCGETS/TCSETS/TIOCGPGRP dispatch |

### What's Missing vs Linux/RedoxOS

| Feature | Linux | RedoxOS | SlopOS |
|---------|-------|---------|--------|
| Per-terminal state | `struct tty_struct` | `ptyd` daemon | ❌ Global singleton |
| Driver abstraction | `struct tty_driver` + ops | scheme:// URIs | ❌ Hardcoded serial output |
| Line discipline | `n_tty.c` (2800 lines) | In `ptyd` | ⚠️ Basic (183 lines) |
| Output processing | OPOST, ONLCR, OCRNL, etc. | Yes | ❌ None |
| Input flags | ICRNL, INLCR, IGNCR, ISTRIP, IXON/IXOFF | Yes | ❌ None |
| Echo modes | ECHO, ECHOCTL, ECHOKE, ECHOPRT | Yes | ⚠️ ECHO, ECHOE, ECHOK only |
| VMIN/VTIME | Full non-canonical timing | Yes | ✅ Enforced (all 4 POSIX cases) |
| Controlling terminal | Per-process `/dev/tty` | Per-process scheme | ✅ `/dev/tty` resolves to controlling terminal |
| Sessions | `setsid()`, session leader | Scheme-based | ❌ None |
| Job control signals | SIGTTIN, SIGTTOU, SIGTSTP | Yes | ❌ Only SIGINT |
| PTY | Full master/slave | `ptyd` | ❌ None |
| Multiple terminals | VT switching, unlimited PTYs | Arbitrary schemes | ❌ Single terminal |

---

## 3. Bug Analysis

### 3.1 Double-Typing Bug (PS/2 Keyboard via QEMU Graphical Window)

**Symptoms**: Each character must be typed twice to register once when using the QEMU graphical window (PS/2 keyboard input).

**Root Cause Analysis**: The keyboard interrupt handler in `drivers/src/ps2/keyboard.rs:202-297` has a **dual delivery** architecture:

```
PS/2 Interrupt
    ├── Line 227: input_route_key_event()  →  Input event queue (compositor)
    │                                          Shell drains via poll_batch()
    │                                          but IGNORES keyboard events
    │
    └── Line 294: tty_handle_input_char()  →  LINE_DISC cooked buffer
                                               Shell reads via fd 0
```

While the shell correctly ignores keyboard events from the input queue, the **focus system** (`tty_task_has_focus` at `tty.rs:196-203`) creates a race:

1. `tty_read_cooked` checks `tty_task_has_focus(task_id)` before reading
2. Focus is determined by `TTY_FOCUSED_TASK_ID` (set lazily) and `TTY_FOREGROUND_PGRP` (compares pgrp ID with task ID — semantically wrong)
3. When the shell spawns a child (like nc), focus may not transfer correctly
4. The `tty_wait_for_focus()` call blocks the reader, causing the next poll cycle to miss the character

Additionally, `tty_drain_hw_input()` is called from multiple contexts (poll, read, block_until_ready) which creates redundant drain cycles. Combined with the single global `TTY_WAIT_QUEUE`, this means a `tty_notify_input_ready()` wakeup may reach the wrong blocked task.

**Fix**: Phases 1 + 3 + 4 eliminate all of these issues by:
- Per-TTY wait queues (no cross-terminal wakeup confusion)
- Clean single input path (keyboard → TTY only)
- Proper session/pgrp-based foreground determination

### 3.2 NC Immediate Termination

**Symptoms**: nc connects to host, user types "hello" + Enter, nc immediately dies with `Terminating task 'nc' (ID 9)` — no nc error message, no data sent to host.

**Root Cause Analysis**: Two contributing factors:

1. **Signal delivery**: When Enter is pressed, `process_raw_char` feeds `'\n'` to the line discipline. If the TTY is still in canonical mode (the `tcsetattr` from nc may race with the first keystroke), `'\n'` triggers `flush_edit_to_cooked`. But more critically, the ad-hoc focus system may route the wakeup to the shell's blocked `waitpid` reader instead of nc, causing the shell to think nc has exited.

2. **TCP dst_mac bug** (separate from TTY, noted here): `drivers/src/net/socket.rs:830` hardcodes `let dst_mac = [0xff; 6]` (broadcast MAC) for TCP segments, while the rest of the IP stack uses proper ARP neighbor resolution (`drivers/src/net/ipv4.rs:275`). This means even if nc successfully sends data, the TCP data frames use broadcast destination MAC instead of the resolved next-hop MAC.

**Fix**: Phase 4 (proper session management) ensures child processes correctly inherit the controlling terminal and foreground group, preventing spurious termination. The TCP dst_mac fix is out of scope for this plan but should be addressed separately.

---

## 4. Target Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         USERLAND                                │
│  ┌────────┐  ┌──────┐  ┌────────┐  ┌─────────────────────────┐ │
│  │ Shell  │  │  nc  │  │ editor │  │  Future: PTY clients    │ │
│  └───┬────┘  └──┬───┘  └───┬────┘  └────────────┬────────────┘ │
│      │ read/write/ioctl(fd 0,1,2)                │              │
└──────┼──────────┼───────────┼────────────────────┼──────────────┘
       │  SYSCALL │           │                    │
┌──────▼──────────▼───────────▼────────────────────▼──────────────┐
│  fileio.rs — per-process FD table                                │
│  FileDescriptor.tty_index → indexes into TTY_TABLE               │
│      │                                                           │
│  ┌───▼───────────────────────────────────────────────────────┐   │
│  │  TTY Subsystem  (drivers/src/tty/)                        │   │
│  │                                                           │   │
│  │  ┌──────────────────┐  ┌──────────────────┐               │   │
│  │  │  Tty[0]          │  │  Tty[1]          │  ...          │   │
│  │  │  ┌────────────┐  │  │  ┌────────────┐  │               │   │
│  │  │  │  LineDisc   │  │  │  │  LineDisc   │  │               │   │
│  │  │  │  - termios  │  │  │  │  - termios  │  │               │   │
│  │  │  │  - edit_buf │  │  │  │  - edit_buf │  │               │   │
│  │  │  │  - cooked   │  │  │  │  - cooked   │  │               │   │
│  │  │  └────────────┘  │  │  └────────────┘  │               │   │
│  │  │  session_id      │  │  session_id      │               │   │
│  │  │  fg_pgrp         │  │  fg_pgrp         │               │   │
│  │  │  wait_queue      │  │  wait_queue      │               │   │
│  │  │  ┌────────────┐  │  │  ┌────────────┐  │               │   │
│  │  │  │ Driver:    │  │  │  │ Driver:    │  │               │   │
│  │  │  │ SerialCon  │  │  │  │ VConsole   │  │               │   │
│  │  │  │ (COM1)     │  │  │  │ (PS/2+FB)  │  │               │   │
│  │  │  └────────────┘  │  │  └────────────┘  │               │   │
│  │  └──────────────────┘  └──────────────────┘               │   │
│  │                                                           │   │
│  │  TTY_TABLE: [Option<Tty>; MAX_TTYS]                       │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                  │
│  Input Event System (unchanged — compositor/mouse only)          │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │  input_event.rs — per-task queues for pointer/window      │   │
│  │  (keyboard events NO LONGER routed here)                  │   │
│  └───────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

1. **Per-TTY `LineDisc`**: Each `Tty` owns its own line discipline instance. No global singleton.

2. **`TtyDriver` trait**: Abstracts the hardware backend. Two initial implementations:
   - `SerialConsoleDriver` — wraps COM1 UART (polling-based, as today)
   - `VConsoleDriver` — wraps PS/2 keyboard + framebuffer output

3. **Single input path**: The keyboard interrupt handler feeds the **active TTY** only. `input_route_key_event()` is NO LONGER called for keyboard input — it remains for pointer/window events only.

4. **Per-TTY wait queue**: Each `Tty` has its own `WaitQueue`. `tty_notify_input_ready()` wakes only tasks blocked on that specific TTY.

5. **Session-based foreground**: Foreground process group is stored per-TTY. `tty_read_cooked` checks the calling process's session and pgrp against the TTY's foreground pgrp.

6. **Controlling terminal per-process**: Each process stores a `controlling_tty: Option<TtyIndex>`. Set on first open or inherited from parent via fork.

---

## 5. Phase 1: TTY Core Abstraction ✅ COMPLETED

**Status**: Completed. All 26 TTY regression tests pass. Build clean. `just test` passes (944/944).

**Post-Phase-1 Hotfix (Read/Wakeup Regression)**: Completed. `tty::read()` now blocks on per-TTY wait queues using `WaitQueue::wait_event(...)` instead of raw `block_current_task()`. Input arrival (`push_input`/`notify_input_ready`) now wakes the matching per-TTY wait queue, and `set_focus()` wakes blocked readers so focus handoff no longer relies on idle-loop reschedule side effects.

**Implementation note (lock ordering)**: In SlopOS, per-TTY wait queues are stored in a separate static array (`TTY_INPUT_WAITERS`) indexed by `TtyIndex`, while line discipline/session state remains in `TTY_TABLE`. This matches existing SlopOS socket wait-queue patterns and avoids sleeping while holding the `TTY_TABLE` lock.

**Operational note (until Phase 4/5)**: Read-side focus gating is temporarily relaxed so stdin readers on console FDs are not hard-blocked by compositor focus state while FD routing is still fixed to TTY 0. Proper job-control/session enforcement remains Phase 4/5 work.

**Phase 1b (Shim Removal)**: All backward-compatible shims removed. TtyServices now takes `tty_index: u8` for per-TTY operations. Focus state moved from global atomics into per-TTY `TtySession.focused_task_id`. Compat wait queue deleted. Keyboard driver calls `push_input(active_tty(), c)` directly. `TTY_WINSIZE` global in `poll_ioctl_handlers.rs` replaced with per-TTY `get_winsize`/`set_winsize` via TtyServices.

**Goal**: Replace the global singleton with `Tty` struct, `TtyDriver` trait, and `TTY_TABLE`.

### 5.1 New file structure

Convert `drivers/src/tty.rs` (single file) → `drivers/src/tty/` (module directory):

```
drivers/src/tty/
├── mod.rs           # Public API: tty_read, tty_write, tty_poll, tty_ioctl
├── driver.rs        # TtyDriver trait + SerialConsoleDriver + VConsoleDriver
├── table.rs         # TTY_TABLE: global array of Tty instances
├── ldisc.rs         # Enhanced LineDisc (Phase 2, but file created here)
├── session.rs       # Session/pgrp management (Phase 4, but file created here)
└── wait_queue.rs    # Per-TTY WaitQueue (extracted from current tty.rs)
```

### 5.2 Core types

```rust
// drivers/src/tty/mod.rs

/// Index into the global TTY table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TtyIndex(pub u8);

pub const MAX_TTYS: usize = 8;

/// The central TTY structure — one per terminal.
pub struct Tty {
    /// Which TTY slot this is (0 = serial console, 1 = virtual console, etc.)
    pub index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub ldisc: LineDisc,

    /// Hardware driver backend.
    pub driver: TtyDriverKind,

    /// Session ID that owns this TTY (0 = no session).
    pub session_id: u32,

    /// Foreground process group ID.
    pub fg_pgrp: u32,

    /// Wait queue for tasks blocked on input.
    pub input_waiters: WaitQueue,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub winsize: UserWinsize,

    /// Whether this TTY is active/allocated.
    pub active: bool,
}
```

### 5.3 TtyDriver trait

```rust
// drivers/src/tty/driver.rs

/// Backend driver operations for a TTY.
pub trait TtyDriver {
    /// Write output bytes to the terminal hardware (serial port, framebuffer, etc.)
    fn write_output(&self, buf: &[u8]);

    /// Poll for pending hardware input. Returns bytes available.
    /// Called by tty_drain_hw_input.
    fn drain_input(&self, out: &mut [u8]) -> usize;

    /// Optional: called when termios changes (e.g., baud rate).
    fn set_termios(&self, _termios: &UserTermios) {}
}

/// Enum dispatch to avoid trait objects (no_std, no alloc).
pub enum TtyDriverKind {
    SerialConsole(SerialConsoleDriver),
    VConsole(VConsoleDriver),
    None,
}
```

### 5.4 Migration steps

1. Create `drivers/src/tty/` directory and move `tty.rs` → `tty/mod.rs`
2. Create `driver.rs` with `TtyDriver` trait and two implementations
3. Create `table.rs` with `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>`
4. Create `wait_queue.rs` — extract `TtyWaitQueue` from current code
5. Create `ldisc.rs` — move `line_disc.rs` content (will be enhanced in Phase 2)
6. Create `session.rs` — stub for Phase 4
7. Update `drivers/src/lib.rs` to use the module instead of single file
8. Update all call sites: `tty::tty_read_cooked` → `tty::read(tty_index, ...)`
9. Initialize TTY 0 (serial console) and TTY 1 (virtual console) at boot
10. `tty_handle_input_char` → `tty::push_input(tty_index, c)`

### 5.5 Compatibility shim

During migration, provide backward-compatible public functions that delegate to TTY 0 (serial console):

```rust
// Temporary shim — will be removed in Phase 5
pub fn tty_read_cooked(buffer: *mut u8, max: usize, nonblock: bool) -> isize {
    read(TtyIndex(0), buffer, max, nonblock)
}

pub fn tty_handle_input_char(c: u8) {
    push_input(TtyIndex(0), c);
}
```

### 5.6 Files modified (actual)

| File | Change |
|------|--------|
| `drivers/src/tty.rs` | **Deleted** — replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | **Deleted** — moved to `drivers/src/tty/ldisc.rs` |
| `drivers/src/lib.rs` | Removed `pub mod line_disc`, added `#[cfg(feature = "itests")] pub mod tty_tests` |
| `drivers/src/tty/mod.rs` | **New** — `Tty` struct, `TtyIndex`, per-TTY API, backward-compatible shims |
| `drivers/src/tty/driver.rs` | **New** — `TtyDriver` trait, `TtyDriverKind` enum, `SerialConsoleDriver`, `VConsoleDriver` |
| `drivers/src/tty/table.rs` | **New** — `TTY_TABLE` global, `tty_table_init()`, `with_tty()` helpers |
| `drivers/src/tty/ldisc.rs` | **New** — `LineDisc` (moved from `line_disc.rs`, identical logic) |
| `drivers/src/tty/session.rs` | **New** — `TtySession` stub for Phase 4 |
| `drivers/src/tty_tests.rs` | **New** — 26 regression tests (LineDisc, TtyIndex, TtyDriverKind, TTY table, compat shims) |
| `drivers/src/ps2/keyboard.rs` | Unchanged — `crate::tty::tty_handle_input_char` path still valid |
| `drivers/src/syscall_services_init.rs` | Unchanged — `crate::tty` path still valid |
| `lib/src/kernel_services/syscall_services/tty.rs` | Unchanged — no import changes needed |
| `fs/src/fileio.rs` | Unchanged — calls `tty::read_cooked` via kernel services layer |

---

## 6. Phase 2: Enhanced Line Discipline ✅ COMPLETED

**Status**: Completed. All 962 tests pass (14 new Phase 2 regression tests). Build clean. `just test` passes.

**Implementation summary**: `LineDisc` rewritten from 216 lines to ~590 lines with:
- Input flag processing (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP, IXON flow control
- Output flag processing (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET via `OutputAction` enum
- Additional signal generation: SIGQUIT (Ctrl+\\), SIGTSTP (Ctrl+Z) alongside existing SIGINT
- Echo modes: ECHOCTL (^X for control chars), ECHOKE (kill with newline), column tracking
- Canonical editing: VWERASE (Ctrl+W word erase), VREPRINT (Ctrl+R redisplay via `ReprintLine` action), VLNEXT (Ctrl+V literal next)
- Flow control: IXON with VSTOP/VSTART (Ctrl+S / Ctrl+Q)
- Non-canonical: VMIN/VTIME parsed in ABI but timing enforcement deferred
- New ABI constants in `abi/src/syscall.rs`: 12 c_iflag bits, 5 c_oflag bits, 6 c_lflag bits, 6 new c_cc indices
- `write()` in `mod.rs` now applies `c_oflag` output processing
- `push_input`/`process_raw_char_for` handle `InputAction::ReprintLine`
- Default termios unchanged (`c_iflag: 0`, `c_oflag: 0`) to preserve existing behavior; userland can enable OPOST+ONLCR via `tcsetattr`

**Goal**: Bring `LineDisc` to feature parity with Linux's `n_tty` (simplified but complete).

### 6.1 Current gaps

The current `LineDisc` (`line_disc.rs`, 183 lines) supports:
- ✅ Canonical mode (ICANON): edit buffer → cooked on newline/EOF
- ✅ ECHO: basic character echo
- ✅ ECHOE: backspace echo (BS-SPACE-BS)
- ✅ ECHOK: kill-line echo (newline)
- ✅ ISIG: SIGINT on Ctrl+C (VINTR)
- ✅ VEOF, VERASE, VKILL

Missing:
- ❌ **Input flags** (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP, IXON, IXOFF, IUTF8
- ❌ **Output flags** (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET
- ❌ **Echo modes**: ECHOCTL (echo ^C for control chars), ECHOKE (kill with BS sequence), ECHOPRT
- ❌ **Non-canonical timing**: VMIN/VTIME — parsed in ABI but never enforced
- ❌ **Signals**: VQUIT (SIGQUIT), VSUSP (SIGTSTP), VDSUSP
- ❌ **Flow control**: VSTOP (Ctrl+S), VSTART (Ctrl+Q)
- ❌ **Word erase**: VWERASE (Ctrl+W in canonical mode)
- ❌ **Reprint**: VREPRINT (Ctrl+R to redisplay line)
- ❌ **Literal next**: VLNEXT (Ctrl+V to insert next char literally)

### 6.2 Enhanced `LineDisc` struct

```rust
pub struct LineDisc {
    termios: UserTermios,

    // Canonical mode buffers
    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,

    // Cooked output ring buffer (ready for userland read)
    cooked: [u8; COOKED_BUF_SIZE],
    cooked_head: usize,
    cooked_tail: usize,
    cooked_count: usize,

    // Non-canonical mode state
    raw_count: usize,          // bytes available for VMIN check

    // Echo state
    echo_pending: bool,        // deferred echo after output processing

    // Flow control
    stopped: bool,             // output stopped via XOFF (Ctrl+S)
    literal_next: bool,        // next char is literal (after Ctrl+V)

    // Columns tracking (for ECHOKE/ECHOPRT)
    column: usize,
}
```

### 6.3 New ABI constants

Add to `abi/src/syscall.rs`:

```rust
// c_iflag bits
pub const IGNBRK:  u32 = 0x001;
pub const BRKINT:  u32 = 0x002;
pub const IGNPAR:  u32 = 0x004;
pub const PARMRK:  u32 = 0x008;
pub const INPCK:   u32 = 0x010;
pub const ISTRIP:  u32 = 0x020;
pub const INLCR:   u32 = 0x040;
pub const IGNCR:   u32 = 0x080;
pub const ICRNL:   u32 = 0x100;
pub const IXON:    u32 = 0x400;
pub const IXOFF:   u32 = 0x1000;
pub const IUTF8:   u32 = 0x4000;

// c_oflag bits
pub const OPOST:   u32 = 0x01;
pub const ONLCR:   u32 = 0x04;
pub const OCRNL:   u32 = 0x08;
pub const ONOCR:   u32 = 0x10;
pub const ONLRET:  u32 = 0x20;

// c_lflag bits (in addition to existing ISIG, ICANON, ECHO, ECHOE, ECHOK, ECHONL)
pub const ECHOCTL: u32 = 0x200;
pub const ECHOPRT: u32 = 0x400;
pub const ECHOKE:  u32 = 0x800;
pub const NOFLSH:  u32 = 0x80;
pub const TOSTOP:  u32 = 0x100;
pub const IEXTEN:  u32 = 0x8000;

// c_cc indices (in addition to existing VINTR..VEOL)
pub const VQUIT:   usize = 1;  // already exists
pub const VSUSP:   usize = 10;
pub const VWERASE: usize = 14;
pub const VREPRINT: usize = 12;
pub const VLNEXT:  usize = 15;
pub const VSTOP:   usize = 9;
pub const VSTART:  usize = 8;
```

### 6.4 Input processing pipeline

```rust
pub fn input_char(&mut self, c: u8) -> InputAction {
    let iflag = self.termios.c_iflag;
    let lflag = self.termios.c_lflag;

    // 1. Input flag processing (c_iflag)
    let c = self.process_input_flags(c, iflag);

    // 2. Literal-next mode (Ctrl+V was pressed)
    if self.literal_next {
        self.literal_next = false;
        return self.insert_char(c);
    }

    // 3. Signal generation (ISIG)
    if (lflag & ISIG) != 0 {
        if c == self.cc(VINTR) { return InputAction::Signal(SIGINT); }
        if c == self.cc(VQUIT) { return InputAction::Signal(SIGQUIT); }
        if c == self.cc(VSUSP) { return InputAction::Signal(SIGTSTP); }
    }

    // 4. Flow control (IXON)
    if (iflag & IXON) != 0 {
        if c == self.cc(VSTOP) { self.stopped = true; return InputAction::None; }
        if c == self.cc(VSTART) { self.stopped = false; return InputAction::None; }
    }

    // 5. Canonical vs non-canonical
    if (lflag & ICANON) != 0 {
        self.canonical_input(c)
    } else {
        self.raw_input(c)
    }
}
```

### 6.5 Output processing

```rust
/// Process a byte through c_oflag before sending to the driver.
pub fn process_output(&self, c: u8) -> OutputAction {
    let oflag = self.termios.c_oflag;
    if (oflag & OPOST) == 0 {
        return OutputAction::Write(&[c]);
    }
    match c {
        b'\n' if (oflag & ONLCR) != 0 => OutputAction::Write(&[b'\r', b'\n']),
        b'\r' if (oflag & OCRNL) != 0 => OutputAction::Write(&[b'\n']),
        b'\r' if (oflag & ONOCR) != 0 && self.column == 0 => OutputAction::Suppress,
        _ => OutputAction::Write(&[c]),
    }
}
```

### 6.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Complete rewrite (was `line_disc.rs`) |
| `abi/src/syscall.rs` | Add c_iflag, c_oflag, c_lflag constants, new c_cc indices |

---

## 7. Phase 3: Input Pipeline Cleanup ✅ COMPLETED

**Goal**: Establish a single, clean input path: keyboard → TTY. Remove dual delivery.

### 7.1 Current dual delivery

In `drivers/src/ps2/keyboard.rs:handle_scancode()`:

```rust
// Line 227: Sends to compositor input queue (ALL events, press + release)
input_event::input_route_key_event(make_code, ascii, is_press, timestamp_ms);

// Lines 279, 294: Sends to TTY line discipline (press only, non-zero ASCII)
tty_handle_input_char(ascii);
```

The compositor input queue receives keyboard events that the shell **ignores** (only processes mouse events in `poll_batch`). This is wasted work and architectural confusion.

### 7.2 New input routing

```
PS/2 Keyboard Interrupt
    │
    ├── Key Press (ASCII != 0)
    │   └── tty::push_input(active_tty_index, ascii)
    │       └── LineDisc::input_char(ascii) → cooked buffer → wake readers
    │
    ├── Key Press (extended: arrows, delete, etc.)
    │   └── tty::push_input(active_tty_index, extended_key)
    │
    └── Key Release
        └── (nothing — release events are ignored)

PS/2 Mouse Interrupt
    └── input_event::input_route_pointer_*()  (unchanged)
```

### 7.3 Changes to keyboard handler

```rust
// drivers/src/ps2/keyboard.rs — handle_scancode()

pub fn handle_scancode(scancode: u8) {
    let mut state = STATE.lock();

    if scancode == 0xE0 {
        state.extended_code = true;
        return;
    }

    let is_press = !is_break_code(scancode);
    let make_code = get_make_code(scancode);

    state.scancode_buffer.push_overwrite(scancode);

    let ascii = translate_scancode(scancode, &state.modifiers);
    // REMOVED: input_event::input_route_key_event(...)
    // Keyboard events no longer go to the compositor input queue.

    if matches!(make_code, 0x2A | 0x36 | 0x1D | 0x38 | 0x3A) {
        handle_modifier(&mut state.modifiers, make_code, is_press);
        return;
    }

    if state.extended_code {
        state.extended_code = false;
        if !is_press { return; }
        let extended_key = match make_code { /* ... */ };
        if extended_key != 0 {
            drop(state);
            tty::push_input(tty::active_tty(), extended_key);
            request_reschedule_from_interrupt();
        }
        return;
    }

    if !is_press { return; }

    if ascii != 0 {
        drop(state);
        tty::push_input(tty::active_tty(), ascii);
        request_reschedule_from_interrupt();
    }
}
```

### 7.4 Serial input integration

Currently `tty_drain_hw_input()` polls the UART and drains `keyboard::char_buffer` (which is never written to). Simplify:

```rust
// In Tty::drain_hw_input() — called from read/poll
fn drain_hw_input(&mut self) {
    let mut scratch = [0u8; 64];
    let count = self.driver.drain_input(&mut scratch);
    for i in 0..count {
        let c = self.ldisc.process_input_flags_byte(scratch[i]);
        self.ldisc_input(c);
    }
}
```

The `SerialConsoleDriver::drain_input` wraps the existing `serial_poll_receive` + `INPUT_BUFFER` drain logic.
The `VConsoleDriver::drain_input` returns 0 (PS/2 input comes via interrupt, not polling).

### 7.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/ps2/keyboard.rs` | Remove `input_route_key_event` for keyboard, call `tty::push_input` |
| `drivers/src/tty/mod.rs` | Add `push_input(TtyIndex, u8)`, `active_tty() -> TtyIndex` |
| `drivers/src/tty/driver.rs` | `SerialConsoleDriver::drain_input` wraps UART polling |
| `drivers/src/input_event.rs` | No changes — still used for mouse/pointer events |

---

## 8. Phase 4: Session & Process Group Management ✅ COMPLETED

**Status**: Completed. All 988 tests pass (21 new Phase 4 regression tests). Build clean. `just test` passes.

**Implementation summary**: `TtySession` rewritten from 35-line stub to 202-line full implementation with:
- `TtySession` struct with `session_leader`, `session_id`, `fg_pgrp`, `focused_task_id` fields
- `ForegroundCheck` enum: `Allowed`, `NoSession`, `BackgroundRead`, `BackgroundWrite`
- `check_read()` / `check_write()` for POSIX foreground access control
- `task_has_access()` transitional helper combining compositor focus + session-based checks
- `set_fg_pgrp_checked()` with same-session validation
- `attach()` / `detach()` for session lifecycle management
- Sentinel constants: `NO_SESSION`, `NO_FOREGROUND_PGRP`
- Cross-crate wiring: `TIOCGSID` ioctl, `current_task_pgid/sid` runtime services, `setsid()` detaches controlling TTY, `TIOCSPGRP` validates caller session
- 21 new regression tests covering session attach/detach, foreground checks, read/write access control, per-TTY API round-trips

**Goal**: Replace ad-hoc `TTY_FOCUSED_TASK_ID` / `TTY_FOREGROUND_PGRP` with proper POSIX-like sessions.

### 8.1 Current focus system problems

| Issue | Location | Problem |
|-------|----------|---------|
| `TTY_FOCUSED_TASK_ID` | `tty.rs:42` | Global atomic. Set lazily on first read. Never properly transferred between parent/child. |
| `TTY_FOREGROUND_PGRP` | `tty.rs:43` | Compares pgrp with task_id — semantically wrong. Process group ≠ task ID. |
| `tty_task_has_focus` | `tty.rs:196-203` | OR's two unrelated checks. Focus and foreground pgrp are conflated. |
| `tty_ensure_focus_for_task` | `tty.rs:205-209` | CAS-free set. Race-prone. |
| `tty_wait_for_focus` | `tty.rs:211-225` | Blocks on global `TTY_FOCUS_QUEUE`. No per-TTY granularity. |

### 8.2 New session model

```rust
// drivers/src/tty/session.rs

/// Per-TTY session/foreground state.
pub struct TtySession {
    /// Session leader's process ID (0 = no session).
    pub session_leader: u32,

    /// Session ID (typically == session_leader's PID).
    pub session_id: u32,

    /// Foreground process group ID.
    pub fg_pgrp: u32,
}
```

### 8.3 Foreground check in read/write

```rust
// In tty::read()
fn check_foreground(&self, caller_pgrp: u32) -> Result<(), TtyError> {
    if self.session.fg_pgrp == 0 {
        return Ok(()); // No session control, allow
    }
    if caller_pgrp == self.session.fg_pgrp {
        return Ok(()); // Caller is in foreground group
    }
    // Background process trying to read → SIGTTIN
    Err(TtyError::BackgroundRead)
}
```

### 8.4 Per-process controlling terminal

Add to the process/task structure (in `core/src/scheduler/task.rs` or equivalent):

```rust
/// Controlling terminal index (None = no controlling terminal).
pub controlling_tty: Option<TtyIndex>,

/// Process group ID.
pub pgrp: u32,

/// Session ID.
pub session_id: u32,
```

These are:
- Inherited from parent on `fork()`
- Set on `setsid()` (new session, no controlling terminal)
- Acquired on first open of a TTY (if no controlling terminal and process is session leader)

### 8.5 ioctl integration

| ioctl | Action |
|-------|--------|
| `TIOCGPGRP` | Return `tty.session.fg_pgrp` |
| `TIOCSPGRP` | Set `tty.session.fg_pgrp` (caller must be in same session) |
| `TIOCGSID` | Return `tty.session.session_id` |

### 8.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/session.rs` | New: `TtySession` struct, foreground checks |
| `drivers/src/tty/mod.rs` | Use `TtySession` in `Tty` struct |
| `core/src/scheduler/task.rs` | Add `controlling_tty`, `pgrp`, `session_id` fields |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Update TIOCGPGRP/TIOCSPGRP to use per-TTY session |

---

## 9. Phase 5: FD Integration ✅ COMPLETED

**Status**: Completed. All 997 tests pass (9 new Phase 5 regression tests). Build clean. `just test` passes.

**Goal**: Wire the new TTY subsystem into the file descriptor layer.

### 9.1 FileDescriptor changes

Currently `FileDescriptor` has a boolean `console` flag. Replace with an optional TTY index:

```rust
// fs/src/fileio.rs
struct FileDescriptor {
    // ... existing fields ...
    console: bool,        // REMOVE
    tty_index: Option<TtyIndex>,  // ADD: None = not a TTY, Some(i) = TTY i
}
```

### 9.2 Console FD bootstrap

```rust
fn bootstrap_console_fds(table: &mut FileTableSlot, tty: TtyIndex) {
    // FD 0 = stdin
    table.descriptors[0] = FileDescriptor {
        flags: FILE_OPEN_READ,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
    // FD 1 = stdout
    table.descriptors[1] = FileDescriptor {
        flags: FILE_OPEN_WRITE,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
    // FD 2 = stderr
    table.descriptors[2] = FileDescriptor {
        flags: FILE_OPEN_WRITE,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
}
```

### 9.3 Read/write/poll routing

```rust
// file_read_fd — console path
if let Some(tty_idx) = desc.tty_index {
    let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
    drop(guard);
    return tty::read(tty_idx, buffer as *mut u8, count, is_nonblock);
}

// file_write_fd — console path
if let Some(tty_idx) = desc.tty_index {
    drop(guard);
    let bytes = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
    return tty::write(tty_idx, bytes) as ssize_t;
}

// file_poll_fd — console path
if let Some(tty_idx) = desc.tty_index {
    let mut revents = 0u16;
    if (events & POLLIN) != 0 && tty::has_data(tty_idx) {
        revents |= POLLIN;
    }
    if (events & POLLOUT) != 0 {
        revents |= POLLOUT;
    }
    drop(guard);
    return revents;
}
```

### 9.4 Write with output processing

Currently console writes go directly to `serial_write_bytes`. With the new system:

```rust
// tty::write(tty_index, bytes)
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    let mut table = TTY_TABLE.lock();
    let tty = match table[idx.0 as usize].as_mut() {
        Some(t) => t,
        None => return 0,
    };

    for &c in data {
        match tty.ldisc.process_output(c) {
            OutputAction::Write(bytes) => tty.driver.write_output(bytes),
            OutputAction::Suppress => {}
        }
    }
    data.len()
}
```

This means `\n` → `\r\n` conversion (ONLCR) is finally handled by the TTY layer, not by userland.

### 9.5 ioctl dispatch

Update `core/src/syscall/fs/poll_ioctl_handlers.rs`:

```rust
fn syscall_ioctl(fd: i32, request: u64, arg: u64) -> i64 {
    let tty_idx = match get_tty_index_for_fd(fd) {
        Some(idx) => idx,
        None => return -1, // ENOTTY
    };

    match request {
        TCGETS => tty::get_termios(tty_idx, arg as *mut UserTermios),
        TCSETS | TCSETSW | TCSETSF => tty::set_termios(tty_idx, arg as *const UserTermios),
        TIOCGPGRP => tty::get_foreground_pgrp(tty_idx, arg as *mut u32),
        TIOCSPGRP => tty::set_foreground_pgrp(tty_idx, arg as *const u32),
        TIOCGWINSZ => tty::get_winsize(tty_idx, arg as *mut UserWinsize),
        TIOCSWINSZ => tty::set_winsize(tty_idx, arg as *const UserWinsize),
        _ => -1, // EINVAL
    }
}
```

### 9.6 Files modified (planned)

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Replace `console: bool` with `tty_index: Option<TtyIndex>`, update read/write/poll |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Route ioctl to per-TTY, add TIOCGWINSZ/TIOCSWINSZ |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update to call `tty::*` with TTY index |

### 9.7 Implementation Notes

**Actual files modified:**

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Replaced `console: bool` with `tty_index: Option<u8>` in `FileDescriptor`; routed `file_read_fd` through `tty::read_cooked(tty_idx, ...)`, `file_write_fd` through `tty::write_bytes(tty_idx, ...)`, `file_poll_fd` through `tty::has_cooked_data(tty_idx)`; added `file_get_tty_index(process_id, fd) -> Option<u8>` for ioctl dispatch; updated `bootstrap_console_fds` to set `tty_index: Some(0)` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Rewrote `syscall_ioctl` to resolve TTY index from FD via `file_get_tty_index(pid, fd)` instead of hardcoded `0`; all TCGETS/TCSETS/TIOCGWINSZ/TIOCSWINSZ/TIOCGPGRP/TIOCSPGRP/TIOCGSID now use the resolved `tty_idx` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added `write_bytes(tty_index: u8, buf: *const u8, len: usize) -> usize` to `TtyServices` |
| `drivers/src/syscall_services_init.rs` | Added `tty_write_bytes_adapter` function and registered it in `TTY_SERVICES` struct |
| `drivers/src/tty_tests.rs` | Added 9 Phase 5 regression tests (output processing, raw passthrough, invalid index, per-TTY termios/winsize/pgrp/data/session isolation, invalid read) |

**Key design decisions:**
- Used `Option<u8>` for `tty_index` instead of `Option<TtyIndex>` to match existing per-TTY API signatures across the codebase
- Added `write_bytes` as a new `TtyServices` function rather than modifying existing `write_char`, keeping backward compatibility
- `file_get_tty_index` is a standalone public function in `fileio.rs` to allow ioctl handlers to resolve TTY index without full FD lock
---

## 10. Phase 6: Control-Plane Correctness ✅ COMPLETED

> **Priority**: P0 — Must fix before any new feature work.
> **Rationale**: The deep architectural review (comparing against Linux `tty_struct` + `n_tty` and RedoxOS) identified that the biggest risks in SlopOS's TTY are **not** parsing/processing logic (which is solid) but **control-plane correctness**: compositor focus is conflated with POSIX session foreground, the transitional `task_has_access()` permanently bypasses session control, and the `TtyIndex` type leaks to raw `u8` at the FD boundary.

**Goal**: Establish correct POSIX-like control semantics by separating compositor focus from session foreground, making `check_read()` the authoritative gate, and enforcing type safety across crate boundaries.

**Status**: Completed. All tests pass (58 suites, 0 failures). Build clean. `just test` passes.

**Implementation summary**:
- **10.1 (Compositor focus split)**: `set_focus()` → `set_compositor_focus()` / `get_focus()` → `get_compositor_focus()` — only sets `focused_task_id`, never touches `fg_pgrp`. Updated `TtyServices` and `ui_handlers.rs` call sites.
- **10.2 (check_read as sole gate)**: Removed `task_has_access()` from `session.rs`. `tty::read()` now uses `check_read()` directly as the sole read-side gate. Background reads send `SIGTTIN` signal to the calling process.
- **10.3 (TtyIndex type safety)**: Moved `TtyIndex` newtype to `abi/src/syscall.rs` (`#[repr(transparent)]` wrapper around `u8`) for cross-crate visibility. Changed `FileDescriptor.tty_index` from `Option<u8>` to `Option<TtyIndex>`. Updated all `TtyServices` signatures and adapter functions to accept `TtyIndex` directly.
- **10.4 (Signal constants)**: Added `SIGINT=2`, `SIGQUIT=3`, `SIGTSTP=20`, `SIGTTIN=21` constants to `abi/src/syscall.rs`. Replaced all magic numbers in `ldisc.rs` signal generation with named constants.
- **Regression tests**: Replaced 4 `task_has_access` tests with 3 `check_read` tests. Renamed `test_focus` → `test_compositor_focus`. Added 4 new Phase 6 regression tests: `test_tty_index_abi_type`, `test_signal_constants`, `test_set_compositor_focus_does_not_set_fg_pgrp`, `test_check_read_sole_gate_background`.

### 10.1 Split compositor focus from POSIX foreground

**Problem**: `set_focus(task_id)` in `mod.rs` sets **both** `focused_task_id` AND `fg_pgrp` to the same value.  A task ID is not a process group ID.  Compositor window focus and POSIX foreground group are independent concepts.

**Current (broken)**:
```rust
pub fn set_focus(task_id: u32) -> i32 {
    // ...
    tty.session.focused_task_id = task_id;
    tty.session.fg_pgrp = task_id;  // ← WRONG: task_id != pgid
}
```

**Fix**:
- `set_focus()` → renamed `set_compositor_focus()`, sets **only** `focused_task_id`
- `fg_pgrp` is modified **only** via `TIOCSPGRP` / `set_foreground_pgrp_checked()`
- Update `core/src/syscall/ui_handlers.rs` to call the renamed function
- Add `set_compositor_focus` / `get_compositor_focus` to `TtyServices`

### 10.2 Replace `task_has_access()` with proper `check_read()` gating

**Problem**: `task_has_access()` in `session.rs` is labeled "transitional" but is the primary read gate.  It OR's compositor focus with session foreground, meaning a background process with compositor focus can bypass POSIX session control.

**Current (broken)**:
```rust
pub fn task_has_access(&self, task_id: u32, caller_pgid: u32) -> bool {
    // Compositor focus takes priority (breaks POSIX)
    if self.focused_task_id != 0 && self.focused_task_id == task_id {
        return true;
    }
    // Session check (correct)
    if self.fg_pgrp != NO_FOREGROUND_PGRP && self.fg_pgrp == caller_pgid {
        return true;
    }
    // ...
}
```

**Fix**:
- Make `check_read(caller_pgid, caller_sid)` the **sole** read-side gate in `tty::read()`
- Use compositor focus **only** for the "no session attached yet" bootstrap path (first reader before `setsid()`)
- Remove or deprecate `task_has_access()`
- Update `tty::read()` and `auto_attach_session()` to use `check_read()` directly
- Add `BackgroundRead` → `SIGTTIN` signal delivery in `read()` when `check_read()` returns `BackgroundRead`

### 10.3 Type safety: `Option<TtyIndex>` end-to-end

**Problem**: `fs/src/fileio.rs` uses `tty_index: Option<u8>` and `TtyServices` bridge functions accept raw `u8`.  Any integer can be silently passed as a TTY index without compile-time checking.

**Fix**:
- Change `FileDescriptor.tty_index` from `Option<u8>` to `Option<TtyIndex>` (re-export `TtyIndex` in `abi/` or `lib/`)
- Update `TtyServices` function signatures to accept `TtyIndex` (or a newtype `TtyHandle(u8)` that lives in `abi/`)
- Update `file_get_tty_index()` to return `Option<TtyIndex>`
- Update all ioctl dispatch in `poll_ioctl_handlers.rs` to pass `TtyIndex`
- Update `drivers/src/syscall_services_init.rs` adapters

### 10.4 Replace hardcoded signal numbers with ABI constants

**Problem**: `ldisc.rs` uses magic numbers `2` (SIGINT), `3` (SIGQUIT), `20` (SIGTSTP) in `InputAction::Signal()`.

**Fix**:
- Add `SIGINT`, `SIGQUIT`, `SIGTSTP` constants to `abi/src/syscall.rs` (or a new `abi/src/signal.rs`)
- Replace all hardcoded signal numbers in `ldisc.rs` with named constants
- Update test assertions to use the same constants

### 10.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Rename `set_focus`→`set_compositor_focus`, replace `task_has_access` calls with `check_read`, update `read()` gating |
| `drivers/src/tty/session.rs` | Deprecate/remove `task_has_access()`, add SIGTTIN delivery helper |
| `drivers/src/tty/ldisc.rs` | Replace signal magic numbers with ABI constants |
| `fs/src/fileio.rs` | Change `tty_index: Option<u8>` → `Option<TtyIndex>` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update `TtyServices` signatures to use `TtyIndex` |
| `drivers/src/syscall_services_init.rs` | Update adapter functions |
| `core/src/syscall/ui_handlers.rs` | Call `set_compositor_focus` instead of `set_focus` |
| `abi/src/syscall.rs` | Add `SIGINT`/`SIGQUIT`/`SIGTSTP` constants |
| `drivers/src/tty_tests.rs` | Update tests for renamed APIs, add `check_read()`-based gating tests, signal constant tests |

---

## 11. Phase 7: Lifecycle & Hangup Semantics ✅ COMPLETED

> **Priority**: P1 — Must fix before PTY implementation or proper shell exit.
> **Rationale**: Linux's biggest TTY complexity is lifecycle management, and it exists for a reason.  Without open counts and hangup signaling, dead processes can hold TTY resources, PTY pairs can't clean up, and shell exit doesn't notify children.  This is the single biggest "pain later" item if deferred.

**Goal**: Add TTY lifecycle management (open/close tracking, hangup signaling, controlling terminal acquisition) modeled after Linux's `tty_port` + `kref` pattern, adapted for SlopOS's static table.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- Added lifecycle state to TTYs (`open_count`, `hung_up`) and wired robust hangup semantics (`hangup()`, wake blocked readers, EOF/EIO behavior) in `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, and `drivers/src/tty/ldisc.rs`.
- Integrated full FD reference counting for TTY-backed descriptors across bootstrap, dup/fork cloning, and close/reset paths in `fs/src/fileio.rs`; exposed hangup via poll (`POLLHUP`).
- Implemented controlling-terminal acquisition via `TIOCSCTTY` with session-leader checks in `core/src/syscall/fs/poll_ioctl_handlers.rs`, added `controlling_tty` task state in scheduler structs, and wired session-leader exit hangup in `core/src/scheduler/task.rs`.
- Extended ABI/services for lifecycle operations (`TIOCSCTTY`, `SIGHUP`, `SIGCONT`, `open_ref`, `close_ref`, `hangup`, `is_hung_up`) across `abi/src/syscall.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, and `drivers/src/syscall_services_init.rs`.
- Added regression coverage in `drivers/src/tty_tests.rs` (flush-all, open-count lifecycle, hangup flag/detach, hangup read semantics) and `core/src/syscall/tests.rs` (`TIOCSCTTY` leader/non-leader behavior).

### 11.1 Add open count to `Tty`

**What**: Track how many file descriptors reference each TTY.  This enables "last close" detection for cleanup.

```rust
pub struct Tty {
    // ... existing fields ...

    /// Number of open file descriptors referencing this TTY.
    pub open_count: u16,
}
```

- Increment on `open()` / FD bootstrap / `dup()` / `fork()` when FD has `tty_index`
- Decrement on `close()` / process exit when FD has `tty_index`
- On `open_count == 0`: trigger cleanup (flush buffers, optionally deallocate for PTY slots)

### 11.2 Implement `tty_hangup()`

**What**: When a session leader exits or carrier is lost, the controlling TTY must signal the session.

```rust
/// Hangup a TTY: send SIGHUP + SIGCONT to the session's foreground group,
/// flush buffers, wake all blocked readers (they get EOF / -EIO).
pub fn hangup(idx: TtyIndex) {
    let (fg_pgrp, session_id) = {
        let mut table = TTY_TABLE.lock();
        let tty = match table.get_mut(idx.0 as usize) {
            Some(Some(t)) => t,
            _ => return,
        };
        let fg = tty.session.fg_pgrp;
        let sid = tty.session.session_id;
        tty.ldisc.flush_all();  // New: clear edit + cooked buffers
        tty.session.detach();   // Clear session state
        (fg, sid)
    };

    // Signal outside the lock to avoid deadlock.
    if fg_pgrp != 0 {
        let _ = signal_process_group(fg_pgrp, SIGHUP);
        let _ = signal_process_group(fg_pgrp, SIGCONT);
    }

    // Wake all blocked readers — they will see EOF or -EIO.
    TTY_INPUT_WAITERS[idx.0 as usize].wake_all();
}
```

### 11.3 Wire hangup into process/session leader exit

- In `core/src/scheduler/task.rs` (or process exit path): when a session leader exits, find its controlling TTY and call `tty::hangup()`
- In `setsid()`: already calls `detach_session_by_id()` — verify it also handles hangup if old session had a controlling terminal

### 11.4 Controlling terminal acquisition (`TIOCSCTTY`)

**What**: Implement the `TIOCSCTTY` ioctl so a session leader can explicitly acquire a controlling terminal.

- Only session leaders with no existing controlling TTY may call this
- Set `tty.session.attach(caller_sid, caller_pgid)`
- Set `controlling_tty` in the process's task struct
- Add `TIOCSCTTY` constant to `abi/src/syscall.rs` and dispatch in `poll_ioctl_handlers.rs`

### 11.5 Add `flush_all()` to `LineDisc`

```rust
/// Clear both edit and cooked buffers (used during hangup/close).
pub fn flush_all(&mut self) {
    self.edit_len = 0;
    self.cooked_head = 0;
    self.cooked_tail = 0;
    self.cooked_count = 0;
    self.stopped = false;
    self.literal_next = false;
    self.column = 0;
}
```

### 11.6 Blocked reader behavior on hangup

- Readers blocked in `TTY_INPUT_WAITERS[idx].wait_event(...)` are woken by `hangup()`
- On wakeup, `read()` re-checks: if session is detached and no data, return 0 (EOF)
- Non-blocking reads return `-EIO` if TTY is hung up
- Add a `hung_up: bool` flag to `Tty` to track post-hangup state

### 11.7 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Added `open_count`/`hung_up`; implemented `open_ref`, `close_ref`, `hangup`, `is_hung_up`; added EOF/EIO read behavior after hangup |
| `drivers/src/tty/table.rs` | Initialized lifecycle fields in `Tty::new()` |
| `drivers/src/tty/ldisc.rs` | Added `flush_all()` |
| `fs/src/fileio.rs` | Wired TTY refcount lifecycle through bootstrap/dup/fork/close and exposed hangup through `POLLHUP` |
| `core/src/scheduler/task_struct.rs` | Added `controlling_tty: Option<TtyIndex>` to task state |
| `core/src/scheduler/task.rs` | Wired session-leader exit path to `tty::hangup()` |
| `core/src/syscall/process_handlers.rs` | Cleared `controlling_tty` in `setsid()` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Added `TIOCSCTTY` dispatch and session-leader ownership checks |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added lifecycle service methods (`attach_session`, `open_ref`, `close_ref`, `hangup`, `is_hung_up`) |
| `drivers/src/syscall_services_init.rs` | Added adapters and service wiring for new lifecycle methods |
| `abi/src/syscall.rs` | Added `TIOCSCTTY`, `SIGHUP`, and `SIGCONT` constants |
| `drivers/src/tty_tests.rs` | Added hangup/open-count/flush regression tests |
| `core/src/syscall/tests.rs` | Added `TIOCSCTTY` regression tests (leader success, non-leader reject) |

---

## 12. Phase 8: Per-TTY Locking & Performance ✅ COMPLETED

> **Priority**: P1 — Must fix before multiple active TTYs or PTY support.
> **Rationale**: The single `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>` lock protected **all** 8 TTY slots.  Any operation on TTY 0 blocked all operations on TTY 1–7.  `write()` held the lock for the entire byte-by-byte serial output loop (~86μs/byte at 115200 baud).  A 1 KB write held the global lock for ~86 ms.  Linux uses per-tty mutexes with a global lock only for lookup/registration.

**Goal**: Move to per-TTY locking so that operations on different TTYs are fully independent, and release the lock before slow driver I/O.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (1016/1016 tests, 0 failures, 7 new Phase 8 regression tests).

**Implementation summary**:
- **12.1 (Per-TTY lock architecture)**: Replaced `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>` (single global lock) with `TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS]` (per-slot independent locks).  Matches existing `UDP_RX_QUEUES` pattern in the socket module.  All ~39 `TTY_TABLE.lock()` call sites in `mod.rs` rewritten to per-slot `TTY_SLOTS[slot].lock()`.
- **12.2 (Split-write pattern)**: `write()` now processes output through the line discipline under the per-TTY lock into a 256-byte stack buffer, copies a lightweight `DriverId` enum, drops the lock, then writes the buffered bytes to hardware via `write_driver_unlocked()`.  Slow serial I/O no longer blocks other TTYs.
- **12.3 (Merged drain+read)**: `read()` now performs foreground check + `drain_hw_input()` + `ldisc.read()` in a single per-TTY lock acquisition per loop iteration, reducing from 5–6 separate lock/unlock cycles to 1.  Deferred signal delivery (e.g. Ctrl+C on serial) happens after dropping the lock.
- **12.4 (Idle callback iterates all TTYs)**: `input_available_cb()` now iterates all `MAX_TTYS` slots, draining hardware input and waking blocked readers on each active TTY (previously only checked TTY 0).
- **12.5 (Lock ordering documented)**: Comprehensive lock ordering rules documented in `table.rs`: never hold two per-TTY locks simultaneously; `TTY_INPUT_WAITERS` is separate from `TTY_SLOTS` to avoid lock-order violations during blocking waits.
- **12.6 (DriverId for lock-free I/O)**: Added `DriverId` enum (`SerialConsole`, `VConsole`, `None`) with `#[derive(Clone, Copy, PartialEq, Eq, Debug)]` and `TtyDriverKind::id()` method for the split-write pattern.  `write_driver_unlocked(DriverId, &[u8])` performs hardware I/O without any TTY lock.
- **Regression tests**: 7 new Phase 8 tests: `test_phase8_per_tty_lock_independence` (slots lockable simultaneously), `test_phase8_driver_id_round_trip` (DriverId matches driver kind), `test_phase8_split_write_returns_input_len` (split-write correctness), `test_phase8_idle_cb_iterates_all_ttys` (idle callback all-TTY check), `test_phase8_merged_drain_read` (single-lock drain+read), `test_phase8_with_tty_per_slot` (per-slot with_tty correctness), `test_phase8_driver_id_traits` (Copy/Clone/Eq).

### 12.1 Per-TTY lock architecture

**Previous**:
```rust
pub static TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>;  // One lock for everything
```

**New**:
```rust
/// Per-TTY locked slots.  Each element is independently locked.
pub static TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS] = [const { IrqMutex::new(None) }; MAX_TTYS];
```

- No global table lock — each slot is independently locked
- `Tty` struct name kept (not renamed to `TtyInner`) to minimize churn
- `with_tty()` and `with_tty_ref()` helpers updated to lock individual slots

### 12.2 Split-write via DriverId

```rust
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    // Phase 1: Process under per-TTY lock (fast — pure computation).
    let driver_id;
    let mut out_buf = [0u8; 256];
    let mut out_len = 0;
    { let mut guard = TTY_SLOTS[slot].lock(); /* process ldisc */ driver_id = tty.driver.id(); }
    // Phase 2: Driver I/O without lock (slow — hardware).
    write_driver_unlocked(driver_id, &out_buf[..out_len]);
}
```

### 12.3 Merged drain+read

Single lock acquisition in `read()` loop body combines: foreground check, `drain_hw_input()`, `ldisc.read()`, and deferred signal extraction.

### 12.4 Idle callback iterates all TTYs

`input_available_cb()` loops over `0..MAX_TTYS`, locking and releasing each slot individually.

### 12.5 Lock ordering rules

Documented in `table.rs` module doc:
1. **`TTY_SLOTS[i]`** — per-TTY, held for ldisc/session/termios.  **Never hold two simultaneously.**
2. **`TTY_INPUT_WAITERS[i]`** — separate static, condition closure may transiently re-acquire same slot.
3. Rule: **Never acquire `TTY_SLOTS[j]` while holding `TTY_SLOTS[i]`** (i ≠ j).

### 12.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/table.rs` | Replaced `TTY_TABLE` with `TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS]`; added comprehensive lock ordering documentation |
| `drivers/src/tty/mod.rs` | Rewrote all ~39 `TTY_TABLE.lock()` call sites to per-slot locking; implemented split-write pattern; merged drain+read; idle callback iterates all TTYs |
| `drivers/src/tty/driver.rs` | Added `DriverId` enum, `TtyDriverKind::id()`, `write_driver_unlocked()` |
| `drivers/src/tty_tests.rs` | Migrated `TTY_TABLE` → `TTY_SLOTS` in existing tests; added 7 Phase 8 regression tests |
---

## 13. Phase 9: Rust Idioms & Termios Completion ✅ COMPLETED

> **Priority**: P2 — Improves code quality and enables realistic userspace.
> **Rationale**: The current codebase uses C-style error patterns (`isize`, `-1`, raw pointers) internally where Rust `Result` types and slices would catch bugs at compile time.  Additionally, `VMIN/VTIME` is parsed but not enforced — terminal applications like readline and curses depend on this for responsive input.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (1026/1026 tests, 0 failures, 10 new Phase 9 regression tests).

**Implementation summary**:
- **13.1 (Result-based error handling)**: Added `TtyError` enum with 7 variants (`InvalidIndex`, `NotAllocated`, `BackgroundRead`, `BackgroundWrite`, `HungUp`, `WouldBlock`, `PermissionDenied`). Converted all 14 public API functions (`read`, `write`, `get_termios`, `set_termios`, `get_winsize`, `set_winsize`, `get_foreground_pgrp`, `set_foreground_pgrp`, `set_foreground_pgrp_checked`, `get_session_id`, `open_ref`, `close_ref`, `set_compositor_focus`, `get_compositor_focus`) to return `Result<T, TtyError>`. Void/bool helpers (`push_input`, `hangup`, `attach_session`, etc.) left unchanged.
- **13.2 (Slice-based internal API)**: `read()` now accepts `&mut [u8]`, `write()` accepts `&[u8]` internally. Raw pointer conversion happens only at the syscall adapter boundary in `syscall_services_init.rs` (16 adapter functions updated).
- **13.3 (VMIN/VTIME enforcement)**: Implemented all 4 POSIX non-canonical read cases using `wait_event_timeout` from `lib/src/waitqueue.rs`. Added `vmin_vtime()` and `is_canonical()` helpers to `ldisc.rs`. Case matrix: VMIN=0/VTIME=0 (immediate return), VMIN=0/VTIME>0 (timed wait), VMIN>0/VTIME=0 (block until VMIN bytes), VMIN>0/VTIME>0 (inter-byte timeout).
- **13.4 (`/dev/tty` support)**: Added intercept in `fs/src/fileio.rs::file_open_for_process()` that resolves `/dev/tty` to the calling process's controlling terminal via `current_task_controlling_tty()` from `DriverRuntimeServices`. Returns `-ENXIO` if no controlling terminal. Wired through `core/src/driver_hooks.rs` reading the task struct's `controlling_tty: Option<TtyIndex>` field.
- **13.5 (ABI boundary preserved)**: The `TtyServices` trait in `lib/` was NOT modified — it retains raw pointer signatures for ABI stability. Result/slice conversion happens entirely in the adapter layer (`syscall_services_init.rs`). Error mapping: `WouldBlock → -11 (EAGAIN)`, `HungUp → -5 (EIO)`, others `→ -1`.
- **Regression tests**: 10 new Phase 9 tests: `test_phase9_tty_error_variants` (all 7 error variants exist), `test_phase9_read_returns_result` (read returns Ok), `test_phase9_read_invalid_index_error` (out-of-range index), `test_phase9_read_not_allocated_error` (unallocated slot), `test_phase9_write_returns_result` (write returns Ok with count), `test_phase9_get_termios_returns_result` (termios retrieval), `test_phase9_vmin0_vtime0_immediate_return` (immediate non-canonical read), `test_phase9_vmin_enforcement` (VMIN byte count enforcement), `test_phase9_set_fg_pgrp_checked_permission_denied` (permission error on non-member pgrp), `test_phase9_hangup_read_returns_hung_up` (hung-up TTY returns HungUp error).

**Goal**: Adopt Rust-idiomatic error handling internally, enforce VMIN/VTIME in non-canonical mode, and add `/dev/tty` support for POSIX compliance.

### 13.1 Internal `Result<usize, Errno>` error handling

**What**: Replace C-style `isize` returns with `Result` internally, converting at the syscall ABI boundary.

```rust
/// Kernel-internal error type for TTY operations.
#[derive(Debug, Clone, Copy)]
pub enum TtyError {
    InvalidIndex,
    NotAllocated,
    BackgroundRead,   // → SIGTTIN
    BackgroundWrite,  // → SIGTTOU
    HungUp,           // → -EIO
    WouldBlock,       // → -EAGAIN
}

// Internal API returns Result:
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> { ... }

// Syscall boundary converts:
fn tty_read_adapter(idx: u8, buf: *mut u8, max: usize, nonblock: bool) -> isize {
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, max) };
    match tty::read(TtyIndex(idx), slice, nonblock) {
        Ok(n) => n as isize,
        Err(TtyError::WouldBlock) => -11, // EAGAIN
        Err(TtyError::HungUp) => -5,      // EIO
        Err(_) => -1,
    }
}
```

### 13.2 Accept slices internally (not raw pointers)

**What**: Change internal functions from `read(idx, buffer: *mut u8, max: usize)` to `read(idx, buf: &mut [u8])`.  Keep raw pointers only at the syscall adapter boundary.

### 13.3 VMIN/VTIME enforcement

**What**: Implement non-canonical read timing:

- **VMIN > 0, VTIME == 0**: Block until VMIN bytes available (current behavior, but enforce count)
- **VMIN == 0, VTIME > 0**: Return immediately if data; else wait up to VTIME deciseconds
- **VMIN > 0, VTIME > 0**: Block until VMIN bytes OR VTIME inter-byte timeout
- **VMIN == 0, VTIME == 0**: Return immediately with available data (pure non-blocking)

Requires timer integration (scheduler or PIT-based timeout) for VTIME.

### 13.4 `/dev/tty` support

**What**: Allow a process to refer to "my controlling terminal" generically, needed for `isatty()`, `ttyname()`, and POSIX utilities.

- When opening `/dev/tty`, resolve to the process's `controlling_tty` from its task struct
- Return `-ENXIO` if no controlling terminal
- Wire into VFS/devfs if present, or handle as a special path in `open()`

### 13.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Change signatures to `Result<usize, TtyError>`, accept `&mut [u8]` slices, add `TtyError` enum |
| `drivers/src/tty/ldisc.rs` | Add VMIN/VTIME enforcement in `read()`, add timer-based timeout |
| `drivers/src/tty/session.rs` | Return `TtyError` variants instead of booleans |
| `drivers/src/syscall_services_init.rs` | Convert `Result` to `isize` at adapter boundary |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update `TtyServices` to match new signatures |
| `fs/src/fileio.rs` | Add `/dev/tty` resolution in `file_open_fd` |
| `drivers/src/tty_tests.rs` | Update all tests for `Result` returns, add VMIN/VTIME tests, `/dev/tty` tests |

---

## 14. Phase 10: Job Control Correctness ✅ COMPLETED

> **Priority**: P0 — Must fix before any real shell or interactive program works correctly.
> **Rationale**: The deep architectural review (comparing against Linux `tty_struct` + `n_tty` and RedoxOS) found that **write-side foreground enforcement is completely missing** — `write()` never calls `check_write()`, `SIGTTOU` is never delivered, and `check_read()` permits cross-session reads with a TODO comment. No job-control-aware shell (bash, zsh) can work correctly without these. Issues #2, #10, #12 from the review.

**Status**: Completed. All tests pass (11 suites, 0 failures). Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- **14.1 (Wire `check_write()` into `write()`)**: Added foreground check at the start of `write()` in `drivers/src/tty/mod.rs`. When `TOSTOP` is set in `c_lflag`, the write path reads the caller's task ID, looks up session/termios state from the locked TTY slot, and calls `check_write(caller_pgid, tostop)`. If `BackgroundWrite` is returned, `SIGTTOU` is delivered to the caller's process group (outside the lock) and `Err(TtyError::BackgroundWrite)` is returned. Kernel tasks (task_id == 0) are exempted to allow early-boot writes.
- **14.2 (Deliver SIGTTOU)**: Added `pub const SIGTTOU: u8 = 22` to `abi/src/syscall.rs` (matching the existing definition in `abi/src/signal.rs`). Imported `SIGTTOU` and `TOSTOP` in `drivers/src/tty/mod.rs` for use in the write-side foreground check.
- **14.3 (Tighten `check_read()`)**: Replaced the permissive cross-session TODO block in `session.rs::check_read()` with proper rejection — `caller_sid != 0 && caller_sid != self.session_id` now returns `ForegroundCheck::NoSession` (maps to `-EIO` in read). Kernel tasks (sid == 0) remain exempted for early-boot bootstrap.
- **Regression tests**: Added 8 Phase 10 tests: `test_phase10_sigttou_constant`, `test_phase10_check_write_tostop_blocks_background`, `test_phase10_check_write_no_tostop_allows_background`, `test_phase10_check_write_tostop_allows_foreground`, `test_phase10_check_read_cross_session_rejected`, `test_phase10_check_read_same_session_foreground`, `test_phase10_check_read_kernel_task_allowed`, `test_phase10_tty_write_foreground_with_tostop`.

**Goal**: Wire write-side foreground gating, deliver SIGTTOU for background writes, tighten read-side session enforcement, and close the remaining job-control gaps.

### 14.1 Wire `check_write()` into `write()`

**Problem**: `session.rs` defines `check_write()` returning `ForegroundCheck::BackgroundWrite`, but `mod.rs::write()` never calls it. Background processes can freely write to the terminal, overwriting foreground output.

**Fix**:
```rust
// In tty::write() — add foreground check before output processing
pub fn write(idx: TtyIndex, data: &[u8]) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    // ... lock slot ...
    
    // Check TOSTOP: if set, background writers get SIGTTOU
    if (tty.ldisc.termios().c_lflag & TOSTOP) != 0 {
        match tty.session.check_write(caller_pgid, caller_sid) {
            ForegroundCheck::Allowed => {}
            ForegroundCheck::BackgroundWrite => {
                let pgid = caller_pgid;
                drop(guard);
                signal_process_group(pgid, SIGTTOU);
                return Err(TtyError::BackgroundWrite);
            }
            _ => {}
        }
    }
    
    // ... existing output processing ...
}
```

### 14.2 Deliver SIGTTOU for background writes

**Problem**: The `SIGTTOU` constant exists in `abi/src/signal.rs` (value 22) but is never used anywhere in the TTY subsystem. Linux sends SIGTTOU to background process groups that attempt to write when TOSTOP is set.

**Fix**:
- Add SIGTTOU delivery in `write()` when `check_write()` returns `BackgroundWrite`
- Ensure `SIGTTOU` constant is accessible from `drivers/src/tty/mod.rs`
- Match Linux behavior: only enforce when `TOSTOP` is set in `c_lflag`

### 14.3 Tighten `check_read()` to reject cross-session access

**Problem**: `session.rs::check_read()` currently allows reads from processes in a different session with a TODO comment ("Phase 5 will enforce"). This was never closed. Any process on the system can read any TTY regardless of session membership.

**Fix**:
```rust
pub fn check_read(&self, caller_pgid: u64, caller_sid: u64) -> ForegroundCheck {
    // No session → allow (bootstrap path)
    if self.session_id == NO_SESSION {
        return ForegroundCheck::Allowed;
    }
    
    // Different session → reject (POSIX requirement)
    if caller_sid != self.session_id as u64 {
        return ForegroundCheck::NoSession;  // -EIO in read()
    }
    
    // Same session, foreground group → allow
    if self.fg_pgrp == NO_FOREGROUND_PGRP || caller_pgid == self.fg_pgrp as u64 {
        return ForegroundCheck::Allowed;
    }
    
    // Same session, background group → SIGTTIN
    ForegroundCheck::BackgroundRead
}
```

### 14.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add `check_write()` call in `write()`, deliver SIGTTOU, return `BackgroundWrite` error |
| `drivers/src/tty/session.rs` | Tighten `check_read()` to reject cross-session, remove permissive TODO |
| `abi/src/syscall.rs` | Add `SIGTTOU` constant (value 22) |
| `drivers/src/tty_tests.rs` | Add 8 Phase 10 regression tests: background write blocked with TOSTOP, cross-session read rejected, SIGTTOU constant, foreground write allowed, kernel task exempted |
---

## 15. Phase 11: Non-Canonical Timing Fix ✅ COMPLETED

> **Priority**: P0 — Breaks interactive serial tools and terminal programs.
> **Rationale**: POSIX specifies that when both VMIN > 0 and VTIME > 0, the timer starts **after the first byte arrives**, not when `read()` is called. The current implementation starts the timeout at call entry, meaning slow typists or serial devices get premature timeouts. Programs like minicom, picocom, and readline in raw mode depend on correct inter-byte timing.

**Goal**: Fix the VMIN > 0 / VTIME > 0 case to use inter-byte timeout semantics per POSIX.

**Status**: Completed. All tests pass (1039/1039, 5 new Phase 11 regression tests). Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- **15.1 (Two-phase wait for VMIN>0/VTIME>0)**: Rewrote the `(_, _)` match arm in `drivers/src/tty/mod.rs::read()` to implement correct POSIX inter-byte timeout semantics. When `total == 0` (no bytes received yet), `wait_timeout_ms` remains `None` so the read blocks indefinitely via `wait_event()` until the first byte arrives. Once `total > 0` (at least one byte received), `wait_timeout_ms` is set to `vtime_ms` so subsequent waits use `wait_event_timeout()` with the inter-byte timer. This matches the POSIX specification: the timer starts after the first byte, not at `read()` entry.
- **15.2 (No ldisc changes needed)**: The `vmin_vtime()` helper and `is_canonical()` methods in `ldisc.rs` were already correct and required no modifications.
- **Regression tests**: 5 new Phase 11 tests: `test_phase11_vmin_vtime_enough_data_returns_immediately` (VMIN bytes available returns immediately), `test_phase11_vmin_vtime_partial_nonblock` (partial data with nonblock returns available bytes), `test_phase11_vmin_vtime_no_data_nonblock` (no data returns WouldBlock), `test_phase11_vmin_vtime_interbyte_timeout_returns_partial` (blocking read with 1 byte returns after inter-byte timeout instead of blocking indefinitely), `test_phase11_ldisc_vmin_vtime_helper` (vmin_vtime accessor correctness).

### 15.1 Previous behavior (incorrect)

```rust
// In read() — VMIN>0, VTIME>0 case
// Timer started at read() call entry — WRONG
should_wait = true;
wait_timeout_ms = Some(vtime_ms); // ← always set, even with no bytes yet
```

### 15.2 Correct POSIX behavior (implemented)

```rust
// VMIN>0, VTIME>0: inter-byte timeout.
// The timer starts after the first byte arrives,
// NOT when read() is called.
should_wait = true;
// Phase 1: no bytes yet — wait indefinitely for the first byte.
// Phase 2: at least one byte received — start inter-byte timer.
if total > 0 {
    wait_timeout_ms = Some(vtime_ms);
}
// else: wait_timeout_ms remains None (indefinite)
```

### 15.3 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Rewrote `(_, _)` match arm in `read()` with two-phase wait: indefinite for first byte, inter-byte timeout after |
| `drivers/src/tty/ldisc.rs` | No changes needed (`vmin_vtime()` helper already correct) |
| `drivers/src/tty_tests.rs` | Added 5 Phase 11 regression tests (enough data immediate return, partial nonblock, no data WouldBlock, inter-byte timeout partial return, vmin_vtime helper) |


## 16. Phase 12: Sane Defaults & Output Column Tracking ✅ COMPLETED

> **Priority**: P1 — Without this, every terminal program looks broken out of the box.
> **Rationale**: Default `c_iflag: 0` and `c_oflag: 0` means `printf("hello\n")` doesn't produce a carriage return — the cursor advances to the next line but stays at the current column. Every userland program must manually `tcsetattr()` to enable ICRNL and ONLCR. Linux defaults have both on. Additionally, column tracking only happens in the input echo path, not the output path, breaking tab expansion and accurate line-erase.

**Goal**: Set Linux-compatible default termios flags and implement bidirectional column tracking.

### 16.1 Set sane default termios

```rust
// In LineDisc::new() or Tty::new()
let default_termios = UserTermios {
    c_iflag: ICRNL,                    // CR → NL on input
    c_oflag: OPOST | ONLCR,           // NL → CRNL on output
    c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE,
    c_cflag: 0,                        // unchanged
    c_cc: default_cc_array(),          // existing defaults
};
```

### 16.2 Track columns in output path

**Problem**: `process_output_byte()` in `ldisc.rs` doesn't update `self.column`. Only the input echo path tracks columns. This means ECHOKE can't accurately erase lines, and tab expansion can't compute stop positions.

**Fix**:
```rust
pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
    let oflag = self.termios.c_oflag;
    if (oflag & OPOST) == 0 {
        return OutputAction::Write(c);
    }
    
    match c {
        b'\n' if (oflag & ONLCR) != 0 => {
            self.column = 0;
            OutputAction::WritePair(b'\r', b'\n')
        }
        b'\r' => {
            self.column = 0;
            if (oflag & OCRNL) != 0 { OutputAction::Write(b'\n') }
            else if (oflag & ONOCR) != 0 && self.column == 0 { OutputAction::Suppress }
            else { OutputAction::Write(b'\r') }
        }
        b'\t' => {
            let spaces = 8 - (self.column % 8);
            self.column += spaces;
            OutputAction::Tab(spaces as u8)
        }
        b'\x08' => {  // Backspace
            if self.column > 0 { self.column -= 1; }
            OutputAction::Write(c)
        }
        c if c >= 0x20 && c < 0x7F => {
            self.column += 1;
            OutputAction::Write(c)
        }
        _ => OutputAction::Write(c),
    }
}
```

### 16.3 Add `OutputAction::Tab` variant

The existing `OutputAction` enum needs a new variant for tab expansion:
```rust
pub enum OutputAction {
    Write(u8),
    WritePair(u8, u8),
    Tab(u8),       // New: expand to N spaces
    Suppress,
}
```

### 16.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Update default termios, add column tracking to `process_output_byte()`, add `OutputAction::Tab`, tab expansion |
| `drivers/src/tty/mod.rs` | Handle `OutputAction::Tab` in `write()`, update default `Tty::new()` termios |
| `drivers/src/tty/mod.rs` | Handle `OutputAction::Tab` in `write()`, buffer threshold relaxed to `OUT_BUF_CAP - 8` |
| `drivers/src/tty_tests.rs` | Fixed 3 existing tests for new defaults; added 10 Phase 12 regression tests |

### 16.5 Implementation summary

**Completed**: All three sub-tasks implemented and verified.

1. **Sane defaults** (`LineDisc::new()`): `c_iflag: ICRNL`, `c_oflag: OPOST | ONLCR`, `c_lflag` gains `ECHOK | ECHOCTL | ECHOKE`.
2. **Output column tracking** (`process_output_byte()`): Rewritten with full column accounting for printable chars (+1), NL/CR (→0), tab (8-stop expansion), backspace (−1), and control chars (no change). Non-OPOST path tracks via `update_column_raw()`. Proper ONOCR (suppress CR at col 0), ONLRET (NL resets col), OCRNL+ONLRET interaction.
3. **Tab expansion**: New `OutputAction::Tab(u8)` variant returns the number of spaces; `Tty::write()` emits N spaces. Buffer split threshold relaxed from `OUT_BUF_CAP - 2` to `OUT_BUF_CAP - 8`.

**Tests** (10 new, 3 fixed):
- `test_phase12_default_termios_has_icrnl`
- `test_phase12_default_termios_has_opost_onlcr`
- `test_phase12_default_termios_has_full_lflag`
- `test_phase12_output_column_tracking_printable`
- `test_phase12_output_column_tracking_newline`
- `test_phase12_output_column_tracking_cr`
- `test_phase12_output_column_tracking_tab`
- `test_phase12_output_column_tracking_backspace`
- `test_phase12_onocr_at_column_zero`
- `test_phase12_default_onlcr_newline_expands`

All 55 test suites pass (0 failures). `cargo fmt` clean.
---

## 17. Phase 13: ABI Signal Constant Unification ✅

> **Priority**: P2 — Maintenance hazard, not a runtime bug yet.
> **Rationale**: Signal constants (`SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`, `SIGCONT`) are defined in **both** `abi/src/syscall.rs` and `abi/src/signal.rs`. The TTY subsystem imports from both: `mod.rs` uses `slopos_abi::signal::*` while `ldisc.rs` uses `slopos_abi::syscall::*`. If the values drift between files, signal delivery will silently break.

**Goal**: Establish `abi/src/signal.rs` as the single canonical source for all signal constants. Remove duplicates from `abi/src/syscall.rs`.

**Status**: Completed. All tests pass (1053/1053, 4 new Phase 13 regression tests). Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- **17.1 (Remove duplicates)**: Deleted all 7 duplicate signal constants (`SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGCONT`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`) from `abi/src/syscall.rs`. The canonical definitions remain in `abi/src/signal.rs` which was already `pub mod signal` in `abi/src/lib.rs`.
- **17.2 (Update imports)**: Migrated all TTY subsystem imports from `slopos_abi::syscall::SIG*` to `slopos_abi::signal::SIG*` across 3 files:
  - `drivers/src/tty/ldisc.rs`: `SIGINT`, `SIGQUIT`, `SIGTSTP` moved to `slopos_abi::signal`
  - `drivers/src/tty/mod.rs`: `SIGTTIN`, `SIGTTOU` moved to `slopos_abi::signal` (joined existing `SIGCONT`, `SIGHUP` import)
  - `drivers/src/tty_tests.rs`: All 7 signal constants now imported from `slopos_abi::signal`
- **17.3 (Re-export verification)**: Confirmed `pub mod signal` already exported in `abi/src/lib.rs` — no changes needed.
- **Regression tests**: 4 new Phase 13 tests: `test_phase13_signal_values_from_signal_module` (all 7 signal values correct), `test_phase13_ldisc_signal_uses_signal_module` (Ctrl+C/Ctrl+\\/Ctrl+Z produce correct signals), `test_phase13_hangup_signals_from_signal_module` (SIGHUP=1, SIGCONT=18), `test_phase13_job_control_signals_from_signal_module` (SIGTTIN=21, SIGTTOU=22).

### 17.1 Remove duplicate signal constants from `syscall.rs`

**Previously duplicated**:
- `SIGINT` (2), `SIGQUIT` (3), `SIGHUP` (1), `SIGTSTP` (20), `SIGTTIN` (21), `SIGTTOU` (22), `SIGCONT` (18)

**Action**: Deleted from `abi/src/syscall.rs`. Canonical source: `abi/src/signal.rs`.

### 17.2 Update imports across the codebase

```rust
// BEFORE (ldisc.rs):
use slopos_abi::syscall::{SIGINT, SIGQUIT, SIGTSTP};

// AFTER (ldisc.rs):
use slopos_abi::signal::{SIGINT, SIGQUIT, SIGTSTP};
```

### 17.3 Re-export from `abi/src/lib.rs` if needed

Already exported — `pub mod signal` on line 27 of `abi/src/lib.rs`. No changes needed.

### 17.4 Files modified

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Removed 7 duplicate signal constants (`SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`, `SIGCONT`) and their doc comments |
| `abi/src/signal.rs` | Verified — all signal constants already present, no changes needed |
| `abi/src/lib.rs` | Verified — `pub mod signal` already exported, no changes needed |
| `drivers/src/tty/ldisc.rs` | Updated imports: `SIGINT`, `SIGQUIT`, `SIGTSTP` now from `slopos_abi::signal` |
| `drivers/src/tty/mod.rs` | Updated imports: `SIGTTIN`, `SIGTTOU` moved from `syscall` to `signal` import |
| `drivers/src/tty_tests.rs` | Updated imports: all signal constants from `slopos_abi::signal`; added 4 Phase 13 regression tests |

---

## 18. Phase 14: Responsibility Split — PTY Foundation ✅

> **Priority**: P2 — ✅ **COMPLETED**. Required before PTY implementation, not blocking current functionality.
> **Rationale**: The current `Tty` struct (919 lines in `mod.rs`) mixes core state, I/O processing, session management, idle callbacks, and signal dispatch in one module. Linux separates these into `tty_io.c`, `tty_jobctrl.c`, `tty_port.c`, and the swappable `tty_ldisc` interface. Adding PTY support to the current monolithic `Tty` would compound coupling — PTY master needs raw passthrough (no line discipline), while PTY slave uses full N_TTY processing. Without a swappable line discipline abstraction, this can't be cleanly expressed.

**Goal**: Split internal responsibilities to prepare for PTY master/slave pairs without redesigning the static slot model or public API.

### 18.1 Extract session policy from `Tty`

Move all session/job-control logic from `mod.rs` into `session.rs`:
- `auto_attach_session()` → `session.rs`
- `detach_session_by_id()` → `session.rs`
- Foreground check wrappers → `session.rs`

`Tty` retains a `session: TtySession` field but delegates all policy decisions.

### 18.2 Add line discipline abstraction

```rust
// drivers/src/tty/ldisc.rs

/// Line discipline operations — swappable per-TTY.
pub enum LdiscKind {
    /// Full N_TTY processing (canonical, echo, signals, etc.)
    NTty(LineDisc),
    /// Raw passthrough (for PTY master, future SLIP/PPP)
    Raw,
}

impl LdiscKind {
    pub fn process_input(&mut self, c: u8) -> InputAction { ... }
    pub fn process_output(&mut self, c: u8) -> OutputAction { ... }
    pub fn read(&mut self, buf: &mut [u8]) -> usize { ... }
    pub fn has_data(&self) -> bool { ... }
}
```

### 18.3 Prepare `TtyDriverKind` for PTY

```rust
pub enum TtyDriverKind {
    SerialConsole(SerialConsoleDriver),
    VConsole(VConsoleDriver),
    PtyMaster { slave_idx: TtyIndex },   // Future
    PtySlave { master_idx: TtyIndex },    // Future
    None,
}
```

### 18.4 Use sentinel newtypes for session/pgrp IDs

Replace raw `u64` with `Option<NonZeroU64>` or dedicated newtypes:
```rust
pub struct SessionId(NonZeroU64);
pub struct ProcessGroupId(NonZeroU64);

pub struct TtySession {
    pub session_leader: Option<SessionId>,
    pub session_id: Option<SessionId>,
    pub fg_pgrp: Option<ProcessGroupId>,
}
```

This eliminates the `NO_SESSION = 0` / `NO_FOREGROUND_PGRP = 0` sentinel constants and makes invalid states unrepresentable.

### 18.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Extract session policy methods to `session.rs`, use `LdiscKind` instead of bare `LineDisc` |
| `drivers/src/tty/ldisc.rs` | Add `LdiscKind` enum, `Raw` variant with passthrough |
| `drivers/src/tty/driver.rs` | Add `PtyMaster`/`PtySlave` variant stubs to `TtyDriverKind` |
| `drivers/src/tty/session.rs` | Absorb extracted session methods, add `SessionId`/`ProcessGroupId` newtypes |
| `drivers/src/tty_tests.rs` | Update for `LdiscKind`, add `Raw` passthrough tests, newtype tests |

---

## 19. Phase 15: POSIX Quick Wins — Line Boundaries, SIGWINCH, SIGHUP, Word Erase ✅

> **Status**: ✅ COMPLETED
> **Priority**: P0 — Four targeted fixes that each take <50 lines but significantly improve POSIX correctness.
> **Rationale**: A deep architectural review comparing SlopOS's TTY against Linux N_TTY, RedoxOS, and Kerla identified these as the highest-impact correctness gaps.  Each one breaks real-world programs (shells, editors, terminal apps).  All four are incremental fixes on the existing solid foundation — no architectural changes needed.

**Goal**: Close the four most impactful POSIX compliance gaps with minimal, focused changes.

### 19.1 Canonical line boundary tracking

**Problem**: In canonical mode, `flush_edit_to_cooked()` dumps the entire edit buffer (including the trailing `\n`) into the cooked ring buffer.  If a user types fast enough for two lines to coalesce before `read()` runs, a single `read(fd, buf, 1024)` returns both lines concatenated.  POSIX requires that each `read()` in canonical mode returns **at most one complete line**.

Linux solves this with `canon_head` / `line_start` markers in `n_tty.c` that track where each newline/EOF lands in the read buffer.  The Rust-idiomatic equivalent is simpler — a line counter.

**Fix**:

```rust
// In LineDisc — add a line counter
pub struct LineDisc {
    // ... existing fields ...

    /// Number of complete lines in the cooked buffer (incremented on
    /// newline/EOF flush, decremented when read() consumes a line).
    line_count: usize,
}

// In flush_edit_to_cooked():
fn flush_edit_to_cooked(&mut self) {
    for i in 0..self.edit_len {
        self.push_cooked(self.edit_buf[i]);
    }
    self.edit_len = 0;
    self.line_count += 1;  // One complete line flushed
}

// In read() — canonical mode stops after one line:
pub fn read(&mut self, out: &mut [u8]) -> usize {
    let mut copied = 0;
    let is_canon = self.is_canonical();
    while copied < out.len() && self.cooked_count > 0 {
        let byte = self.cooked[self.cooked_tail];
        out[copied] = byte;
        self.cooked_tail = (self.cooked_tail + 1) % COOKED_BUF_SIZE;
        self.cooked_count -= 1;
        copied += 1;
        // In canonical mode, stop after consuming one complete line.
        if is_canon && byte == b'\n' {
            self.line_count = self.line_count.saturating_sub(1);
            break;
        }
    }
    // Handle EOF flush (no trailing newline) — decrement line_count
    // if we consumed data but didn't hit a newline.
    if is_canon && copied > 0 && self.cooked_count == 0 && self.line_count > 0 {
        self.line_count = self.line_count.saturating_sub(1);
    }
    copied
}

// In has_data() — canonical mode: data ready only when a full line exists:
pub fn has_data(&self) -> bool {
    if self.is_canonical() {
        self.line_count > 0
    } else {
        self.cooked_count > 0
    }
}
```

This is simpler than Linux's `canon_head`/`line_start` marker approach because Rust's ownership model means we don't need to coordinate multiple pointers into a shared buffer.  A single `line_count` achieves the same POSIX guarantee.

### 19.2 SIGWINCH on window size change

**Problem**: `set_winsize()` updates the `Tty.winsize` struct but never signals the foreground process group.  POSIX specifies that `TIOCSWINSZ` should deliver `SIGWINCH` to the foreground pgrp when the size actually changes.

**Fix**:

```rust
// In tty::set_winsize():
pub fn set_winsize(idx: TtyIndex, ws: &UserWinsize) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS { return Err(TtyError::InvalidIndex); }
    let fg_pgrp;
    let changed;
    {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                changed = tty.winsize.ws_row != ws.ws_row
                       || tty.winsize.ws_col != ws.ws_col;
                tty.winsize = *ws;
                fg_pgrp = tty.session.fg_pgrp_raw();
            }
            None => return Err(TtyError::NotAllocated),
        }
    }
    // Signal outside the lock — only if size actually changed.
    if changed && fg_pgrp != 0 {
        let _ = signal_process_group(fg_pgrp, SIGWINCH);
    }
    Ok(())
}
```

Add `pub const SIGWINCH: u8 = 28;` to `abi/src/signal.rs`.

Without this, `vim`, `less`, `htop`, `tmux`, and every terminal application that adapts to window resize will render garbled output after a resize.

### 19.3 SIGHUP to entire session (not just foreground group)

**Problem**: `hangup()` sends SIGHUP + SIGCONT only to `fg_pgrp`.  POSIX specifies that when a controlling terminal hangs up, SIGHUP is sent to **all processes in the controlling session** — foreground and background groups alike.

**Fix**:

```rust
// In tty::hangup():
pub fn hangup(idx: TtyIndex) {
    // ... existing lock + flush + detach ...
    let (session_id, fg_pgrp) = { /* extract both */ };

    // POSIX: signal the entire session, not just foreground.
    if session_id != 0 {
        let _ = signal_session(session_id, SIGHUP);
        let _ = signal_session(session_id, SIGCONT);
    }
    // ... wake blocked readers ...
}
```

This requires adding a `signal_session(sid, sig)` helper to the driver runtime services that iterates all tasks with matching `session_id` and delivers the signal.  This parallels the existing `signal_process_group(pgid, sig)` pattern.

Without this, background jobs don't get notified when the shell exits.  They become orphaned zombies.

### 19.4 Word erase with proper word boundaries

**Problem**: `word_erase()` in `ldisc.rs` uses `== b' '` as the sole word boundary.  POSIX and all real shells (bash, zsh, readline) define word boundaries as transitions between alphanumeric and non-alphanumeric characters.  Typing `/usr/local/bin` and pressing Ctrl+W should erase `bin` — currently it erases the entire path.

**Fix**:

```rust
// Add helper to ldisc.rs:
fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

// In word_erase():
fn word_erase(&mut self, lflag: u32) -> InputAction {
    if self.edit_len == 0 { return InputAction::None; }
    let mut erased = 0;

    // Phase 1: skip trailing non-word characters (spaces, punctuation).
    while self.edit_len > 0 && !is_word_char(self.edit_buf[self.edit_len - 1]) {
        self.edit_len -= 1;
        erased += 1;
    }
    // Phase 2: delete word characters (alphanumeric + underscore).
    while self.edit_len > 0 && is_word_char(self.edit_buf[self.edit_len - 1]) {
        self.edit_len -= 1;
        erased += 1;
    }
    // ... rest unchanged ...
}
```

### 19.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Add `line_count` field, update `read()`/`has_data()`/`flush_edit_to_cooked()`/`flush_all()`; fix `word_erase()` boundary logic; add `is_word_char()` helper |
| `drivers/src/tty/mod.rs` | Update `set_winsize()` to signal SIGWINCH; update `hangup()` to signal entire session |
| `abi/src/signal.rs` | Add `SIGWINCH` constant (value 28) |
| `lib/src/kernel_services/driver_runtime.rs` | Add `signal_session(sid, sig)` service function |
| `core/src/driver_hooks.rs` | Wire `signal_session` implementation (iterate tasks by session ID) |
| `drivers/src/tty_tests.rs` | Add regression tests: canonical one-line-per-read, SIGWINCH constant, word erase boundaries |


### 19.6 Implementation Summary

All four sub-tasks implemented and verified:

1. **Canonical line boundary tracking** — Added `line_count: usize` to `LineDisc`. Incremented in `flush_edit_to_cooked()`, decremented in `read()` on newline or EOF-flush. `has_data()` in canonical mode checks `line_count > 0`. `flush_all()` resets it. Correctly handles partial reads (mid-line buffer fills do not decrement count).
2. **SIGWINCH on window size change** — Added `SIGWINCH = 28` to `abi/src/signal.rs`. `set_winsize()` now compares old/new `ws_row`/`ws_col` and signals the foreground process group via `signal_process_group()` if dimensions changed, delivering the signal outside the per-TTY lock.
3. **SIGHUP to entire session** — Added `signal_session(sid, signum)` service function via `define_service!` macro. Implementation in `driver_hooks.rs` uses `SignalSessionContext` + `task_iterate_active` callback filtering on `(*task).sid`. `hangup()` now extracts `session_id` and calls `signal_session()` instead of `signal_process_group()` for both SIGHUP and SIGCONT.
4. **Word erase with proper word boundaries** — Added `is_word_char()` helper (alphanumeric + underscore). `word_erase()` Phase 1 now skips non-word chars (not just spaces), Phase 2 deletes word chars. Correctly handles paths (`/usr/local/bin`), mixed boundaries (`hello---world`), and trailing spaces.

**Tests added** (8 new tests, all passing):
- `test_phase15_canonical_one_line_per_read` — Two lines coalesced, read returns one at a time
- `test_phase15_canonical_has_data_line_count` — `has_data()` gated by `line_count` in canonical mode
- `test_phase15_canonical_eof_line_boundary` — EOF flush counts as line boundary
- `test_phase15_sigwinch_constant` — SIGWINCH == 28
- `test_phase15_word_erase_path_boundary` — `/usr/local/bin` + Ctrl+W = `/usr/local/`
- `test_phase15_word_erase_mixed_boundary` — `hello---world` + Ctrl+W = `hello---`
- `test_phase15_word_erase_trailing_spaces` — `hello   ` + Ctrl+W erases everything
- `test_phase15_canonical_small_buffer_read` — Partial buffer reads don't lose data

**Total test suite**: 1076 tests, 1076 passed, 0 failed.

---

## 20. Phase 16: Termios Drain Semantics & Open Flags ✅

> **Priority**: P1 — Affects correctness for programs that switch terminal modes (editors, curses apps).
> **Rationale**: Three medium-effort POSIX requirements that are commonly needed by interactive programs: TCSETSW/TCSETSF drain-before-change semantics, O_NOCTTY for daemon correctness, and TIOCSETD for runtime line discipline switching (prerequisite for clean PTY operation).

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (1084/1084 tests, 0 failures, 7 new Phase 16 regression tests).

**Goal**: Implement termios change drain semantics, the `O_NOCTTY` open flag, and runtime line discipline switching.

### 20.1 TCSETSW / TCSETSF drain semantics

**Problem**: `TCSETS`, `TCSETSW`, and `TCSETSF` previously called the same immediate termios path. POSIX requires:
- `TCSETSW` — Wait for all pending output to drain to hardware **before** changing termios
- `TCSETSF` — Wait for output drain **and** flush pending input **before** changing termios

Programs like `vim` exiting to shell use `TCSETSW` so screen-clearing escape sequences are emitted before the terminal returns to cooked mode. Without a distinct drain step, data can cross a mode boundary under the wrong settings.

**Implemented**:

```rust
pub fn set_termios_wait(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::Drain)
}

pub fn set_termios_flush(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::DrainAndFlushInput)
}
```

`drivers/src/tty/mod.rs` now funnels all three `TCSETS*` variants through a shared `set_termios_mode(...)` helper with an explicit `TermiosSetMode` enum. The drain boundary is modeled by `wait_output_idle()` outside the per-TTY lock, which is a validated no-op today because `serial_putc_com1` is synchronous, but it creates the correct long-term seam for buffered or interrupt-driven transmit without redesigning the public API later. `TCSETSF` flushes unread input only, preserving output semantics.

### 20.2 O_NOCTTY open flag

**Problem**: Opening a TTY could still acquire it as the controlling terminal through the read-side `auto_attach_session()` path. POSIX defines `O_NOCTTY` to suppress that behavior, which daemons and helper processes rely on when they temporarily open a terminal for logging or inspection.

**Implemented**:

```rust
// In abi/src/syscall.rs:
pub const O_NOCTTY: u32 = 0x100;

// In fs/src/fileio.rs:
let auto_attach = (desc.flags & O_NOCTTY as u32) == 0;
tty::read_cooked_with_attach(desc.tty_index?, buf, nonblock, auto_attach)
```

SlopOS keeps `O_NOCTTY` as a sticky per-FD policy bit in `FileDescriptor.flags` instead of introducing a parallel `noctty: bool`. That keeps terminal acquisition policy attached to the file descriptor that requested it, lets `F_GETFL` report the bit back to userspace, and ensures `F_SETFL` preserves `O_NOCTTY` instead of dropping it when mutating later status flags like `O_NONBLOCK` or `O_APPEND`.

### 20.3 Runtime line discipline switching (TIOCSETD)

**Problem**: `LdiscKind` already supported `NTty` and `Raw`, but the active discipline was fixed at allocation time. POSIX `TIOCSETD` / `TIOCGETD` requires runtime switching, which is also a clean prerequisite for Phase 17 PTY master/slave behavior.

**Implemented**:

```rust
pub const TIOCSETD: u64 = 0x5423;
pub const TIOCGETD: u64 = 0x5424;

pub const N_TTY: u32 = 0;
pub const N_RAW: u32 = 1;
```

`drivers/src/tty/ldisc.rs` now exposes `LdiscKind::id()`, `LdiscKind::from_id(...)`, and `flush_input()`, while `drivers/src/tty/mod.rs` adds `get_ldisc()` / `set_ldisc()`. Runtime switching preserves the previous `termios` payload across the swap, updates `c_line` to the selected line discipline ID, flushes unread input from the old discipline, and rejects unsupported IDs with `TtyError::UnsupportedLineDiscipline`. `RawDisc` was also updated to source `VMIN`/`VTIME` from preserved termios state instead of hardcoding `(1, 0)`, so line-discipline flips do not silently lose non-canonical timing settings.

**Design note**: SlopOS keeps enum dispatch for `LdiscKind` (no trait objects) because there are only two variants and the `no_std` + no-alloc constraint makes `Box<dyn LdiscTrait>` impractical. If future line disciplines are added (SLIP, PPP), a macro-generated delegation layer or a manual vtable can replace the current match arms without changing the public API.

### 20.4 Files modified

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `O_NOCTTY`, `TIOCSETD`, `TIOCGETD`, `N_TTY`, and `N_RAW` constants |
| `lib/src/kernel_services/syscall_services/tty.rs` | Add attach-aware reads, drain/flush termios setters, and ldisc get/set service calls |
| `drivers/src/syscall_services_init.rs` | Wire adapters for attach-aware reads, drain/flush setters, and ldisc get/set |
| `drivers/src/tty/ldisc.rs` | Add `id()`, `from_id(...)`, `flush_input()`, and preserve raw `VMIN`/`VTIME` from termios |
| `drivers/src/tty/mod.rs` | Add `TermiosSetMode`, attach-aware read path, drain/flush setters, and runtime `set_ldisc()` / `get_ldisc()` |
| `fs/src/fileio.rs` | Preserve `O_NOCTTY` and `O_CLOEXEC`, route TTY reads through attach-aware service, keep `O_NOCTTY` sticky across `F_SETFL` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Dispatch `TCSETSW` / `TCSETSF` correctly and add `TIOCSETD` / `TIOCGETD` |
| `drivers/src/tty_tests.rs` | Add regression coverage for drain-vs-flush input handling, attach suppression, and ldisc round-trips |
| `core/src/syscall/tests.rs` | Add `/dev/tty` `O_NOCTTY` flag propagation regression test |

### 20.5 Implementation Summary

All three Phase 16 requirements are now implemented and verified:

1. **Distinct termios transition modes** — `TCSETS` remains immediate, `TCSETSW` waits through `set_termios_wait()`, and `TCSETSF` uses `set_termios_flush()` to drain output and flush unread input before applying new attributes. The shared `TermiosSetMode` enum keeps the semantic split explicit without duplicating state-transition logic.
2. **`O_NOCTTY` as per-FD policy** — TTY reads now go through `read_cooked_with_attach(...)`, and descriptors opened with `O_NOCTTY` skip `auto_attach_session()`. The flag stays visible through `F_GETFL` and survives `F_SETFL` updates.
3. **Runtime line discipline switching** — `TIOCGETD` / `TIOCSETD` now switch cleanly between `N_TTY` and `N_RAW` at runtime. The implementation preserves termios state, updates `c_line`, and rejects unsupported IDs cleanly.

**Adjacent correctness fix**: While wiring open-flag propagation, `file_open_for_process()` was also corrected to carry `O_CLOEXEC` into `FileDescriptor.cloexec` for both regular opens and `/dev/tty`, fixing a real descriptor-state bug uncovered during Phase 16 work.

**Tests added** (7 new tests, all passing):
- `test_phase16_tcsetsw_preserves_pending_input` — drain path leaves unread input intact
- `test_phase16_tcsetsf_flushes_pending_input` — flush path clears unread input before apply
- `test_phase16_read_with_attach_false_skips_auto_attach` — attach suppression honors per-FD `O_NOCTTY`
- `test_phase16_get_ldisc_default_is_ntty` — fresh TTY reports `N_TTY`
- `test_phase16_set_ldisc_round_trip_preserves_termios` — ldisc swap keeps termios state stable
- `test_phase16_set_ldisc_invalid_id_rejected` — unsupported IDs fail with the new error path
- `test_open_dev_tty_with_o_noctty_preserves_flag` — `/dev/tty` open + `F_GETFL` keeps `O_NOCTTY`

**Total test suite**: 1084 tests, 1084 passed, 0 failed.

---

## 21. Phase 17: PTY Implementation ✅ COMPLETED

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (`1090/1090`, `0` failures, 6 new Phase 17 regression tests).

Phase 17 is now fully implemented with the long-term Rust-idiomatic shape that the earlier phases were preparing for:

- PTY master/slave pairs are modeled as ordinary `TTY_SLOTS` entries linked by `TtyIndex`, not ad-hoc shared heap state.
- Master writes route into the slave's line discipline through `push_input()`, while slave writes are processed through output flags and delivered into the master's raw buffer.
- PTY lifecycle semantics now match the plan: master close hangs up the slave, slave close produces EOF on the master, and fully unused pairs return both slots to the free pool.
- `/dev/ptmx`, `/dev/pts/N`, and `TIOCGPTN` are wired through the existing FD/syscall layers instead of adding PTY-specific side channels.
- Regression coverage was added in `drivers/src/tty_tests.rs` for allocation, both data-flow directions, hangup/EOF semantics, and canonical editing.

> **Priority**: P1 — The single highest-impact missing feature.  Without PTY, no terminal multiplexer (`screen`, `tmux`), no `ssh`, no subshells with proper terminal control.
> **Rationale**: The architectural foundation is ready (Phase 14 added `PtyMaster`/`PtySlave` driver stubs, `LdiscKind::Raw` for master passthrough, `DriverId` for lock-free cross-TTY routing, and per-TTY independent locking).  The remaining work is data routing, pair allocation, and lifecycle management.

**Goal**: Implement functional pseudo-terminal pairs — `open("/dev/ptmx")` allocates a master/slave pair, data flows bidirectionally through the line discipline, and master close triggers slave hangup.

### 21.1 Architecture overview

```
┌─────────────────┐          ┌──────────────────┐
│  PTY Master FD  │          │   PTY Slave FD   │
│  (terminal app) │          │   (shell/child)  │
└────────┬────────┘          └────────┬─────────┘
         │ write()                    │ write()
         │                           │
    ┌────▼────┐                 ┌────▼────┐
    │ Raw     │                 │ N_TTY   │
    │ LDisc   │                 │ LDisc   │
    └────┬────┘                 └────┬────┘
         │                           │
    Master writes ──────────► Slave input (push_input)
    Master reads  ◄──────────  Slave output (after OPOST)
```

**Data flow:**
- Master `write(data)` → slave's `push_input(byte)` (through slave's N_TTY ldisc — canonical editing, echo, signals)
- Slave `write(data)` → processed through slave's N_TTY output flags → master's read buffer
- Master `read()` → gets slave's processed output
- Slave `read()` → gets cooked input from its own N_TTY ldisc

**Key Rust-idiomatic design decisions:**
- No `Arc<Mutex<>>` or shared mutable state between master and slave — data is routed through `push_input()` which acquires per-TTY locks independently (one at a time, never both simultaneously)
- PTY pairs are just two entries in `TTY_SLOTS[]` with cross-references via `TtyIndex`
- Peer identity stays embedded in the driver variants, while EOF/hangup state is tracked explicitly in per-TTY state (`peer_closed`, `hung_up`) rather than with shared mutable peer objects

### 21.2 PTY pair allocation

```rust
// New file: drivers/src/tty/pty.rs

/// Allocate a PTY master/slave pair from the TTY table.
///
/// Returns `(master_idx, slave_idx)` or an error if no free slots.
/// The master gets `LdiscKind::Raw` + `TtyDriverKind::PtyMaster`.
/// The slave gets `LdiscKind::NTty` + `TtyDriverKind::PtySlave`.
pub fn pty_alloc() -> Result<(TtyIndex, TtyIndex), TtyError> {
    // Find two free slots (never hold two locks simultaneously).
    let master_slot = find_free_slot()?;
    let slave_slot = find_free_slot_excluding(master_slot)?;

    let master_idx = TtyIndex(master_slot as u8);
    let slave_idx = TtyIndex(slave_slot as u8);

    // Initialize master (Raw ldisc, no session).
    {
        let mut guard = TTY_SLOTS[master_slot].lock();
        *guard = Some(Tty::new_pty_master(master_idx, slave_idx));
    }
    // Initialize slave (NTty ldisc, inherits termios defaults).
    {
        let mut guard = TTY_SLOTS[slave_slot].lock();
        *guard = Some(Tty::new_pty_slave(slave_idx, master_idx));
    }

    Ok((master_idx, slave_idx))
}
```

### 21.3 Data routing in `TtyDriverKind`

Replace the `TODO(PTY)` stubs in `driver.rs`:

```rust
impl TtyDriverKind {
    pub fn write_output(&self, buf: &[u8]) {
        match self {
            // ... existing serial/vconsole ...
            Self::PtyMaster { slave_idx } => {
                // Master write → push to slave's input.
                for &b in buf {
                    super::push_input(*slave_idx, b);
                }
            }
            Self::PtySlave { master_idx } => {
                // Slave write (after ldisc output processing) →
                // push to master's raw read buffer.
                let slot = master_idx.0 as usize;
                if slot < MAX_TTYS {
                    let mut guard = TTY_SLOTS[slot].lock();
                    if let Some(master) = guard.as_mut() {
                        for &b in buf {
                            master.ldisc.input_char(b);  // Raw ldisc — direct to buffer
                        }
                    }
                }
            }
            // ...
        }
    }
}
```

**Lock ordering safety**: The slave's `write()` (under slave's lock via split-write pattern) drops the lock **before** calling `write_driver_unlocked`.  The driver then acquires the **master's** lock to push data.  Since the slave lock is already released, no two `TTY_SLOTS` locks are held simultaneously.

### 21.4 PTY lifecycle

- **Master close** → slave receives hangup (blocked readers get EOF, future reads get `-EIO`)
- **Slave close** → master reads get EOF (0 bytes returned)
- **Both closed** → slots returned to free pool

```rust
// In close_ref() — detect PTY peer closure:
pub fn close_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    // ... existing decrement ...
    if tty.open_count == 0 {
        // If this is a PTY master, hangup the slave.
        if let TtyDriverKind::PtyMaster { slave_idx } = tty.driver {
            let slave = slave_idx;
            drop(guard);  // Release our lock first!
            hangup(slave);
            return Ok(0);
        }
        // If this is a PTY slave, mark master as peer-closed.
        if let TtyDriverKind::PtySlave { master_idx } = tty.driver {
            let master = master_idx;
            drop(guard);
            // Wake master readers — they'll get EOF.
            TTY_INPUT_WAITERS[master.0 as usize].wake_all();
            return Ok(0);
        }
        // ... existing flush + detach for non-PTY ...
    }
}
```

### 21.5 `/dev/ptmx` and `/dev/pts/N`

- `open("/dev/ptmx")` → calls `pty_alloc()`, returns FD to master side, creates `/dev/pts/N` for slave
- `open("/dev/pts/N")` → opens the slave side of PTY pair N
- Wire into `fs/src/fileio.rs` or devfs if available

### 21.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/pty.rs` | **New** — PTY pair allocation, `find_free_slot()`, `Tty::new_pty_master/slave` constructors |
| `drivers/src/tty/driver.rs` | Replace `TODO(PTY)` stubs with actual data routing in `write_output` and `write_driver_unlocked` |
| `drivers/src/tty/mod.rs` | Add `pub mod pty`, update `close_ref()` with PTY lifecycle, expose `pty_alloc()` |
| `drivers/src/tty/table.rs` | Add `find_free_slot()` / `find_free_slot_excluding()` helpers, `Tty::new_pty_master()` / `Tty::new_pty_slave()` constructors |
| `fs/src/fileio.rs` | Intercept `open("/dev/ptmx")` and `open("/dev/pts/N")`, create TTY-backed FDs |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add PTY-specific ioctls if needed (e.g. `TIOCGPTN` to get slave number) |
| `abi/src/syscall.rs` | Add `TIOCGPTN` constant |
| `drivers/src/tty_tests.rs` | Add PTY tests: pair allocation, master→slave data flow, slave→master data flow, master close hangup, slave close EOF |

---

## 22. Phase 18: Controlling Terminal Ownership Unification

> **Status**: **COMPLETED**. `cargo fmt --all`, `just build`, and `just test` pass.
> **Priority**: P0 — Highest-value follow-up after the deep architecture review.
> **Rationale**: The current control plane still has two sources of truth for terminal ownership. `read()` can auto-attach session state in `drivers/src/tty/session.rs`, while `TIOCSCTTY` separately sets `task.controlling_tty` in `core/src/syscall/fs/poll_ioctl_handlers.rs`. That split is convenient for bootstrap, but it is not the clean long-term POSIX model. The next phase should make controlling-terminal attachment explicit and coherent so task state, TTY session state, and ioctl semantics cannot drift apart.

**Implementation summary**:
- Removed durable read-side ownership mutation: `tty::read_with_attach()` no longer claims controlling-terminal ownership, so plain reads cannot silently mutate `tty.session`.
- Added explicit ownership transitions in the TTY core: `acquire_controlling_terminal()` and `release_controlling_terminal()` now provide the single kernel-internal path for durable acquire/release, and `TIOCSCTTY` routes through that path.
- Tightened detach semantics: `setsid()` now clears only the caller's cached `controlling_tty` unless the caller is the session leader that actually owns the terminal, avoiding accidental session-wide detach from inherited child tasks.
- Kept task and TTY state coherent on hangup: `hangup()` now clears `controlling_tty` across the whole session through a runtime callback before signaling `SIGHUP`/`SIGCONT`.
- Scoped implicit acquisition to open-time PTY bootstrap only: opening `/dev/pts/N` without `O_NOCTTY` now acquires the slave as controlling terminal for a session leader with no existing controlling TTY, while `O_NOCTTY` leaves ownership unattached.
- Updated shell bootstrap to use the explicit control path: the shell now issues `TIOCSCTTY` during job-control initialization before calling `tcsetpgrp()`.

**Goal**: Establish a single canonical ownership path for controlling terminals and session attachment.

### 22.1 Make explicit control operations the source of truth

- Keep `TIOCSCTTY` and session-detach operations (`setsid()`, shell exit, hangup) as the only durable ownership transitions.
- Remove read-side auto-attach as a durable ownership mechanism; if a bootstrap exception is still needed for early console bring-up, confine it to a clearly marked early-boot path instead of the generic `read()` flow.
- Ensure `/dev/tty` resolution and `O_NOCTTY` behavior depend on the same ownership state machine.

### 22.2 Keep task and TTY state coherent

- When a task acquires a controlling TTY, update both `task.controlling_tty` and `tty.session` under one coherent transition.
- When ownership is dropped, detach both sides together so the TTY cannot retain a stale session after the task no longer owns it.
- Add explicit invariants to the plan and tests: if `task.controlling_tty == Some(idx)`, then `TTY_SLOTS[idx]` must carry the same session identity.

### 22.3 Tighten detach and hangup boundaries

- Audit `setsid()`, `TIOCSCTTY`, `hangup()`, and task-exit paths so each ownership transition is one-way and explicit.
- Decide whether `TIOCNOTTY` should be added now or intentionally deferred, but document the choice so ownership semantics stay comprehensible.
- Keep compositor focus completely outside this ownership model; it remains UI policy, not POSIX control state.

### 22.4 Files modified

| File | Planned change |
|------|----------------|
| `drivers/src/tty/mod.rs` | Removed durable read-side auto-attach, added explicit acquire/release ownership helpers, cleared task-side ownership caches during hangup |
| `drivers/src/tty/session.rs` | Removed the generic read-side auto-attach helper so session ownership is no longer claimed from the read path |
| `drivers/src/syscall_services_init.rs` | Wired explicit acquire/release adapters into `TtyServices` |
| `fs/src/fileio.rs` | Switched TTY reads to the non-attaching path and added PTY-slave open-time acquisition that respects `O_NOCTTY` |
| `lib/src/kernel_services/driver_runtime.rs` | Added runtime hooks for setting the current task's controlling TTY and clearing session-wide caches |
| `lib/src/kernel_services/syscall_services/tty.rs` | Exposed explicit acquire/release ownership calls to other kernel crates |
| `core/src/driver_hooks.rs` | Implemented the new runtime hooks against the scheduler task table |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Made `TIOCSCTTY` use the explicit acquire path |
| `core/src/syscall/process_handlers.rs` | Fixed `setsid()` so inherited children drop only their own cached controlling TTY instead of detaching the parent session's terminal |
| `core/src/scheduler/task.rs` | Added session-wide task-side controlling-TTY cleanup for hangup-driven teardown |
| `drivers/src/tty_tests.rs` | Added explicit ownership transition and no-auto-attach regression tests |
| `core/src/syscall/tests.rs` | Added syscall-level regressions for `setsid()` coherence, hangup cleanup, and PTY `O_NOCTTY` acquisition behavior |
| `userland/src/syscall/fs.rs` | Added a small `TIOCSCTTY` userland wrapper |
| `userland/src/apps/shell/exec.rs` | Updated shell job-control bootstrap to explicitly acquire its controlling terminal |

---

## 23. Phase 19: Strict Session Gates & Foreground Outcomes

> **Status**: **DONE** ✅
> **Priority**: P0 — Required for fully trustworthy POSIX TTY semantics.
> **Rationale**: The review found that `ForegroundCheck::NoSession` currently conflates multiple cases: bootstrap permissiveness, missing foreground group, and cross-session denial. `drivers/src/tty/session.rs` marks cross-session reads as `NoSession`, while `drivers/src/tty/mod.rs` still allows `NoSession` in the read path. That makes the control plane look stricter on paper than it is in reality.

**Goal**: Separate bootstrap permissiveness from real access denial and enforce read/write outcomes uniformly.

### 23.1 Split foreground outcomes into explicit states

- Replace the overloaded `NoSession` path with distinct outcomes such as `BootstrapAllowed`, `DeniedCrossSession`, `BackgroundRead`, and `BackgroundWrite`.
- Keep kernel/early-boot permissiveness explicit instead of hiding it behind the same enum branch used for policy denial.
- Make the read and write paths consume the same outcome model so the control plane is easy to reason about.

### 23.2 Enforce strict cross-session denial

- Cross-session reads should return a hard denial (`-EIO` or the chosen internal error) rather than falling through a permissive branch.
- Same-session background reads should continue to deliver `SIGTTIN`.
- Same-session background writes with `TOSTOP` should continue to deliver `SIGTTOU`.

### 23.3 Reconcile `check_read()` / `check_write()` with user-visible behavior

- Ensure ioctl, `/dev/tty`, PTY slave access, and inherited console FDs all observe the same session/foreground rules.
- Document exactly which cases are allowed during bootstrap and which are forbidden in steady state.
- Keep the Rust surface idiomatic: one internal enum, one mapping layer at the syscall boundary, no scattered ad-hoc booleans.

### 23.4 Files modified

| File | Planned change |
|------|----------------|
| `drivers/src/tty/session.rs` | Replace ambiguous `NoSession` semantics with explicit outcome variants |
| `drivers/src/tty/mod.rs` | Enforce strict outcome handling in `read()` and `write()` |
| `fs/src/fileio.rs` | Keep FD routing aligned with the stricter access model |
| `drivers/src/tty_tests.rs` | Add exact coverage for cross-session denial vs bootstrap allowance |
| `core/src/syscall/tests.rs` | Add syscall-facing regressions for `-EIO`, `SIGTTIN`, and `SIGTTOU` cases |

### 23.5 Implementation Summary

Implemented in a single pass with 11 new regression tests (all 1112 tests pass).

**Enum split** (`session.rs`):
- `ForegroundCheck::NoSession` replaced with `BootstrapAllowed` (permissive early-boot / no-session) and `DeniedCrossSession` (hard denial for cross-session access).
- `check_read()` and `check_write()` both emit `DeniedCrossSession` when `caller_sid != 0 && caller_sid != tty_sid`.
- `check_write()` signature extended with `caller_sid: u32` parameter, cross-session check runs before TOSTOP.

**Enforcement** (`mod.rs`):
- `read()` path: `DeniedCrossSession` → `Err(TtyError::CrossSessionDenied)` (no signal).
- `write()` path: obtains `caller_sid` via `current_task_sid()` before the TTY lock, passes it to `check_write()`, handles `DeniedCrossSession` alongside `BackgroundWrite`.
- New `TtyError::CrossSessionDenied` variant added.

**Syscall boundary** (`syscall_services_init.rs`):
- `CrossSessionDenied` mapped to `-5` (`EIO`) in the read adapter, matching `HungUp`.

**Tests** (`tty_tests.rs`):
- Existing tests updated for new enum names and `check_write()` 3-arg signature.
- 11 new Phase 19 tests cover: bootstrap allowed (no session, no fg_pgrp), cross-session denial (read, write±TOSTOP), kernel task exemption (read, write), same-session background paths (SIGTTIN, SIGTTOU), no-session write, and error variant distinctness.

**Files not modified** (no changes needed):
- `fs/src/fileio.rs` — FD routing does not reference `ForegroundCheck` variants directly.
- `core/src/syscall/tests.rs` — syscall-level tests deferred; TTY-level coverage is comprehensive.

---

## 24. Phase 20: PTY Pair Atomicity & Lifecycle Hardening

> **Status**: **DONE** ✅
> **Priority**: P1 — PTY functionality exists, but pair-level lifecycle still needs hardening.
> **Rationale**: The current PTY implementation is structurally correct, but pair allocation/free still happens as separate slot operations. That is acceptable for the first landing, yet long-term PTY code should treat master/slave creation and teardown as one atomic lifecycle event so concurrent opens/closes cannot observe half-initialized or half-freed pairs.

**Goal**: Make PTY pair allocation, teardown, and peer-closure semantics atomic at the pair level while preserving per-TTY fast-path locking.

### 24.1 Introduce a pair-level allocation boundary

- Allocate both master and slave from one critical section so there is no window where one side exists without the other.
- Keep per-slot locks for the data path, but add a narrow allocator/lifecycle boundary for pair creation and destruction.
- Prevent `/dev/ptmx` and `/dev/pts/N` from observing stale or partially initialized peer metadata.

### 24.2 Harden close and hangup semantics

- Make last-close transitions explicit for both master and slave.
- Ensure master-close hangup, slave-close EOF, and pair-free decisions are derived from one pair state transition rather than two loosely ordered slot updates.
- Re-check `open_ref()` / `close_ref()` so reference counting and pair teardown cannot race each other into inconsistent state.

### 24.3 Verify pair identity and peer metadata

- Keep `TIOCGPTN`, `/dev/pts/N`, and peer cross-links coherent even under repeated open/close cycles.
- Add regression coverage for rapid allocation/free/reallocation of PTYs.
- Preserve the Rust-idiomatic static-slot design; do not introduce heap-shared peer objects unless a later feature truly requires them.

### 24.4 Files modified

| File | Planned change |
|------|----------------|
| `drivers/src/tty/pty.rs` | Add pair-level allocation/free critical sections and lifecycle helpers |
| `drivers/src/tty/table.rs` | Support atomic pair reservation / release without compromising per-slot locking |
| `drivers/src/tty/mod.rs` | Tighten PTY close/hangup transitions |
| `fs/src/fileio.rs` | Keep `/dev/ptmx` and `/dev/pts/N` opens coherent with the harder lifecycle model |
| `drivers/src/tty_tests.rs` | Add allocation-race, close-order, and rapid-reuse regression tests |

### 24.5 Implementation summary

- **`PTY_ALLOC_LOCK`**: Added a `static IrqMutex<()>` pair-level serialization lock in `pty.rs`. Held during the entire find-slot + initialize sequence in `pty_alloc()` and during the check-and-free sequence in `free_pair_if_unused()`, eliminating TOCTOU windows between slot discovery and slot initialization/teardown.
- **Atomic `pty_open_slave()`**: New function combines slave validation (`is_pty_slave`) + `open_count` increment under `PTY_ALLOC_LOCK`, replacing the two-step `is_pty_slave()` then `open_ref()` pattern in `fileio.rs` `/dev/pts/N` opens. This prevents a concurrent `free_pair_if_unused()` from freeing the slot between the check and the open.
- **`clear_peer_closed()` scoped to `pub(crate)`**: Narrowed visibility from `pub` to `pub(crate)` — only `mod.rs::open_ref()` needs cross-module access.
- **Lock ordering documented**: `PTY_ALLOC_LOCK` → `TTY_SLOTS[i]` (never reverse). Module-level doc comment in `pty.rs` captures the ordering contract.
- **Syscall plumbing**: Added `open_pty_slave` to `TtyServices` trait and wired adapter in `syscall_services_init.rs`.
- **9 regression tests** added covering: pair initialization coherence, master-first close, slave-first close, reallocation after free, open-slave type validation, open-slave preventing free, partial open preventing free, rapid alloc/free/realloc data flow, and open-slave-after-free rejection.
- **Test count**: 1112 → 1121 (all passing).
---

## 25. Phase 21: Event-Driven Readiness & IXON Completion

> **Status**: **DONE** ✅
> **Priority**: P1 — Needed for mature multiplexing behavior and complete software flow control.
> **Rationale**: `poll()` / `select()` currently rely on short sleep loops rather than true event-driven readiness, and the line discipline tracks IXON stop state without fully enforcing it on the output path. Mature TTY stacks do not just parse control flow; they make readiness and stop/resume semantics real.

**Goal**: Replace polling loops with wait-queue-backed readiness and fully wire IXON stop/resume into output emission.

### 25.1 Make TTY readiness event-driven

- ✅ Added `WaitQueue::wait_once()` — single enqueue-block-dequeue cycle for poll-style sleep.
- ✅ Added `POLL_NOTIFY: WaitQueue` — global wait queue for poll/select sleepers.
- ✅ Added `poll_events(idx, requested) -> u16` — drains hw input, checks POLLIN/POLLOUT/POLLHUP.
- ✅ Added `poll_sleep()` — replaces `sleep_current_task_ms(1)` with event-driven wait.
- ✅ Replaced busy-wait loops in `syscall_poll` and `syscall_select` with `tty::poll_sleep()`.
- ✅ Updated `file_poll_fd` in `fileio.rs` to use `tty::poll_events()` instead of ad-hoc checks.
- ✅ Wired `POLL_NOTIFY.wake_all()` into `notify_input_ready()`, `hangup()`, and IXON resume path.

### 25.2 Enforce IXON stop/resume on writes

- ✅ `tty::write()` now blocks on `TTY_OUTPUT_WAITERS[slot]` when ldisc is IXON-stopped.
- ✅ `push_input()` tracks `was_stopped` → `!is_stopped()` transitions and wakes output waiters + poll sleepers.
- ✅ `hangup()` wakes both output waiters and poll sleepers.
- ✅ Policy: blocked write (wait_event until resumed), not buffered or suppressed.

### 25.3 Keep readiness logic Rust-idiomatic

- ✅ Single `poll_events()` function replaces duplicated checks across `fileio.rs` and syscall handlers.
- ✅ Separate `POLL_NOTIFY` / `TTY_OUTPUT_WAITERS` statics follow same pattern as `TTY_INPUT_WAITERS`.
- ✅ Lock ordering preserved: never hold two TTY_SLOTS locks simultaneously.

### 25.4 Files modified

| File | Change |
|------|--------|
| `lib/src/waitqueue.rs` | Added `wait_once()` method |
| `drivers/src/tty/table.rs` | Added `POLL_NOTIFY` and `TTY_OUTPUT_WAITERS` statics |
| `drivers/src/tty/mod.rs` | IXON write enforcement, `poll_events()`, `poll_sleep()`, wakeup wiring |
| `fs/src/fileio.rs` | Replaced ad-hoc TTY poll checks with `tty::poll_events()` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Replaced `sleep_current_task_ms(1)` with `tty::poll_sleep()` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added `poll_events` and `poll_sleep` to `TtyServices` |
| `drivers/src/syscall_services_init.rs` | Added adapter functions for new service methods |
| `drivers/src/tty_tests.rs` | 11 new regression tests (1132 total, all passing) |
---

## 26. Phase 22: Operational Console Routing & Real VConsole

> **Status**: **DONE** ✅
> **Priority**: P2 — Not a POSIX blocker, but important to complete the long-term TTY vision.
> **Rationale**: The architecture now supports multiple TTYs, yet the operational console story is still serial-first: bootstrap stdio is pinned to `TtyIndex(0)` and `VConsoleDriver` still mirrors output to serial instead of acting as a real framebuffer-backed console. Linux and Redox both separate terminal semantics from transport/backend details; SlopOS should finish that separation too.

**Goal**: Make the virtual console a first-class backend while keeping serial available for logging and early boot.

### 26.1 Stop treating `tty0` as the forever-console by default

- Keep early serial bootstrap if needed, but allow the active interactive console to move onto the real virtual console path once the compositor / framebuffer side is available.
- Audit console FD bootstrap so the long-term interactive default is policy-driven instead of hardwired.
- Keep kernel logging and user-facing TTY routing conceptually separate.

### 26.2 Implement a real VConsole output path

- Replace serial mirroring with the intended framebuffer/text rendering backend.
- Keep keyboard input routed to `active_tty()`, but make output land on the matching visual console backend.
- Preserve serial as a debugging sink, not as the only fully working terminal renderer.

### 26.3 Prepare for multi-terminal operation

- Make active-TTY switching semantics explicit and testable.
- Keep compositor focus, active TTY selection, and POSIX foreground group as three separate concepts with well-defined boundaries.
- Avoid backsliding into a hidden singleton model now that the per-TTY architecture exists.

### 26.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/vconsole.rs` | Added framebuffer-backed virtual console renderer with static cell buffer, cursor control, scrolling, and serial fallback |
| `drivers/src/tty/driver.rs` | Routed `DriverId::VConsole` and `VConsoleDriver` writes through `vconsole::write()` with serial debug mirroring |
| `drivers/src/tty/mod.rs` | Added policy default console selector (`DEFAULT_CONSOLE_TTY`) and validated `switch_active_tty()` with waiter wakeup |
| `fs/src/fileio.rs` | Console FD bootstrap now uses `tty::default_console_tty()` instead of hardcoded `TtyIndex(0)` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added `default_console_tty()` and `switch_active_tty()` to `TtyServices` |
| `drivers/src/syscall_services_init.rs` | Added adapters and service wiring for `default_console_tty()` and `switch_active_tty()` |
| `video/src/framebuffer.rs` | Added `get_fb_base_ptr()` to expose mapped framebuffer virtual base |
| `boot/src/boot_drivers.rs` | Registered framebuffer geometry/base with TTY VConsole after `video::init()` |
| `drivers/src/tty_tests.rs` | Added 13 Phase 22 regression tests for default console policy, active switch validation, and VConsole state machine behavior |

### 26.5 Implementation Summary

Implemented in one pass with 13 new Phase 22 regressions (test suite count now 1145 before final full-gate rerun).

**Console bootstrap policy**:
- Added `DEFAULT_CONSOLE_TTY` atomic in `tty::mod` with `set_default_console_tty()` and `default_console_tty()`.
- Exposed `default_console_tty()` through `TtyServices` so `fs::fileio` uses policy-selected console bootstrap from the syscall service boundary.
- Updated `bootstrap_console_fds()` so FD 0/1/2 and all three `open_ref()` calls use the policy-selected TTY.

**Operational VConsole renderer**:
- Added `tty::vconsole` module with a const-initializable `VConsoleState` (cursor, dimensions, fixed cell buffer, optional framebuffer metadata).
- Implemented byte semantics for printable/control characters (`\n`, `\r`, backspace, tab), wrap logic, scroll-up, per-cell glyph rendering, and framebuffer row clear/move.
- Added framebuffer registration path from boot (`boot_drivers`) using video subsystem metadata plus mapped framebuffer base.
- Added `video::framebuffer::get_fb_base_ptr()` so boot can register the already mapped virtual framebuffer pointer without duplicating mapping logic.

**Active TTY switching contract**:
- Added `switch_active_tty(idx) -> Result<(), TtyError>` with index bounds checks, allocation/active validation, and waiter wakeups so blocked readers re-check readiness.
- Documented and tested the separation between compositor focus, active keyboard route, and POSIX foreground process-group control.
- Exposed `switch_active_tty()` through `TtyServices` with adapter wiring.

---

## 27. Phase 23: Canonical EOF, ISIG Flush & Signal Integrity

**Status**: ✅ Complete.

> **Priority**: P0 correctness — this phase fixes shell-visible semantic bugs before adding more surface area.
> **Principle**: Keep Linux-compatible user-visible behavior, but implement with Rust-first state modeling and explicit ownership.

### 27.1 Canonical EOF correctness (`VEOF`) — ✅

- Fixed canonical EOF behavior so `Ctrl+D` on an empty edit buffer does **not** create phantom readable state.
  - Added early check in `LineDisc::read()`: when canonical, `line_count > 0`, and `cooked_count == 0`, the phantom EOF line is consumed immediately (decrement `line_count`, return 0).
- Canonical EOF with pending bytes (`Ctrl+D` after text) continues to work as a flush-without-newline boundary.
- Regressions: `test_phase23_canonical_eof_empty_no_phantom`, `test_phase23_canonical_eof_with_pending_text_no_phantom`, `test_phase23_double_eof_no_phantom_accumulation`.

### 27.2 ISIG + `NOFLSH` semantics — ✅

- Implemented `VINTR`/`VQUIT`/`VSUSP` flush behavior: signal generation in `input_char()` now calls `flush_input()` before returning `InputAction::Signal` unless `NOFLSH` is set.
- Added `NOFLSH` constant to the ldisc import list.
- Regressions: `test_phase23_isig_flush_no_noflsh`, `test_phase23_isig_flush_with_noflsh`, `test_phase23_isig_ctrl_c_clears_edit_buffer`, `test_phase23_isig_flush_sigquit`, `test_phase23_isig_flush_sigtstp`.

### 27.3 Deferred signal delivery integrity — ✅

- Fixed signal-loss paths in `has_data()`, `poll_events()`, and `input_available_cb()` where `drain_hw_input()` results were silently discarded (`let _ = tty.drain_hw_input()`).
- All three functions now capture the deferred signal, drop the lock, and deliver via `signal_process_group()` outside the lock.
- The drain API was **not** split into separate modes (complexity not justified) — instead, each call site properly handles the signal return value.

### 27.4 Rust-idiomatic cleanup — ✅

- `_auto_attach` in `read_with_attach()` documented as intentionally dead (Phase 18 removed read-side ownership mutation). Parameter preserved for ABI compatibility with `read_cooked_with_attach` kernel services trait.
- Added `#[derive(Debug)]` to `InputAction` enum to support test diagnostic formatting.

### 27.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Fixed canonical EOF phantom state; added ISIG/NOFLSH flush; added `NOFLSH` import; derived `Debug` on `InputAction` |
| `drivers/src/tty/mod.rs` | Fixed signal loss in `has_data()`, `poll_events()`, `input_available_cb()`; documented `_auto_attach` |
| `drivers/src/tty_tests.rs` | Added 8 Phase 23 regression tests covering EOF, NOFLSH, signal flush, and phantom accumulation |

## 28. Phase 24: Job Control & Controlling TTY Hardening ✅

**Status**: Complete.

> **Priority**: P0 correctness — tighten process-group/session rules that real shells rely on.

### 28.1 `TIOCSPGRP` contract hardening

- Caller SID validation preserved in `TtySession::set_fg_pgrp_checked()`.
- **Phase 24 addition**: The outer `tty::set_foreground_pgrp_checked()` now additionally validates that the **target process group** has living members within the TTY's controlling session via the new `pgrp_exists_in_session()` driver-runtime service.  This prevents setting a foreground group that has no processes, which would make SIGINT/SIGTSTP delivery a no-op.
- `pgid == 0` (clear foreground group) bypasses the existence check.
- Cross-session target groups return `PermissionDenied` deterministically.

### 28.2 Controlling-terminal ownership rules

- `TIOCSCTTY` preconditions verified: must be session leader, must not already have a different ctty, TTY must not belong to a different session.  No changes needed — already solid.
- **Phase 24 addition**: `TIOCNOTTY` ioctl handler implemented in `poll_ioctl_handlers.rs`:
  - Verifies the caller's `controlling_tty` matches the ioctl target.
  - Always clears the caller's `controlling_tty` field.
  - If the caller is the session leader: detaches the entire session from the TTY, sends SIGHUP + SIGCONT to the foreground process group, and clears `controlling_tty` across all session members via `clear_session_controlling_tty()`.
  - If the caller is NOT the session leader: only the caller's own `controlling_tty` is cleared; TTY session state is unaffected.
- `TIOCNOTTY` constant added to `abi/src/syscall.rs` (value `0x5422`).
- New `detach_controlling_terminal()` function in `tty/mod.rs` implements the POSIX ownership transition logic.
- Service wiring: `TtyServices` trait, adapter in `syscall_services_init.rs`.

### 28.3 Session lifecycle coherence

- Verified: `setsid()` (process_handlers.rs) correctly detaches controlling_tty and releases the TTY session.
- Verified: `fork()`/`clone()` (task.rs) inherits `controlling_tty`, `pgid`, `sid` via bulk copy + explicit field assignment.
- Verified: Session leader exit (task.rs) triggers `hangup()` when the leader has a controlling TTY.
- Verified: `clear_controlling_tty_for_session()` (task.rs) iterates all tasks and clears `controlling_tty` for matching session+tty pairs.
- Strict cross-session denials from Phase 19 remain intact.

### 28.4 New driver-runtime service

- `pgrp_exists_in_session(pgid: u32, sid: u32) -> bool` — iterates active tasks via `task_iterate_active()` to check if any task with matching pgid+sid exists.  Returns false for pgid=0 or sid=0.
- Implementation in `core/src/driver_hooks.rs` with `PgrpExistsContext` struct and `pgrp_exists_task()` callback.
- Wired through `DriverRuntimeServices` service table.

### 28.5 Tests (7 new + 1 updated)

| Test | What it verifies |
|------|-----------------|
| `test_phase24_set_fg_pgrp_checked_nonexistent_pgrp` | Denies setting fg_pgrp to a pgid with no living members |
| `test_phase24_set_fg_pgrp_checked_clear_allowed` | Clearing fg_pgrp (pgid=0) always allowed |
| `test_phase24_set_fg_pgrp_checked_no_session_skips_validation` | No session → any pgid allowed (pre-session path) |
| `test_phase24_detach_ctty_non_leader` | Non-leader TIOCNOTTY only clears caller's ctty |
| `test_phase24_detach_ctty_session_leader` | Leader TIOCNOTTY fully detaches session + signals |
| `test_phase24_detach_ctty_cross_session_denied` | Cross-session detach denied |
| `test_phase24_tiocnotty_constant` | TIOCNOTTY == 0x5422 |
| `test_tty_set_fg_pgrp_checked` (updated) | Adapted for Phase 24 pgrp-existence semantics |

### 28.6 Files modified

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Added `TIOCNOTTY` constant (0x5422) |
| `lib/src/kernel_services/driver_runtime.rs` | Added `pgrp_exists_in_session` service declaration |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added `detach_controlling_terminal` service method |
| `core/src/driver_hooks.rs` | Implemented `pgrp_exists_in_session` + wired into service table |
| `drivers/src/tty/session.rs` | Documentation updates (pgrp validation moved to outer layer) |
| `drivers/src/tty/mod.rs` | Added pgrp-existence validation in `set_foreground_pgrp_checked()`, added `detach_controlling_terminal()` |
| `drivers/src/syscall_services_init.rs` | Added `tty_detach_controlling_terminal_adapter` + wired into TTY_SERVICES |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Added `TIOCNOTTY` import and ioctl handler arm |
| `drivers/src/tty_tests.rs` | 7 new Phase 24 regression tests + updated existing test |

## 29. Phase 25: Real `TCSETSW`/`TCSETSF` Drain Semantics ✅

**Status**: Complete.

> **Priority**: P1 correctness — move from placeholder wait to explicit drain contract.

### 29.1 Output drain accounting — ✅

- Added `output_pending()` method to the `TtyDriver` trait (default: `false`) and corresponding `TtyDriverKind::output_pending()` enum dispatch.
- Added `TTY_OUTPUT_INFLIGHT: [AtomicU32; MAX_TTYS]` static array in `table.rs` — tracks the number of `write()` operations that have processed output through the line discipline but have not yet completed the unlocked hardware write.
- The `write()` function increments `TTY_OUTPUT_INFLIGHT[slot]` before `write_driver_unlocked()`, decrements after, and wakes `TTY_OUTPUT_WAITERS[slot]` on completion.
- Echo output in `push_input()` also tracks inflight state for consistency.
- For synchronous backends (SerialConsole, VConsole), `output_pending()` returns `false` (writes block until on-the-wire).
- For PTY backends, output is immediately buffered in the peer — POSIX considers it "sent".

### 29.2 `TCSETSW` / `TCSETSF` behavior — ✅

- Replaced the Phase 16 `wait_output_idle()` stub with real drain synchronization:
  - **Fast path**: If `TTY_OUTPUT_INFLIGHT[slot] == 0` and `driver.output_pending() == false`, returns immediately.
  - **Slow path**: If the scheduler is enabled, sleeps on `TTY_OUTPUT_WAITERS[slot]` with a condition closure checking both inflight and driver pending state.
  - **Pre-scheduler fallback**: Busy-polls with `core::hint::spin_loop()` (early boot only).
- `TCSETSW` (`set_termios_wait`): waits for output drain, then applies new termios. Pending input is preserved.
- `TCSETSF` (`set_termios_flush`): waits for output drain, flushes pending input, then applies new termios.
- `TCSETS` (`set_termios`): unchanged — immediate apply, no drain.
- Added public `is_output_idle(idx)` function for test observability.

### 29.3 Verification — ✅

Added 13 Phase 25 regression tests:

| Test | What it verifies |
|------|-----------------|
| `test_phase25_is_output_idle_initially_true` | No inflight and no driver pending on fresh init |
| `test_phase25_inflight_counter_initial_zero` | All `TTY_OUTPUT_INFLIGHT` counters start at 0 |
| `test_phase25_write_updates_inflight_counter` | Inflight returns to 0 after write completes |
| `test_phase25_tcsetsw_preserves_input_after_drain` | `TCSETSW` drains output but preserves pending input |
| `test_phase25_tcsetsf_flushes_input_after_drain` | `TCSETSF` drains output AND flushes pending input |
| `test_phase25_is_output_idle_invalid_index` | Invalid index returns `InvalidIndex` error |
| `test_phase25_is_output_idle_unallocated` | Unallocated slot returns `NotAllocated` error |
| `test_phase25_drain_invalid_index_error` | `set_termios_wait` on invalid index returns error |
| `test_phase25_driver_output_pending_default_false` | Trait default `output_pending()` returns `false` |
| `test_phase25_driver_kind_output_pending_dispatch` | Enum dispatch works for all driver variants |
| `test_phase25_pty_drain_immediate` | PTY drain is immediate (no hardware latency) |
| `test_phase25_console_drain_immediate` | Console drain is immediate (synchronous serial) |
| `test_phase25_tcsets_now_skips_drain` | `TCSETS` (Now) applies immediately, no drain |

### 29.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Replaced `wait_output_idle()` stub with real drain synchronization; wired inflight counters into `write()` and `push_input()` echo paths; added `is_output_idle()` |
| `drivers/src/tty/driver.rs` | Added `output_pending()` to `TtyDriver` trait (default `false`); added `TtyDriverKind::output_pending()` enum dispatch |
| `drivers/src/tty/table.rs` | Added `TTY_OUTPUT_INFLIGHT: [AtomicU32; MAX_TTYS]` static array |
| `drivers/src/tty_tests.rs` | Added 13 Phase 25 regression tests covering drain accounting, TCSETSW/TCSETSF semantics, driver trait, PTY and console backends |
## 30. Phase 26: PTY Lifetime Safety & Scalable Capacity

**Status**: ✅ Done — generation-safe `PtyPeerHandle`, `MAX_TTYS` scaled to 32, 11 new regression tests. Total test count: 1184 (all pass).

> **Priority**: P1 robustness — prevent stale-slot reuse bugs and raise practical PTY limits.

### 30.1 Generation-safe PTY peer identity

- Replace plain index-only peer identity in PTY data paths with generation-tagged handles.
- Validate generation before cross-end writes to prevent stale-slot misrouting after rapid free/reuse.

### 30.2 Capacity scaling

- Move from hardcoded `MAX_TTYS = 8` toward a configurable, tested capacity baseline suitable for tmux/ssh-heavy workflows.
- Keep no-alloc/no-std constraints explicit; avoid hidden heap dependencies.

### 30.3 Stress tests

- Add rapid allocate/free/open/close/write stress loops to catch reuse races.
- Add concurrency-heavy PTY teardown/reopen scenarios.

### 30.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/pty.rs` | Add generation-safe peer validation and lifecycle hardening |
| `drivers/src/tty/driver.rs` | Carry generation-safe peer IDs through driver dispatch |
| `drivers/src/tty/mod.rs` | Integrate generation checks into read/write/open/close paths |
| `drivers/src/tty/table.rs` | Support scalable slot configuration and generation bookkeeping |
| `drivers/src/tty_tests.rs` | Add PTY stress and stale-handle regressions |

### 30.5 Implementation Summary

**Completed in Phase 26:**

1. **`PtyPeerHandle` generation-safe identity** (`pty.rs`)
   - New `PtyPeerHandle { idx: TtyIndex, generation: u32 }` struct replaces bare `TtyIndex` for cross-end peer references.
   - `PtyPeerHandle::snapshot()` captures the current generation from `TTY_GENERATIONS[slot]` at allocation time.
   - `validate_peer()` checks stored generation against the live `TTY_GENERATIONS` counter — stale handles (from freed/reused slots) are detected and rejected.
   - `master_write()` and `slave_write()` validate generation before forwarding data; stale writes are silently discarded (no panic, no misroute).

2. **Per-slot generation counter** (`table.rs`)
   - `pub static TTY_GENERATIONS: [AtomicU32; MAX_TTYS]` — monotonically increasing counter per slot.
   - `free_pair_if_unused()` bumps both master and slave slot generations via `fetch_add(1, Release)` on free.

3. **Driver-layer plumbing** (`driver.rs`)
   - `TtyDriverKind::PtyMaster { peer: PtyPeerHandle }` and `PtySlave { peer: PtyPeerHandle }` replace raw `slave_idx`/`master_idx` fields.
   - `DriverId` variants carry the same `PtyPeerHandle` for consistent identity throughout dispatch.
   - All `write_output()` and `write_driver_unlocked()` paths pass `PtyPeerHandle` through.

4. **Capacity scaling** (`mod.rs`)
   - `MAX_TTYS` increased from 8 to 32 — suitable for tmux/ssh-heavy workflows.
   - All open/close/destructuring paths updated for `PtyPeerHandle`.

5. **Regression tests** (`tty_tests.rs` — 11 new tests)
   - `test_phase26_max_ttys_is_32` — compile-time capacity assertion.
   - `test_phase26_pty_peer_handle_creation` — `PtyPeerHandle` stores idx + generation.
   - `test_phase26_pty_peer_handle_snapshot` — snapshot captures current `TTY_GENERATIONS`.
   - `test_phase26_generation_bumped_on_free` — `free_pair_if_unused` increments both slot generations.
   - `test_phase26_stale_handle_detected` — `validate_peer` returns false after free.
   - `test_phase26_pty_alloc_captures_generation` — alloc stores correct peer generation.
   - `test_phase26_stale_write_safe_noop` — stale handle write after free/realloc is a silent no-op.
   - `test_phase26_rapid_alloc_free_stress` — 10 rapid alloc/free cycles, generations monotonically increase.
   - `test_phase26_data_flow_with_generation` — master→slave data flow works with generation handles.
   - `test_phase26_validate_peer_out_of_range` — out-of-range index rejected.
   - `test_phase26_multiple_pty_pairs` — 10 concurrent PTY pairs can be allocated.

**Test results**: `just test` → 1184 total, 1184 passed, 0 failed.

## 31. Phase 27: POSIX Completion Set (Rust-Idiomatic)

**Status**: ✅ Done — 1197 total tests (1197 passed, 0 failed); 13 new Phase 27 regressions.

> **Priority**: P2 completeness — close high-value remaining gaps without cloning Linux internals.

### 31.1 High-value termios/ioctl completion

- ✅ **Break handling (IGNBRK, BRKINT, PARMRK)**: Added `handle_break()` to `LineDisc` implementing full POSIX break semantics — IGNBRK discards silently, BRKINT flushes queues and signals SIGINT, PARMRK inserts `\xff\x00\x00` marker. Wired into `input_char()` with NUL-byte detection when any break flag is active.
- ✅ **FIONREAD/TIOCINQ ioctl** (0x541B): Added `bytes_available()` to `LineDisc`, `RawDisc`, and `LdiscKind`. Exposed through `TtyServices`, syscall adapter, and poll/ioctl handler. Returns count of readable bytes in the input queue.

### 31.2 Echo/editing polish

- ✅ **ECHOKE deterministic visual erase**: New `InputAction::KillLineEcho { columns: u16 }` variant. `kill_line()` computes total column width and returns `KillLineEcho` when ECHOKE+ECHO are set, emitting BS-SP-BS triples for each column. Falls back to ECHOK (newline echo) when only ECHOK is set.
- ✅ **ECHOCTL erase fix**: `erase_char()` now returns `KillLineEcho { columns: 2 }` for control characters echoed as `^X`, correctly erasing both columns (was previously sending a single incomplete BS-SP-BS triple).

### 31.3 Non-goals (explicit)

- No 1:1 clone of Linux lock graph or full `n_tty.c` complexity.
- No legacy behavior cargo-culting without a concrete userspace compatibility need.

### 31.4 Rust safety bar

- Prefer typed state machines/newtypes over sentinel booleans and raw IDs.
- Keep unsafe blocks minimal and localized to hardware boundaries.
- Keep error mapping explicit across service adapters and syscall boundaries.

### 31.5 Verification policy (replaces old standalone final-test phase)

- ✅ All 13 Phase 27 tests pass; full `just build` + `just test` pass (1197/1197).
- No separate trailing "test-only" phase; verification is now embedded in every phase gate.

### 31.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | `handle_break()`, `KillLineEcho` action variant, `bytes_available()`, fixed ECHOKE/ECHOCTL erase |
| `drivers/src/tty/mod.rs` | `KillLineEcho` handling in `drain_hw_input()`/`push_input()`, `bytes_available()` public API |
| `drivers/src/tty_tests.rs` | 13 new Phase 27 regression tests |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | FIONREAD ioctl handler |
| `abi/src/syscall.rs` | `FIONREAD` constant (0x541B) |
| `lib/src/kernel_services/syscall_services/tty.rs` | `bytes_available` service function |
| `drivers/src/syscall_services_init.rs` | `tty_bytes_available_adapter` + registration |

### 31.7 Phase 27 regression tests

| # | Test | Covers |
|---|------|--------|
| 1 | `test_phase27_ignbrk_discards_break` | IGNBRK silently drops NUL |
| 2 | `test_phase27_brkint_generates_sigint` | BRKINT flushes + signals |
| 3 | `test_phase27_parmrk_inserts_marker` | PARMRK `\xff\x00\x00` sequence |
| 4 | `test_phase27_nul_without_break_flags_passes_through` | NUL passthrough baseline |
| 5 | `test_phase27_ignbrk_takes_priority_over_brkint` | Flag priority ordering |
| 6 | `test_phase27_echoke_visual_erase` | ECHOKE BS-SP-BS column erase |
| 7 | `test_phase27_echok_newline_on_kill` | ECHOK newline fallback |
| 8 | `test_phase27_echoctl_erase_two_columns` | ECHOCTL `^X` 2-column erase |
| 9 | `test_phase27_kill_empty_line_no_echo` | Kill on empty line = no output |
| 10 | `test_phase27_bytes_available` | LineDisc byte count |
| 11 | `test_phase27_raw_disc_bytes_available` | RawDisc byte count |
| 12 | `test_phase27_ldisc_kind_bytes_available` | LdiscKind dispatch |
| 13 | `test_phase27_fionread_constant` | ABI constant value |
---

## 32. Phase 28: Type-Safe Termios Foundation ✅

**Status**: ✅ Done — 1207 total tests (1207 passed, 0 failed); 10 new Phase 28 regression tests. `InputFlags`, `OutputFlags`, `LocalFlags`, `ControlFlags` bitflags types, `CcIndex` enum, `TtyError::SignalInterrupt` + `to_errno()`, `#[must_use]` on 21 functions, full `ldisc.rs` migration (~40 flag checks, ~15 c_cc accesses).

> **Priority**: P1 infrastructure — foundational type safety that every subsequent phase benefits from.
> **Principle**: Replace raw integer bitmasking with Rust-idiomatic `bitflags!` types. Make invalid flag combinations unrepresentable. Keep the ABI wire format (`UserTermios`) unchanged — conversion happens at the kernel/userspace boundary.

### 32.1 `bitflags!` for termios flag words

- Define `InputFlags`, `OutputFlags`, `LocalFlags`, `ControlFlags` bitflags types wrapping `c_iflag`/`c_oflag`/`c_lflag`/`c_cflag`.
- All existing flag constants (`ICRNL`, `OPOST`, `ECHO`, `CREAD`, etc.) become named bitflags members with `contains()` / `insert()` / `remove()` semantics.
- Migrate all flag checks in `ldisc.rs`, `mod.rs`, `session.rs`, `pty.rs` from `termios.c_iflag & ICRNL != 0` to `flags.contains(InputFlags::ICRNL)`.
- `UserTermios` in `abi/src/syscall.rs` retains raw `u32` fields for syscall ABI stability. `From<u32>` / `Into<u32>` impls on the bitflags types handle conversion at the boundary.

### 32.2 Strongly-typed `CcIndex` enum

- Replace raw `usize` indices into `c_cc[]` with a `CcIndex` enum: `VINTR`, `VQUIT`, `VERASE`, `VKILL`, `VEOF`, `VTIME`, `VMIN`, `VSTART`, `VSTOP`, `VSUSP`, `VEOL`, `VREPRINT`, `VDISCARD`, `VWERASE`, `VLNEXT`, `VEOL2`.
- Implement `Into<usize>` for array access. Invalid indices become compile-time errors instead of runtime panics.
- Add `_POSIX_VDISABLE` constant (value `0`) for disabled control characters.

### 32.3 `TtyError` refinement

- Split the monolithic `TtyError` enum into internal diagnostic variants (for logging/debugging within the TTY subsystem) and a boundary `to_errno()` mapping method.
- Add `#[must_use]` on all error-returning functions to prevent silent error drops.
- Keep error variants semantically meaningful: `HungUp`, `NotAllocated`, `InvalidIndex`, `WouldBlock`, `PermissionDenied`, `SignalInterrupt`, etc.

### 32.4 Non-goals

- No new termios features — pure type-safety refactor.
- No changes to external behavior or ABI wire format.
- No behavioral changes to any existing test.

### 32.5 Verification

- All existing 1197+ tests pass unchanged (behavioral equivalence proves the migration is correct).
- New compile-time tests: invalid flag combinations rejected, `CcIndex` exhaustiveness verified.
- `just build` + `just test` gate.

### 32.6 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `bitflags!` type definitions, `CcIndex` enum, `From`/`Into` impls between raw and typed representations |
| `drivers/src/tty/ldisc.rs` | Migrate all flag checks to `bitflags!` `contains()` API; use `CcIndex` for `c_cc` access |
| `drivers/src/tty/mod.rs` | Migrate flag checks, refine `TtyError` enum, add `#[must_use]` |
| `drivers/src/tty/session.rs` | No flag checks found — no changes needed |
| `drivers/src/tty/pty.rs` | No flag checks found — no changes needed |
| `drivers/src/tty_tests.rs` | Update test helpers to use typed flags; add compile-time exhaustiveness tests |

### 32.7 Implementation notes

- **`session.rs` and `pty.rs`**: Audited — neither file contains direct termios flag checks requiring migration; only comments reference flag names. No changes applied.
- **Backward compatibility**: Raw `u32`/`usize` constants preserved in `abi/src/syscall.rs` for userland consumers (`userland/src/apps/shell/input.rs` imports `ECHO`, `ECHOE`, `ICANON`, `ISIG` as raw constants).
- **`bitflags` crate**: v2.4 already a workspace dependency — no new crates added.
- **`UserTermios` ABI**: Struct fields remain raw `u32`; typed accessor methods (`input_flags()`, `output_flags()`, `local_flags()`, `control_flags()`, `cc(CcIndex)`, `set_cc(CcIndex, u8)`) handle conversion at kernel/userspace boundary.
- **`CcIndex`**: 14 variants (`Vintr`..`Vlnext`); `VDISCARD` omitted (not wired in SlopOS). `POSIX_VDISABLE = 0` constant added.
- **`TtyError`**: Added `SignalInterrupt` variant and `to_errno()` method mapping all variants to negative errno values.
- **Test delta**: +10 tests (1197 → 1207). All behavioral-equivalence tests confirm zero regression.
---

## 33. Phase 29: LdiscKind Dispatch Consolidation

**Status**: ✅ **DONE**

> **Priority**: P2 maintainability — eliminate mechanical duplication that makes the ldisc layer harder to extend.
> **Principle**: Reduce boilerplate without sacrificing explicitness. Codify the split-write invariant with naming conventions so future contributors can’t accidentally violate it.

### 33.1 `dispatch_ldisc!` macro

- Create a `dispatch_ldisc!` macro that generates delegating match arms for methods shared across `LineDisc` and `RawDisc`.
- Each method signature is declared once in the macro invocation; the macro expands to the full `match self { NTty(l) => l.foo(), Raw(r) => r.foo() }` dispatch.
- Target: eliminate 40+ mechanical match arms currently duplicated in `LdiscKind`.

### 33.2 `*_locked()` naming convention

- Rename internal methods that must be called while holding the TTY slot lock to use the `_locked` suffix (e.g., `process_input_locked()`, `read_locked()`).
- Methods that operate on data extracted outside the lock keep their current names (e.g., `write_driver_unlocked()`).
- Document the convention in a module-level comment so future contributors understand the split-write contract.

### 33.3 Internal `LdiscOps` trait

- Where `LineDisc` and `RawDisc` share identical method signatures, extract a private `LdiscOps` trait to formalize the contract.
- The `dispatch_ldisc!` macro dispatches through this trait.
- The trait is `pub(crate)` — purely internal organizational improvement. External API unchanged.

### 33.4 Dead code audit

- Identify and remove any ldisc methods that exist but are never called from the public `tty::` API.
- Document intentionally-unused methods (if any) with `#[allow(dead_code)]` and a justification comment.

### 33.5 Verification

- All existing tests pass unchanged.
- Code line count in `ldisc.rs` reduced measurably (target: 15–25% reduction in boilerplate lines).
- `just build` + `just test` gate.

### 33.6 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Add `dispatch_ldisc!` macro, extract `LdiscOps` trait, rename `*_locked` methods, remove dead code |
| `drivers/src/tty/mod.rs` | Update call sites for renamed methods, add module-level convention documentation |
| `drivers/src/tty_tests.rs` | Update any test helpers referencing renamed internals |

---

## 34. Phase 30: `/dev/tty` Controlling Terminal Device

**Status**: 📋 Planned

> **Priority**: P0 correctness — the single most critical missing feature for bash/zsh job control.
> **Principle**: `/dev/tty` is a per-process magic alias that resolves to the caller’s controlling terminal. Shells use it for prompts, job control, and error output. Without it, interactive shells cannot function correctly.

### 34.1 Resolution logic

- When a process opens `/dev/tty`, the kernel looks up `current_task().controlling_tty`.
- If set: the open succeeds and the returned FD operates on that TTY index. All read/write/ioctl/poll operations go through the same per-TTY dispatch as a direct device open.
- If not set (no controlling terminal): the open fails with `ENXIO`.
- Opening `/dev/tty` never acquires a new controlling terminal — it only accesses an existing one. `O_NOCTTY` is irrelevant.

### 34.2 FD routing

- The file descriptor created by `/dev/tty` open is identical in kind to one opened on the actual device path (`/dev/ttyN` or `/dev/pts/N`).
- Intercept `/dev/tty` in `fileio.rs` open path, alongside existing `/dev/ptmx` and `/dev/pts/N` intercepts.
- Reuse `controlling_tty` field from `task_struct.rs` — no new kernel state needed.

### 34.3 Process inheritance

- Child processes inherit the controlling terminal via `fork()`, so `/dev/tty` resolves to the same device as the parent.
- After `setsid()`, the controlling terminal is cleared, so `/dev/tty` returns `ENXIO` until a new ctty is acquired.

### 34.4 Verification

- Test: process with controlling terminal opens `/dev/tty` → reads/writes succeed, ioctl returns same termios as direct device open.
- Test: process without controlling terminal opens `/dev/tty` → `ENXIO`.
- Test: `setsid()` + open `/dev/tty` → `ENXIO` (ctty cleared by setsid).
- Test: fork child inherits `/dev/tty` resolution.
- `just build` + `just test` gate.

### 34.5 Files expected to change

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Add `/dev/tty` path intercept in `open()`, resolve via `controlling_tty` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Add `get_controlling_tty()` service method if not already exposed |
| `drivers/src/syscall_services_init.rs` | Wire adapter if new service method needed |
| `drivers/src/tty_tests.rs` | Regression tests for `/dev/tty` resolution, `ENXIO`, fork inheritance |

---

## 35. Phase 31: Background Write Protection (SIGTTOU on `tcsetattr`)

**Status**: 📋 Planned

> **Priority**: P0 correctness — POSIX requires SIGTTOU when a background process changes terminal settings.
> **Principle**: Background processes must not silently mutate the terminal. The foreground application’s terminal state is sacred.

### 35.1 Foreground check on `tcsetattr`

- Before applying any termios change (`TCSETS`, `TCSETSW`, `TCSETSF`), verify that the calling process is in the foreground process group of the target TTY.
- If the caller is in a background process group and `SIGTTOU` is not blocked or set to `SIG_IGN` by the caller, deliver `SIGTTOU` to the caller’s process group and return `EINTR`.
- Reuse the existing `ForegroundCheck` enum outcomes from `session.rs`.
- Add the check in `tty::set_termios_mode()`, `set_termios_wait()`, and `set_termios_flush()` before acquiring the slot lock.

### 35.2 Blocked/ignored SIGTTOU bypass

- Per POSIX, if `SIGTTOU` is blocked or set to `SIG_IGN` in the caller’s signal disposition, the `tcsetattr` proceeds silently (no signal, no error).
- Implement signal disposition check via existing `signal_is_blocked_or_ignored()` infrastructure, or add a minimal check against the task’s signal handler table.

### 35.3 Orphaned process group

- If the background process group is orphaned (no process in the group has a parent in a different process group within the same session), return `EIO` instead of delivering `SIGTTOU`.
- Orphan detection reuses `pgrp_exists_in_session()` from Phase 24 + parent cross-check.

### 35.4 TOSTOP audit

- Verify that `TOSTOP` flag on `write()` paths correctly delivers `SIGTTOU` for background writes.
- Audit the existing implementation against POSIX specification and harden edge cases if needed.

### 35.5 Verification

- Test: background process `tcsetattr` → `SIGTTOU` delivered, termios unchanged.
- Test: background process with `SIGTTOU` blocked → `tcsetattr` succeeds silently.
- Test: foreground process `tcsetattr` → no signal, applies normally.
- Test: `TOSTOP` + background write → `SIGTTOU` delivered.
- Test: orphaned background pgrp `tcsetattr` → `EIO`.
- `just build` + `just test` gate.

### 35.6 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add foreground check to `set_termios_mode()`, `set_termios_wait()`, `set_termios_flush()` |
| `drivers/src/tty/session.rs` | Add `is_sigttou_relevant()` helper or extend `ForegroundCheck` |
| `abi/src/syscall.rs` | Verify `SIGTTOU` constant present, add `EIO` errno if missing |
| `drivers/src/tty_tests.rs` | Regression tests for SIGTTOU on tcsetattr, TOSTOP audit, blocked signal bypass, orphan EIO |

---

## 36. Phase 32: Controlling Terminal Lifecycle Integrity

**Status**: 📋 Planned

> **Priority**: P0 correctness — the controlling terminal lifecycle chain must be watertight for shells to work.
> **Principle**: Individual pieces exist from earlier phases. This phase performs a comprehensive end-to-end audit and hardens every state transition in the controlling terminal lifecycle: `fork()` → `setsid()` → `open()` → `TIOCSCTTY` → `TIOCNOTTY` → process exit.

### 36.1 `fork()` inheritance audit

- Verify child inherits `controlling_tty`, `session_id`, `pgid` correctly.
- Add explicit test for deep fork chains: grandchild inherits grandparent’s ctty.
- Verify no accidental double-assignment or stale reference on fork.

### 36.2 `setsid()` clearing

- Verify `setsid()` clears `controlling_tty` and detaches from the TTY’s session state.
- Verify subsequent opens do NOT auto-acquire a ctty unless `TIOCSCTTY` is explicitly used.
- Verify `setsid()` fails with `EPERM` if the caller is already a session leader or shares a pgid with another session.

### 36.3 `O_NOCTTY` enforcement

- Verify that opening a TTY device with `O_NOCTTY` does not acquire it as controlling terminal, even if the process is a session leader without a ctty.
- Verify that opening a TTY device WITHOUT `O_NOCTTY` as a session leader without a ctty DOES auto-acquire (Linux behavior).

### 36.4 `TIOCSCTTY` preconditions

- Verify all preconditions: must be session leader, must not already have a different ctty, TTY must not belong to a different session.
- Document steal-ctty semantics (`arg=1` with sufficient privilege) as explicit non-goal for now, or implement if needed for specific userspace.

### 36.5 Session leader exit → hangup

- Verify the full chain: session leader exits → controlling TTY receives hangup → all session members get `SIGHUP` + `SIGCONT` → controlling terminal cleared for all members.
- Verify non-leader exit does NOT trigger hangup.

### 36.6 Race condition audit

- Audit for TOCTOU races between checking and setting controlling terminal (e.g., two session leaders racing to `TIOCSCTTY` the same device).
- Add stress test with rapid `fork()`/`setsid()`/`TIOCSCTTY` cycling.

### 36.7 Verification

- Minimum 12 lifecycle chain tests covering every state transition.
- Stress test: rapid fork/setsid/TIOCSCTTY cycling.
- `just build` + `just test` gate.

### 36.8 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Fix any discovered inconsistencies in ctty acquisition/release |
| `drivers/src/tty/session.rs` | Harden TIOCSCTTY preconditions, document steal semantics |
| `fs/src/fileio.rs` | Audit and fix O_NOCTTY handling, auto-acquire logic on open |
| `core/src/syscall/process_handlers.rs` | Audit setsid() ctty clearing, EPERM checks |
| `core/src/scheduler/task.rs` | Audit fork inheritance, session leader exit → hangup chain |
| `drivers/src/tty_tests.rs` | 12+ lifecycle chain tests, race condition stress tests |

---

## 37. Phase 33: Post-Hangup I/O Hardening

**Status**: 📋 Planned

> **Priority**: P0 correctness — consistent I/O behavior after hangup is essential for shells to detect session termination.
> **Principle**: A hung-up TTY is permanently dead. All I/O operations must produce deterministic, POSIX-mandated results. No code path may silently succeed on a hung-up device.

### 37.1 I/O behavior after hangup

- **Read → EOF (0 bytes)**: All `tty_read()` paths return 0 bytes when the TTY is hung up. This is how shells detect session termination.
- **Write → `-EIO`**: All `tty_write()` paths return `EIO` after hangup. The data has nowhere to go.
- **Poll → `POLLHUP`**: Readers see `POLLIN | POLLHUP` (readable with immediate EOF). Writers see `POLLHUP`.
- **Ioctl → `EIO` with exceptions**: Most ioctls return `EIO`. Exceptions: `TIOCGPGRP`, `TIOCSPGRP`, `TIOCGSID` remain functional (POSIX requires job control queries to work after hangup).

### 37.2 Hangup flag consistency

- Verify a per-TTY `hung_up: bool` flag is checked at the top of every I/O entry point.
- The flag is set by `tty_hangup()` and never cleared — a hung-up TTY is permanently dead until the slot is reclaimed.
- Use a consistent early-return pattern: `if self.hung_up { return Err(TtyError::HungUp); }` at method entry, with `HungUp` mapped to appropriate errno at the syscall boundary.

### 37.3 PTY cross-end hangup

- When the master side of a PTY is closed, the slave sees hangup.
- Verify slave reads return 0 (EOF), slave writes return `EIO`, slave poll returns `POLLHUP`.
- Verify the master cannot be written to after the slave is hung up (generation check should catch this, but audit explicitly).

### 37.4 Verification

- Test: hangup → read returns 0.
- Test: hangup → write returns `EIO`.
- Test: hangup → poll returns `POLLHUP`.
- Test: PTY master close → slave read returns 0, slave write returns `EIO`.
- Test: hangup → `TIOCGPGRP` still works (hangup-safe ioctl).
- Test: hangup → `TCSETS` returns `EIO` (hangup-blocked ioctl).
- `just build` + `just test` gate.

### 37.5 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add/verify `hung_up` flag, add early-return checks to read/write/poll |
| `drivers/src/tty/ldisc.rs` | Verify ldisc methods propagate hangup state |
| `drivers/src/tty/pty.rs` | Verify cross-end hangup propagation |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add hangup checks to ioctl dispatch, document hangup-safe ioctls |
| `drivers/src/tty_tests.rs` | Hangup I/O consistency tests, PTY cross-end hangup tests |

---

## 38. Phase 34: Extended Line Boundaries (VEOL, VEOL2)

**Status**: 📋 Planned

> **Priority**: P1 completeness — required for protocols and applications that use custom line delimiters.
> **Principle**: POSIX allows two additional end-of-line terminators beyond hardcoded `\n` and `VEOF`. These are user-configurable via `c_cc[VEOL]` and `c_cc[VEOL2]`.

### 38.1 Scope

- In canonical mode, when the input character matches `c_cc[VEOL]` (and VEOL is not disabled, i.e., value != `_POSIX_VDISABLE` / 0), treat it as a line terminator: increment `line_count`, complete the canonical line, wake blocked readers.
- Same behavior for `c_cc[VEOL2]` — a second independent line terminator.
- VEOL/VEOL2 do NOT replace `\n` or `VEOF` — they are additional terminators. Line completion logic checks all four: `\n`, `VEOF`, `VEOL`, `VEOL2`.
- VEOL/VEOL2 characters are echoed normally if `ECHO` is set (they are not control characters in the ECHOCTL sense).
- Disabled by default (`_POSIX_VDISABLE` = 0 in default termios `c_cc`).
- Use `CcIndex` from Phase 28 for type-safe access.

### 38.2 Verification

- Test: VEOL character completes canonical line.
- Test: VEOL2 character completes canonical line.
- Test: VEOL disabled (value 0) → no effect.
- Test: VEOL + `\n` both work simultaneously as independent terminators.
- Test: VEOL echo behavior with ECHO set.
- `just build` + `just test` gate.

### 38.3 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `VEOL`/`VEOL2` `CcIndex` entries if missing, `_POSIX_VDISABLE` constant |
| `drivers/src/tty/ldisc.rs` | Add VEOL/VEOL2 checks in canonical input path alongside NL/EOF |
| `drivers/src/tty_tests.rs` | Line boundary tests for VEOL/VEOL2, disabled state, multi-terminator |

---

## 39. Phase 35: UTF-8 Aware Editing (IUTF8)

**Status**: 📋 Planned

> **Priority**: P1 quality — without IUTF8, backspace erases a single byte, potentially leaving partial UTF-8 sequences in the edit buffer. Required for zsh/readline quality.
> **Principle**: When IUTF8 is set, editing operations work on codepoints, not bytes. Display width accounts for wide characters. When IUTF8 is not set, byte-at-a-time editing continues unchanged (backward compatible).

### 39.1 Backspace (VERASE) with IUTF8

- Count the number of bytes in the trailing UTF-8 codepoint by examining continuation byte prefixes (`10xxxxxx` pattern).
- Erase all bytes of the codepoint from the edit buffer.
- Echo the correct number of BS-SP-BS triples based on the codepoint’s display width (1 for most characters, 2 for CJK/fullwidth).

### 39.2 Word erase (VWERASE) with IUTF8

- Erase full codepoints until a word boundary (whitespace).
- Each codepoint’s display width determines the echo erase column count.

### 39.3 Display width helper

- Implement a minimal `utf8_char_width(codepoint: u32) -> u8` function that returns:
  - `2` for CJK Unified Ideographs, fullwidth forms, and common emoji ranges (U+1100–U+115F, U+2E80–U+A4CF, U+F900–U+FAFF, U+FE30–U+FE6F, U+FF01–U+FF60, U+FFE0–U+FFE6, U+20000–U+2FA1F).
  - `1` for everything else.
- This is a range-based lookup, NOT a full Unicode database. Covers the common cases without bloating the kernel.

### 39.4 Fallback

- When `IUTF8` is not set, byte-at-a-time editing continues unchanged.
- No output processing changes — IUTF8 only affects input editing.

### 39.5 Verification

- Test: IUTF8 + backspace on ASCII → 1 byte erased, 1 column cleared.
- Test: IUTF8 + backspace on 2-byte UTF-8 (e.g., é) → 2 bytes erased, 1 column cleared.
- Test: IUTF8 + backspace on 3-byte CJK → 3 bytes erased, 2 columns cleared.
- Test: IUTF8 off + backspace on multi-byte → 1 byte erased (legacy behavior preserved).
- Test: word erase with mixed ASCII + UTF-8 content.
- `just build` + `just test` gate.

### 39.6 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `IUTF8` constant to input flags |
| `drivers/src/tty/ldisc.rs` | UTF-8 aware `erase_char()`, `word_erase()`, `utf8_char_width()` helper function |
| `drivers/src/tty_tests.rs` | UTF-8 editing tests, display width tests, fallback behavior tests |

---

## 40. Phase 36: Input Buffer Policy (IMAXBEL, IXOFF, CREAD)

**Status**: 📋 Planned

> **Priority**: P1 completeness — three input-side termios flags that control buffer-full behavior, terminal-to-host flow control, and receiver enable/disable.
> **Principle**: Each flag is a cheap early check in the input path. Implement them as clean, orthogonal guards rather than interleaved conditionals.

### 40.1 IMAXBEL

- When `IMAXBEL` is set and the input buffer is full, ring the bell (send BEL `\x07` to output) instead of silently discarding the character.
- Without `IMAXBEL`, buffer-full characters are silently dropped (current behavior).
- Implementation: check buffer capacity before enqueue in `LineDisc::input_char()`. If full and `IMAXBEL` set, return an `InputAction::Bell` (or emit BEL directly through the output path).

### 40.2 IXOFF

- When `IXOFF` is set, the kernel sends `XOFF` (`c_cc[VSTOP]`, typically `^S`) to the terminal when the input buffer reaches a high-water mark (~80% capacity), and `XON` (`c_cc[VSTART]`, typically `^Q`) when buffer level drops below a low-water mark (~20% capacity).
- This is terminal-to-kernel flow control: the kernel tells the terminal device to stop/start sending.
- Track `xoff_sent: bool` flag per ldisc. Check thresholds in `input_char()` (high-water) and `read()` (low-water).

### 40.3 CREAD

- When `CREAD` is NOT set in `c_cflag`, input characters are silently discarded (receiver disabled).
- Check at the very top of `input_char()` — before any other processing.
- Default termios has `CREAD` set (receiver enabled).

### 40.4 Verification

- Test: IMAXBEL set + buffer full → BEL output, character discarded.
- Test: IMAXBEL not set + buffer full → silent discard (no BEL).
- Test: IXOFF set + buffer fills past high water → XOFF sent to output.
- Test: IXOFF + buffer drained past low water after XOFF → XON sent.
- Test: CREAD cleared → all input silently discarded.
- Test: CREAD set → input processed normally (baseline).
- `just build` + `just test` gate.

### 40.5 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `IMAXBEL`, `IXOFF`, `CREAD` flag constants if missing |
| `drivers/src/tty/ldisc.rs` | Add CREAD gate at top of `input_char()`, IMAXBEL bell action on buffer full, IXOFF threshold tracking with `xoff_sent` state |
| `drivers/src/tty/mod.rs` | Wire IXOFF XON/XOFF output actions to driver write path |
| `drivers/src/tty_tests.rs` | Tests for all three flags independently and their interactions |

---

## 41. Phase 37: Deferred Reprint (PENDIN)

**Status**: 📋 Planned

> **Priority**: P1 quality — PENDIN ensures users see their pending input re-echoed correctly after terminal mode changes. Required for quality readline/zsh experience.
> **Principle**: PENDIN is a one-shot flag. When set, the next input processing call triggers a full reprint of the edit buffer with current echo settings. The flag is then cleared.

### 41.1 PENDIN flag semantics

- Add `pending_reprint: bool` to `LineDisc` state.
- When `PENDIN` is set in `c_lflag`, the line discipline marks the input as needing reprint.
- The next `input_char()` call (or explicit `VREPRINT` character) triggers a full reprint of the pending edit buffer contents through the current echo processing logic.
- After reprint, `PENDIN` is cleared (one-shot).

### 41.2 Mode change trigger

- Certain termios changes (switching between canonical/non-canonical, changing echo flags like ECHO, ECHOE, ECHOK, ECHOCTL) automatically set `PENDIN`.
- This causes the user to see their pending input re-echoed with the new settings applied.
- Set `pending_reprint = true` in `set_termios_mode()` when echo-affecting flags change.

### 41.3 VREPRINT integration

- `VREPRINT` character already triggers reprint — enhance it to also clear the `PENDIN` flag.
- Reprint mechanism: walk the edit buffer and re-echo each character through current echo processing. Prefix reprint output with `^R\n` (per convention).

### 41.4 Verification

- Test: toggle ECHO flag → PENDIN set automatically → next input reprints buffer.
- Test: VREPRINT character triggers reprint and clears PENDIN.
- Test: PENDIN reprint reflects current echo settings, not the old ones.
- Test: PENDIN only fires once (one-shot: set, trigger, cleared).
- `just build` + `just test` gate.

### 41.5 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `PENDIN` constant to local flags if missing |
| `drivers/src/tty/ldisc.rs` | Add `pending_reprint` state, reprint logic in `input_char()`, VREPRINT clearing |
| `drivers/src/tty/mod.rs` | Set PENDIN automatically on termios changes that affect echo/canonical mode |
| `drivers/src/tty_tests.rs` | PENDIN auto-set, VREPRINT clearing, echo-mode-change reprint, one-shot behavior |

---

## 42. Phase 38: PTY Namespace & Device Nodes

**Status**: 📋 Planned

> **Priority**: P2 infrastructure — standard Unix PTY allocation interface required for tmux, screen, ssh, and any process that programmatically creates terminals.
> **Principle**: Replace internal-only PTY allocation with filesystem-visible device nodes. `/dev/ptmx` for master allocation, `/dev/pts/N` for slave access, with proper lock semantics to prevent setup races.

### 42.1 `/dev/ptmx` open

- Opening `/dev/ptmx` allocates a new PTY pair and returns a file descriptor to the master side.
- The slave is identified by its PTY number N (obtainable via `TIOCGPTN` ioctl, already partially implemented).
- The slave starts locked — cannot be opened until the master holder unlocks it.

### 42.2 `/dev/pts/N` open

- Opening `/dev/pts/N` returns a file descriptor to the slave side of PTY pair N.
- The slave must be unlocked (via `TIOCSPTLCK` with arg=0) before it can be opened. Attempting to open a locked slave returns `EIO`.
- Slave open also triggers controlling terminal acquisition if the opener is a session leader without a ctty (unless `O_NOCTTY`).

### 42.3 Lock ioctls

- **`TIOCSPTLCK`**: Set the PTY slave lock state. `arg=0` unlocks, `arg=1` locks. Only valid on the master FD.
- **`TIOCGPTLCK`**: Get the current lock state. Returns 0 (unlocked) or 1 (locked).
- Add `locked: bool` per PTY pair in `pty.rs`. Default `true` on allocation.

### 42.4 devfs integration

- If SlopOS devfs supports dynamic node creation: register `/dev/ptmx` as a static device and `/dev/pts/` as a dynamic directory where entries appear/disappear with PTY allocation/free.
- If devfs does not yet support dynamic entries: use path-based intercepts in `fileio.rs` as a stepping stone (consistent with existing `/dev/ptmx` and `/dev/pts/N` handling).

### 42.5 Verification

- Test: open `/dev/ptmx` → returns master FD with valid `TIOCGPTN`.
- Test: slave locked by default → open `/dev/pts/N` fails with `EIO`.
- Test: `TIOCSPTLCK` with arg=0 → slave unlocked → open succeeds.
- Test: data flow through device node FDs (master write → slave read and vice versa).
- Test: close master → slave hangup.
- Test: multiple simultaneous PTY pairs via `/dev/ptmx`.
- `just build` + `just test` gate.

### 42.6 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/pty.rs` | Add `locked` field per PTY pair, lock/unlock methods, slave-open guard |
| `fs/src/fileio.rs` | Enhance `/dev/ptmx` and `/dev/pts/N` open paths with lock checking |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add `TIOCSPTLCK`/`TIOCGPTLCK` ioctl handlers |
| `abi/src/syscall.rs` | Add `TIOCSPTLCK`/`TIOCGPTLCK` ioctl constants |
| `drivers/src/tty_tests.rs` | PTY namespace tests, lock semantics, multi-pair allocation via device nodes |

---

## 43. Phase 39: PTY Packet Mode (TIOCPKT)

**Status**: 📋 Planned

> **Priority**: P2 infrastructure — terminal multiplexers (tmux, screen) need packet mode to receive out-of-band control information about slave-side state changes.
> **Principle**: When packet mode is enabled on a PTY master, every `read()` is prefixed with a single control byte indicating the event type. This gives multiplexers visibility into flow control, flushes, and mode changes on the slave.

### 43.1 `TIOCPKT` ioctl

- Enable/disable packet mode on a PTY master FD.
- When enabled, every `read()` on the master is prefixed with a single control byte.
- When disabled, reads behave normally (no prefix).

### 43.2 Control byte encoding

- `TIOCPKT_DATA` (0x00): Normal data follows.
- `TIOCPKT_FLUSHREAD` (0x01): Slave input queue was flushed.
- `TIOCPKT_FLUSHWRITE` (0x02): Slave output queue was flushed.
- `TIOCPKT_STOP` (0x04): Slave output stopped (XOFF received).
- `TIOCPKT_START` (0x08): Slave output started (XON received).
- `TIOCPKT_NOSTOP` (0x10): `IXON` cleared on slave.
- `TIOCPKT_DOSTOP` (0x20): `IXON` set on slave.

### 43.3 Event tracking

- Add `packet_mode: bool` and `packet_events: u8` bitmask to PTY master state in `pty.rs`.
- When events occur on the slave (queue flush, flow control state change, IXON toggle via termios), set the corresponding bit in `packet_events`.
- On master `read()`: if `packet_events != 0`, return a zero-data read with just the control byte (events are consumed). If no events pending and data available, return `TIOCPKT_DATA` prefix + data.
- `TIOCPKT` ioctl toggles `packet_mode`. Clearing it resets `packet_events`.

### 43.4 Poll interaction

- Master reports `POLLIN` when either data OR pending packet events are available.

### 43.5 Verification

- Test: `TIOCPKT` on → master read gets control byte prefix on normal data.
- Test: slave flush → master sees `TIOCPKT_FLUSHREAD`/`TIOCPKT_FLUSHWRITE`.
- Test: slave IXON toggle → master sees `TIOCPKT_DOSTOP`/`TIOCPKT_NOSTOP`.
- Test: `TIOCPKT` off → normal read (no prefix).
- Test: poll reports readable when packet events pending.
- `just build` + `just test` gate.

### 43.6 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/pty.rs` | Add `packet_mode`, `packet_events` fields; modify `master_read()` for packet framing; event bit tracking |
| `drivers/src/tty/mod.rs` | Set packet event bits on flush and flow control changes |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add `TIOCPKT` ioctl handler |
| `abi/src/syscall.rs` | Add `TIOCPKT` and `TIOCPKT_*` constants |
| `drivers/src/tty_tests.rs` | Packet mode tests, event delivery, toggle on/off, poll interaction |

---

## 44. Phase 40: VT100/ANSI Terminal Emulation

**Status**: 📋 Planned

> **Priority**: P2 infrastructure — required for programs that use cursor positioning, colors, or screen clearing (ncurses, vi, top, htop, etc.).
> **Principle**: Replace raw byte output in the virtual console with a state-machine-driven ANSI/VT100 parser. The parser is pure `no_std`, no-alloc, and produces typed actions that the console renderer executes. Separate parsing from rendering for testability.

### 44.1 `VtParser` state machine

- Implement a `VtParser` with states: `Ground`, `Escape`, `CsiEntry`, `CsiParam`, `CsiIntermediate`, `OscString`.
- Parse CSI sequences (`\x1b[...`) with parameter accumulation into a fixed-size array (max 16 params, matching xterm).
- Produce `VtAction` enum variants: `Print(u8)`, `MoveCursor { direction, count }`, `SetCursorPos { row, col }`, `EraseDisplay(mode)`, `EraseLine(mode)`, `ScrollUp(n)`, `ScrollDown(n)`, `SetAttribute(SgrAttr)`, `SaveCursor`, `RestoreCursor`, `SetMode(mode)`, `ResetMode(mode)`.

### 44.2 Supported sequences (initial set)

- **Cursor movement**: CUU (`\x1b[A`), CUD (`\x1b[B`), CUF (`\x1b[C`), CUB (`\x1b[D`), CUP (`\x1b[row;colH`), home (`\x1b[H`).
- **Erase**: ED (`\x1b[J`, `\x1b[0J`, `\x1b[1J`, `\x1b[2J`), EL (`\x1b[K`, `\x1b[0K`, `\x1b[1K`, `\x1b[2K`).
- **Scroll**: SU (`\x1b[nS`), SD (`\x1b[nT`).
- **SGR**: Reset (`\x1b[0m`), bold (`1`), underline (`4`), inverse (`7`), 8-color foreground (`30-37`), 8-color background (`40-47`), bright variants (`90-97`, `100-107`).
- **Misc**: DECSC/DECRC (save/restore cursor), cursor visibility (`\x1b[?25h`/`\x1b[?25l`), alternate screen buffer (`\x1b[?1049h`/`\x1b[?1049l`).
- **Non-goals for this phase**: 256-color, 24-bit truecolor, Unicode grapheme clustering.

### 44.3 Integration

- Wire `VtParser` into `VConsoleDriver` output path. Each byte passes through the parser; `VtAction` variants are dispatched to `VConsoleState` methods.
- `VConsoleState` gains cursor attributes (foreground/background color, bold, underline, inverse) and applies them when rendering glyphs.
- Malformed or unrecognized sequences are silently discarded (no panic, no corruption).

### 44.4 Verification

- Test: `\x1b[2J` clears screen.
- Test: `\x1b[10;20H` positions cursor at row 10, column 20.
- Test: `\x1b[31m` sets red foreground.
- Test: `\x1b[0m` resets all attributes.
- Test: `\x1b[A` moves cursor up one row.
- Test: nested/malformed sequences handled gracefully (garbage in → no crash, parser returns to ground state).
- Test: parser state machine fuzz with random byte sequences.
- `just build` + `just test` gate.

### 44.5 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/vtparser.rs` | **New file** — VT100/ANSI escape sequence state machine, `VtAction` enum, parameter accumulator |
| `drivers/src/tty/vconsole.rs` | Integrate `VtParser`, add cursor attribute state (color, bold, etc.), implement erase/scroll/color rendering |
| `drivers/src/tty/mod.rs` | Update module declarations for `vtparser` |
| `drivers/src/tty_tests.rs` | VT parser unit tests, cursor movement, SGR, erase, malformed sequence resilience |

---

## 45. Phase 41: Advanced PTY & Session Control (EXTPROC, vhangup)

**Status**: 📋 Planned

> **Priority**: P2 infrastructure — EXTPROC is needed for ssh/network terminals; vhangup is needed for secure session handoff (login/getty).
> **Principle**: Two independent features grouped because both relate to PTY/session edge cases beyond basic terminal usage.

### 45.1 EXTPROC

- **`EXTPROC` local flag**: When set, the line discipline passes all input to the read buffer without local echo or canonical editing. Signal characters (`VINTR`, `VQUIT`, `VSUSP`) are still processed if `ISIG` is set.
- Use case: network TTY protocols (ssh, telnet) where the remote side handles line editing. The local ldisc should not interfere.
- Implementation: check `EXTPROC` at the top of the canonical editing path in `input_char()`. If set, bypass VERASE/VKILL/VWERASE processing and enqueue the character directly. Suppress echo output.
- When `EXTPROC` is cleared, normal canonical/echo behavior resumes.

### 45.2 `vhangup()` syscall

- Revokes access to the calling process’s controlling terminal. All other file descriptors referencing this TTY become invalid — subsequent I/O returns `EIO`.
- Use case: `login` calls `vhangup()` to ensure no leftover FDs from previous sessions can spy on the new session. The caller then re-opens the device to get a fresh, uncompromised FD.
- Implementation: `vhangup()` calls `tty_hangup()` on the caller’s controlling terminal, reusing the existing hangup infrastructure from Phases 7 and 33.
- Only callable by a process that holds the controlling terminal. Return `EPERM` if the caller has no ctty.

### 45.3 Verification

- Test: EXTPROC set → no echo, no canonical editing, signals still delivered.
- Test: EXTPROC cleared → normal canonical/echo behavior resumes.
- Test: `vhangup()` → existing FDs return `EIO` on I/O.
- Test: caller can re-open device after `vhangup()` for a fresh FD.
- Test: `vhangup()` without ctty → `EPERM`.
- `just build` + `just test` gate.

### 45.4 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `EXTPROC` flag constant, `SYS_VHANGUP` syscall number |
| `drivers/src/tty/ldisc.rs` | Add EXTPROC bypass in canonical input path, suppress echo when EXTPROC active |
| `drivers/src/tty/mod.rs` | Wire `vhangup()` public API through hangup infrastructure |
| `core/src/syscall/process_handlers.rs` | Add `sys_vhangup` handler with ctty + permission checks |
| `drivers/src/tty_tests.rs` | EXTPROC behavior tests, vhangup lifecycle tests |

---

## 46. Phase 42: Legacy Termios Completion (ECHOPRT, IUCLC, OLCUC)

**Status**: 📋 Planned

> **Priority**: P3 parity — remaining POSIX/Linux termios flags for claiming complete termios support. Low urgency but necessary for full compatibility.
> **Principle**: Implement remaining flags cleanly without legacy baggage. Each flag is a small, isolated transformation that slots into existing processing pipelines.

### 46.1 ECHOPRT (hardcopy erase style)

- When `ECHOPRT` and `ECHO` are set, erased characters display between `\` and `/` instead of using BS-SP-BS. This is the "hardcopy terminal" erase style.
- On first erase in a sequence: output `\`, then the erased character.
- On subsequent contiguous erases: output each erased character without another `\`.
- When the erase sequence ends (next non-erase input): output `/`.
- Implementation: add `in_erase_seq: bool` to `LineDisc` state. Set on first VERASE with ECHOPRT; clear on any non-erase input.

### 46.2 IUCLC (input uppercase-to-lowercase)

- When `IUCLC` is set in `c_iflag`, uppercase input characters (A-Z) are mapped to lowercase (a-z).
- Applied in `input_char()` before canonical editing and echo processing.
- Simple range check: `if b'A' <= ch && ch <= b'Z' { ch += 32 }`.
- Linux marks this as non-POSIX (XSI extension) but supports it. We implement for compatibility.

### 46.3 OLCUC (output lowercase-to-uppercase)

- When `OLCUC` is set in `c_oflag`, lowercase output characters (a-z) are mapped to uppercase (A-Z).
- Applied during output processing in `process_output_byte()` alongside existing OPOST/ONLCR/OCRNL transforms.
- Inverse of IUCLC — used by legacy terminals that only support uppercase.

### 46.4 Verification

- Test: ECHOPRT erase produces `\chars/` format.
- Test: ECHOPRT ends `\...` sequence on non-erase input with `/`.
- Test: IUCLC maps A-Z to a-z in input.
- Test: IUCLC does not affect non-alpha or already-lowercase characters.
- Test: OLCUC maps a-z to A-Z in output.
- Test: all three flags disabled by default (no effect in default termios).
- `just build` + `just test` gate.

### 46.5 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `ECHOPRT`, `IUCLC`, `OLCUC` flag constants |
| `drivers/src/tty/ldisc.rs` | ECHOPRT erase state machine with `in_erase_seq` tracking, IUCLC mapping in input path, OLCUC mapping in output path |
| `drivers/src/tty_tests.rs` | Tests for all three flags, erase sequence state transitions, case mapping edge cases |

## 47. File Inventory

### New files

| File | Purpose |
|------|---------|
| `drivers/src/tty/mod.rs` | TTY public API (replaces `tty.rs`) |
| `drivers/src/tty/driver.rs` | `TtyDriver` trait, `SerialConsoleDriver`, `VConsoleDriver` |
| `drivers/src/tty/table.rs` | `TTY_SLOTS` global array, init, lookup |
| `drivers/src/tty/ldisc.rs` | Enhanced `LineDisc`, `RawDisc`, `LdiscKind` abstraction |
| `drivers/src/tty/session.rs` | `TtySession`, foreground checks, session policy, `SessionId`/`ProcessGroupId` newtypes |
| `drivers/src/tty/pty.rs` | PTY pair allocation, constructors, free-slot helpers (Phase 17) |
| `drivers/src/tty/vconsole.rs` | Framebuffer-backed virtual console renderer (Phase 22) |
| `drivers/src/tty/vtparser.rs` | VT100/ANSI escape sequence state machine (Phase 40, planned) |

### Modified files

| File | Nature of change |
|------|-----------------|
| `drivers/src/lib.rs` | Update `mod tty` to module dir |
| `drivers/src/ps2/keyboard.rs` | Remove `input_route_key_event`, call `tty::push_input` |
| `fs/src/fileio.rs` | Replace `console` bool with `tty_index`, update routing, `/dev/tty` + `/dev/ptmx` + `/dev/pts/N` intercepts, `O_NOCTTY` handling |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Per-TTY ioctl dispatch, `TCSETSW`/`TCSETSF`/`TIOCSETD`/`TIOCGETD`/`TIOCGPTN` |
| `core/src/scheduler/task.rs` | Add `controlling_tty`, `pgrp`, `session_id` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update to per-TTY API, add ldisc/PTY services |
| `lib/src/kernel_services/driver_runtime.rs` | Add `signal_session()` service |
| `core/src/driver_hooks.rs` | Wire `signal_session()` implementation |
| `abi/src/syscall.rs` | Add iflag/oflag/lflag constants, new c_cc indices, `O_NOCTTY`, `TIOCSETD`/`TIOCGETD`/`TIOCGPTN`, `N_TTY`/`N_RAW` |
| `abi/src/signal.rs` | Canonical source for all signal constants, add `SIGWINCH` |

### Deleted files

| File | Reason |
|------|--------|
| `drivers/src/tty.rs` | Replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | Replaced by `drivers/src/tty/ldisc.rs` |

---

## 48. Appendix: Linux N_TTY Reference

For implementation reference, Linux's N_TTY line discipline (`drivers/tty/n_tty.c`) handles:

1. **Input processing** (`n_tty_receive_char_inline`):
   - c_iflag: ISTRIP, IGNCR, ICRNL, INLCR, IUCLC
   - Signal chars: VINTR→SIGINT, VQUIT→SIGQUIT, VSUSP→SIGTSTP
   - Flow control: VSTOP/VSTART with IXON
   - Canonical editing: VERASE, VWERASE, VKILL, VREPRINT, VLNEXT
   - Echo processing with column tracking

2. **Output processing** (`do_output_char`):
   - c_oflag: OPOST, ONLCR, OCRNL, ONOCR, ONLRET
   - Tab expansion (TABDLY)
   - Column tracking for proper backspace echo

3. **Read** (`n_tty_read`):
   - Canonical: return on newline or EOF — **one line at a time** via `canon_head`/`line_start`
   - Non-canonical: VMIN/VTIME timing (inter-byte for VMIN>0/VTIME>0)
   - Job control: SIGTTIN for background reads, SIGTTOU for background writes with TOSTOP

4. **Lifecycle** (`tty_io.c`):
   - Refcounted via `tty_kref` — last close triggers cleanup
   - `tty_hangup()` signals entire session, flushes buffers, wakes blocked readers
   - TCSETSW drains output via `tty_wait_until_sent()` before changing termios
   - TCSETSF drains output AND flushes input (`n_tty_flush_buffer()`)

5. **PTY** (`pty.c`):
   - Master and slave are separate `tty_struct` instances linked via `tty->link`
   - Master write → `pty_write()` → feeds bytes to slave's `receive_buf()` (through slave's ldisc)
   - Slave write → output processed through slave's ldisc → stored in `tty->write_buf` → master reads it
   - Master close → `pty_close()` → slave gets hangup

The SlopOS implementation follows this structure but simplified for `no_std` Rust. Phases 1–28 built the core subsystem, closed critical semantic gaps, and established a type-safe termios foundation; Phases 29–42 target dispatch consolidation, `/dev/tty` magic device, complete POSIX termios flag coverage (including IUTF8, IUCLC, OLCUC, EXTPROC), PTY namespace/packet mode for multiplexer support, and VT100/ANSI terminal emulation.
