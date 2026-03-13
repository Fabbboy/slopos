# SlopOS TTY Finishing Touches Plan

> **Status**: 22-phase gold-standard roadmap — Phases 1–17 COMPLETE, Phases 18–22 pending (post-audit hardening)
> **Predecessor**: TTY Overhaul Plan (42 phases, all complete) — the foundational rewrite from global singleton to per-terminal subsystem
> **Target**: Close the remaining architectural gaps identified in the Linux N_TTY / RedoxOS / Asterinas comparative review, bringing the TTY subsystem to production-grade quality
> **Current**: `drivers/src/tty/` — 14 files, ~7900 lines, ~1579 regression tests. Clean per-TTY API, PTY with generation-safe peer handles, per-slot locking, full POSIX termios flag coverage (c_iflag, c_oflag, c_lflag, c_cc), session/job control, VT100 emulation with UTF-8 + 256-color/truecolor + DEC private modes, packet mode, EXTPROC, vhangup. Zero TODO/FIXME/HACK comments. Module decomposition complete — `mod.rs` slimmed from ~2543 to ~239 lines with focused sub-modules for I/O, termios, job control, lifecycle, and poll. POSIX controlling terminal semantics hardened (PTY master ctty guard, TIOCSPGRP ctty validation, fg_pgrp change wake).
> **Post-Audit**: Phases 17–22 address findings from a comprehensive gold-standard audit against Linux N_TTY, RedoxOS, Asterinas, and POSIX.1-2024. Focus: POSIX semantic correctness, Rust idiomaticity, encapsulation, and forward-looking architecture.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State — What the Overhaul Delivered](#2-current-state--what-the-overhaul-delivered)
3. [Gap Analysis Summary](#3-gap-analysis-summary)
4. [Phase 1: Per-TTY Poll Notification](#4-phase-1-per-tty-poll-notification)
5. [Phase 2: PTY Flow Control (Throttle Mechanism)](#5-phase-2-pty-flow-control-throttle-mechanism)
6. [Phase 3: Cooked Buffer Overflow Hardening](#6-phase-3-cooked-buffer-overflow-hardening)
7. [Phase 4: c_cflag ABI Completion](#7-phase-4-c_cflag-abi-completion)
8. [Phase 5: Missing Ioctls (TCFLSH, TCSBRK, TCXONC)](#8-phase-5-missing-ioctls-tcflsh-tcsbrk-tcxonc)
9. [Phase 6: Edit Buffer Expansion](#9-phase-6-edit-buffer-expansion)
10. [Phase 7: Signal Restart Infrastructure (ERESTARTSYS)](#10-phase-7-signal-restart-infrastructure-erestartsys)
11. [Phase 8: TCXONC Behavioral Completion (Tier 1)](#11-phase-8-tcxonc-behavioral-completion-tier-1)
12. [Phase 9: Output Queue Visibility (TIOCOUTQ) (Tier 1)](#12-phase-9-output-queue-visibility-tiocoutq-tier-1)
13. [Phase 10: Input Wake Batching (WAKEUP_CHARS-style) (Tier 2)](#13-phase-10-input-wake-batching-wakeup_chars-style-tier-2)
14. [Phase 11: TABDLY/XTABS Output Compatibility (Tier 2)](#14-phase-11-tabdlyxtabs-output-compatibility-tier-2)
15. [Phase 12: no_room-style Overflow Recovery (Tier 3)](#15-phase-12-no_room-style-overflow-recovery-tier-3)
16. [Phase 13: Output Drain Semantics Hardening (Tier 3)](#16-phase-13-output-drain-semantics-hardening-tier-3)
17. [Phase 14: Core Semantic Correctness (Gold Standard Audit)](#17-phase-14-core-semantic-correctness-gold-standard-audit)
18. [Phase 15: VConsole Unicode & Broadened Xterm Emulation](#18-phase-15-vconsole-unicode--broadened-xterm-emulation)
19. [Phase 16: mod.rs Module Decomposition](#19-phase-16-modrs-module-decomposition)
20. [Phase 17: POSIX Controlling Terminal Semantics](#20-phase-17-posix-controlling-terminal-semantics)
21. [Phase 18: TIOCOUTQ Byte Accounting & Packet Mode Edge Fix](#21-phase-18-tiocoutq-byte-accounting--packet-mode-edge-fix)
22. [Phase 19: Missing Ioctls (TIOCGSID, TIOCEXCL) & HUPCL Enforcement](#22-phase-19-missing-ioctls-tiocgsid-tiocexcl--hupcl-enforcement)
23. [Phase 20: Rust Encapsulation & Type Safety](#23-phase-20-rust-encapsulation--type-safety)
24. [Phase 21: Deferred Actions RAII & Boilerplate Reduction](#24-phase-21-deferred-actions-raii--boilerplate-reduction)
25. [Phase 22: Forward-Looking — TIOCGPTPEER & Flip-Buffer Architecture](#25-phase-22-forward-looking--tiocgptpeer--flip-buffer-architecture)
26. [File Inventory](#26-file-inventory)
27. [Appendix: Review Findings Reference](#27-appendix-review-findings-reference)

---

## 1. Executive Summary

The 42-phase TTY overhaul transformed SlopOS from a global singleton line discipline behind a single `IrqMutex` into a proper per-terminal subsystem with PTY support, session/job control, VT100 emulation, and near-complete POSIX termios coverage. The subsystem is genuinely well-engineered — generation-tagged PTY peer handles, type-safe bitflags, deferred signal delivery outside locks, and a clean split-write pattern are all production-quality patterns.

A comparative review against Linux N_TTY and RedoxOS identified **7 initial gaps**, all addressed in Phases 1-7. A follow-up gold-standard pass identified **6 additional Tier 1-3 gaps** focused on behavior parity, queue visibility, and throughput/recovery hardening (Phases 8-16). A comprehensive post-audit against Linux N_TTY, RedoxOS, Asterinas, and POSIX.1-2024 identified **6 more improvement areas** focused on POSIX semantic correctness, Rust idiomaticity, and forward-looking architecture (Phases 17-22).

### Summary of phases

| Phase | What | Priority | Effort | Status |
|-------|------|----------|--------|--------|
| 1 | Per-TTY poll notification (replace thundering herd) | P0 | Small | **DONE** |
| 2 | PTY flow control / throttle mechanism | P0 | Medium | **DONE** ✅ |
| 3 | Cooked buffer overflow hardening | P1 | Small | **DONE** ✅ |
| 4 | c_cflag ABI completion (constants + defaults) | P1 | Small | **DONE** ✅ |
| 5 | Missing ioctls (TCFLSH, TCSBRK, TCXONC) | P1 | Small | **DONE** ✅ |
| 6 | Edit buffer expansion (1024 → 4096) | P2 | Trivial | **DONE** ✅ |
| 7 | Signal restart infrastructure (ERESTARTSYS) | P2 | Large | **DONE** ✅ |
| 8 | TCXONC behavioral completion (real flow control semantics) | P0 | Medium | **DONE** ✅ |
| 9 | Output queue visibility (`TIOCOUTQ`) | P0 | Small | **DONE** ✅ |
| 10 | Input wake batching (`WAKEUP_CHARS`-style) | P1 | Medium | **DONE** ✅ |
| 11 | `TABDLY`/`XTABS` output compatibility | P1 | Small | **DONE** ✅ |
| 12 | `no_room`-style overflow recovery | P1 | Medium | **DONE** ✅ |
| 13 | Output drain semantics hardening | P2 | Small | **DONE** ✅ |
| 14 | Core semantic correctness (typed input, interruptible waits, job control, PTY ABI, batched ingress) | P0 | Large | **DONE** ✅ |
| 15 | VConsole Unicode & broadened xterm emulation | P1 | Medium-Large | **DONE** ✅ |
| 16 | `mod.rs` module decomposition | P2 | Medium | **DONE** ✅ |
| | | | | |
| **Post-Audit Hardening (Phases 17–22)** | | | | |
| 17 | POSIX controlling terminal semantics (ctty guard, O_NOCTTY, TIOCSPGRP validation, fg_pgrp wake) | P0 | Medium | **DONE** ✅ |
| 18 | TIOCOUTQ byte accounting & packet mode edge fix | P1 | Small | Pending |
| 19 | Missing ioctls (TIOCGSID, TIOCEXCL) & HUPCL enforcement | P1 | Small | Pending |
| 20 | Rust encapsulation & type safety (privatize fields, TtyFlags, remove redundant state) | P2 | Medium | Pending |
| 21 | Deferred actions RAII & boilerplate reduction | P2 | Medium | Pending |
| 22 | Forward-looking: TIOCGPTPEER & flip-buffer architecture | P3 | Medium-Large | Pending |

---

## 2. Current State — What the Overhaul Delivered

### Architecture strengths (keep as-is)

| Pattern | Quality | Notes |
|---------|---------|-------|
| Per-slot `IrqMutex` with documented lock ordering | Excellent | Cleaner than Linux's `tty_mutex` → `tty_lock` → `ldisc_sem` chain |
| Generation-tagged `PtyPeerHandle` | Better than Linux | Prevents stale-slot misrouting after free/reuse — a class of CVE that Linux has had |
| Split-write (process under lock, hardware IO unlocked) | Matches Linux | Achieved without separate `tty_port` abstraction |
| Type-safe bitflags + `CcIndex` enum | Better than Linux | Linux uses raw `unsigned int` for everything |
| `NonZeroU32` newtypes for `SessionId`/`ProcessGroupId` | Better than Linux | Niche optimization + type safety vs Linux's `pid_t` |
| `TtyError` with 11 variants + `to_errno()` | Better than Linux | More structured than ad-hoc `-EINVAL`/`-EIO` returns |
| All 4 POSIX VMIN/VTIME cases | Correct | Implemented via `wait_event_timeout` |
| UTF-8 aware editing (IUTF8) | Matches Linux | Proper multi-byte backspace with codepoint width tracking |
| Deferred signal delivery outside locks | Correct | The #1 mistake hobby OS TTY implementations make — SlopOS gets it right |
| Post-hangup hardening (EOF/EIO/POLLHUP) | Matches Linux | `hung_up` + `peer_closed` flags with consistent behavior |
| VT100 parser separated from renderer | Better than RedoxOS | RedoxOS mixes these; SlopOS's separation is cleaner |

### Termios coverage

| Category | Coverage | Detail |
|----------|----------|--------|
| c_iflag | **14/14** | All POSIX input flags implemented |
| c_oflag | **6/6 core** | OPOST, ONLCR, OCRNL, ONOCR, ONLRET, OLCUC. Missing only legacy delay flags (OFILL/OFDEL) |
| c_lflag | **15/15** | All flags including IUTF8, EXTPROC, ECHOPRT. Missing only FLUSHO (deprecated in Linux) |
| c_cflag | **Expanded** | Character size/parity/baud/modem-control constants added in Phase 4 |
| c_cc | **17/17** | All control character indices implemented |
| Ioctls | **Full coverage** | Missing only `TIOCSTI` (deferred for security reasons). `TIOCOUTQ` added in Phase 9. |

---

## 3. Gap Analysis Summary

Gaps identified by comparative review against Linux `drivers/tty/n_tty.c` (~3500 lines) and RedoxOS `ptyd`.

### Critical (will cause real bugs or data loss)

| ID | Gap | Linux equivalent | Impact |
|----|-----|-----------------|--------|
| C1 | Global `POLL_NOTIFY` WaitQueue | Per-TTY `wait_address` in `tty_poll()` | Thundering herd: every poll on ANY TTY wakes ALL pollers. Spurious wakeups waste CPU. |
| C2 | No PTY flow control | `TTY_THROTTLED` flag + `tty_throttle()`/`tty_unthrottle()` | Rapid PTY master writes silently overflow slave's cooked buffer. Data loss. |

### Important (usability limiters)

| ID | Gap | Linux equivalent | Impact |
|----|-----|-----------------|--------|
| I1 | c_cflag nearly empty | 30+ constants (CS5-CS8, PARENB, PARODD, B*, CRTSCTS, CLOCAL, HUPCL) | `stty -a` shows garbage. Programs that roundtrip `tcgetattr/tcsetattr` clear unknown bits. |
| I2 | No signal restart | `ERESTARTSYS` + `SA_RESTART` | Every TTY read needs userland retry loop. Window resize (`SIGWINCH`) causes input glitches. |
| I3 | Missing TCFLSH ioctl | `tcflush()` via `TCFLSH` | Programs cannot discard stale input/output. |
| I4 | Edit buffer 1024 bytes | 4096 bytes | Long pastes and commands truncated in canonical mode. |

### Nice-to-have (fine for hobby OS)

| ID | Gap | Notes |
|----|-----|-------|
| N1 | Separate echo buffer | Cosmetic — prevents interleaved echo from concurrent writers |
| N2 | `tty_port` abstraction | Only needed for USB-serial hotplug or real hardware |
| N3 | Async write buffer | Only matters for real 9600 baud serial, not QEMU virtio |
| N4 | Output delay flags (OFILL/OFDEL) | Legacy terminal delays, almost nothing uses them |

---

## 4. Phase 1: Per-TTY Poll Notification

**Status**: **DONE** — Replaced global `POLL_NOTIFY` with `TTY_POLL_WAITERS[MAX_TTYS]`, added `WaitQueue::enqueue_current()`/`remove_current()` for multi-queue poll registration, updated poll/select handlers to collect TTY indices and sleep per-slot, 6 regression tests added.

> **Priority**: P0 performance/correctness — the global `POLL_NOTIFY` creates a thundering herd that wakes every poller on every TTY event.
> **Principle**: The infrastructure for per-slot notification already exists (`TTY_INPUT_WAITERS`, `TTY_OUTPUT_WAITERS` in `table.rs`). This phase replaces the single global `POLL_NOTIFY` with per-slot poll wake targeting.

### 4.1 Problem statement

Currently, `POLL_NOTIFY` is a single global `WaitQueue` in `table.rs`. When any TTY has a state change (input available, output space, hangup), it calls `POLL_NOTIFY.wake_all()`. Every process blocked in `poll()`/`select()` on ANY TTY wakes up, checks its specific TTY, finds no data (unless it's the right TTY), and goes back to sleep.

With 32 PTY slots and any shell-heavy workload, this creates:
- O(n) wakeups per event instead of O(1)
- Spurious wakeups that waste CPU time in a kernel that can't afford it
- False readiness reports that require re-checking under lock

### 4.2 Replace `POLL_NOTIFY` with per-slot notification

- Remove the global `POLL_NOTIFY: WaitQueue` from `table.rs`.
- Add `TTY_POLL_WAITERS: [WaitQueue; MAX_TTY_SLOTS]` alongside the existing `TTY_INPUT_WAITERS` and `TTY_OUTPUT_WAITERS` arrays (or reuse the existing arrays directly for poll).
- All poll registration calls (`poll_register` or equivalent) must specify the target TTY index to select the correct per-slot WaitQueue.
- All poll wake calls must target the specific slot's WaitQueue.

### 4.3 Update wake sites

Audit every call to `POLL_NOTIFY.wake_all()` and replace with the slot-specific equivalent:
- `push_input()` → wake `TTY_POLL_WAITERS[slot]`
- `write()` completion → wake `TTY_POLL_WAITERS[slot]`
- `hangup()` → wake `TTY_POLL_WAITERS[slot]`
- `peer_closed` transitions → wake `TTY_POLL_WAITERS[slot]`
- PTY cross-wake (master event wakes slave pollers and vice versa) → wake `TTY_POLL_WAITERS[peer_slot]`

### 4.4 Update poll registration sites

Audit `poll_ioctl_handlers.rs` and `fileio.rs` for poll registration paths:
- Each `poll()` call on a TTY FD must register on the correct slot's WaitQueue, not the global one.
- The FD already carries `tty_index` — use it to index into the per-slot array.

### 4.5 PTY cross-slot consideration

A PTY master polling for readability needs to wake when the slave writes (and vice versa). Ensure:
- Slave write → wake `TTY_POLL_WAITERS[master_slot]`
- Master write (push_input to slave) → wake `TTY_POLL_WAITERS[slave_slot]`
- Peer hangup → wake `TTY_POLL_WAITERS[peer_slot]`

The generation-tagged `PtyPeerHandle` already provides safe peer slot resolution.

### 4.6 Verification

- Test: two PTYs active, poll on PTY-A does NOT wake when PTY-B receives input.
- Test: poll on PTY slave wakes when master writes to it.
- Test: poll on PTY master wakes when slave writes to it.
- Test: poll returns `POLLHUP` on hangup (per-slot, not broadcast).
- Test: console TTY poll still works correctly.
- Regression: all existing poll/read/write tests pass unchanged.
- `just build` + `just test` gate.

### 4.7 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/table.rs` | Remove `POLL_NOTIFY`, add `TTY_POLL_WAITERS: [WaitQueue; MAX_TTY_SLOTS]` (or document reuse of existing per-slot waiters) |
| `drivers/src/tty/mod.rs` | Replace all `POLL_NOTIFY.wake_all()` calls with `TTY_POLL_WAITERS[idx].wake_all()` |
| `drivers/src/tty/pty.rs` | Update PTY cross-wake to target peer slot's poll waiter |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Update poll registration to use per-slot WaitQueue |
| `fs/src/fileio.rs` | Update any poll-related routing to pass TTY index for per-slot registration |
| `drivers/src/tty_tests.rs` | Per-slot poll isolation tests, PTY cross-wake tests |

---

## 5. Phase 2: PTY Flow Control (Throttle Mechanism)

**Status**: **DONE** ✅

> **Priority**: P0 correctness — without throttling, rapid PTY master writes silently overflow the slave's cooked buffer, causing data loss.
> **Principle**: Linux's `TTY_THROTTLED` flag is a proven pattern. When the slave's input buffer fills, the master must be back-pressured. This is not optional for any terminal multiplexer (tmux, screen, ssh) to function correctly.

### 5.1 Problem statement

When a PTY master writes faster than the slave consumer reads (e.g., `cat /dev/urandom > /dev/pts/0`, or any burst of output from a remote process), the data flows through `master_write()` → `push_input()` on the slave → line discipline processing → `push_cooked()`. When the cooked ring buffer (`COOKED_BUF_SIZE = 4096`) is full, `push_cooked()` silently returns without enqueuing. The master's `write()` call returns success — the master believes the data was delivered.

This is **silent data loss**. The master has no way to know bytes were dropped.

### 5.2 Add throttle state

- Add `throttled: bool` field to `Tty` struct.
- Define high-water mark: `THROTTLE_HIGH_WATER = COOKED_BUF_SIZE * 3 / 4` (3072 bytes, 75% capacity).
- Define low-water mark: `THROTTLE_LOW_WATER = COOKED_BUF_SIZE / 4` (1024 bytes, 25% capacity).

### 5.3 Throttle activation (in slave's input path)

After `push_cooked()` in `push_input()` and `drain_hw_input_locked()`:
- Check cooked buffer occupancy.
- If occupancy ≥ `THROTTLE_HIGH_WATER` and `!throttled`: set `throttled = true`.
- Wake any master-side poll waiters with `POLLOUT` cleared (master should stop writing).

### 5.4 Master write back-pressure

In the PTY master `write()` path (the code that calls `push_input()` on the slave):
- Before writing, check the slave's `throttled` flag.
- If throttled:
  - **Blocking mode**: Block on the slave's output WaitQueue until `throttled` is cleared (or hangup/signal).
  - **Non-blocking mode** (`O_NONBLOCK`): Return `WouldBlock` / `-EAGAIN`.
- Partial writes: If some bytes were accepted before throttle triggers, return the count of bytes successfully written (short write). The caller retries the rest.

### 5.5 Unthrottle (in slave's read path)

After consuming data from the cooked buffer in `read()`:
- Check cooked buffer occupancy.
- If occupancy ≤ `THROTTLE_LOW_WATER` and `throttled`: set `throttled = false`.
- Wake the master-side output WaitQueue (master can resume writing).
- Wake master-side poll waiters with `POLLOUT` set.

### 5.6 Interaction with existing IXOFF

IXOFF is terminal-to-host software flow control (sends XON/XOFF characters). The throttle mechanism is buffer-level back-pressure (blocks/returns EAGAIN on the writing syscall). They are complementary:
- IXOFF: signals the remote terminal device to pause sending.
- Throttle: blocks the local master-side writer.

Both can coexist. The IXOFF watermarks (80%/20%) may be aligned with throttle watermarks for consistency, but they serve different purposes.

### 5.7 Verification

- Test: master writes more than `COOKED_BUF_SIZE` without slave reading → master blocks (blocking mode) or returns `-EAGAIN` (non-blocking).
- Test: slave reads some data → master unblocks and can continue writing.
- Test: partial write — master writes 5000 bytes, only ~3072 accepted before throttle → returns 3072 (short write).
- Test: throttle/unthrottle cycle — verify no data loss across multiple fill/drain cycles.
- Test: hangup while master is blocked on throttle → master unblocked with error.
- Test: signal while master is blocked on throttle → master returns `-EINTR`.
- Test: non-PTY TTYs (serial, vconsole) unaffected by throttle mechanism.
- Regression: all existing PTY read/write/lifecycle tests pass.
- `just build` + `just test` gate.

### 5.8 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add `throttled: bool` to `Tty`, add `THROTTLE_HIGH_WATER`/`THROTTLE_LOW_WATER` constants, throttle check in `push_input()`, unthrottle check in `read()` |
| `drivers/src/tty/pty.rs` | Add back-pressure logic in master write path: check slave throttle, block or return EAGAIN, handle short writes |
| `drivers/src/tty/ldisc.rs` | Expose cooked buffer occupancy method for throttle watermark checks |
| `drivers/src/tty/table.rs` | Potentially add per-slot throttle WaitQueue (or reuse `TTY_OUTPUT_WAITERS`) |
| `drivers/src/tty_tests.rs` | Throttle activation, unthrottle, short write, hangup-while-throttled, signal-while-throttled, non-PTY isolation tests |

---

## 6. Phase 3: Cooked Buffer Overflow Hardening

**Status**: **DONE** ✅

> **Priority**: P1 correctness — even with Phase 2's throttle mechanism, the cooked buffer overflow path should be explicitly safe rather than silently dropping.
> **Principle**: Defense in depth. The throttle prevents overflow under normal conditions, but the overflow path itself should be hardened for edge cases (race conditions, non-PTY sources, direct `push_input` callers).

### 6.1 Problem statement

`push_cooked()` in `ldisc.rs` silently drops bytes when the cooked ring buffer is full. The IMAXBEL bell is only wired for the edit buffer (canonical mode input), not for the cooked buffer. There is no error return, no flag, no diagnostic.

### 6.2 Return value for `push_cooked()`

- Change `push_cooked()` to return `bool` (or `Result<(), ()>`) indicating whether the byte was enqueued.
- Callers can then decide how to handle the failure:
  - In canonical mode (`flush_edit_to_cooked`): if cooked buffer is full during edit-to-cooked flush, this is an internal error — log or panic (should never happen if edit buffer ≤ cooked buffer).
  - In non-canonical/raw mode: the byte is genuinely lost. The caller should propagate the failure.

### 6.3 IMAXBEL for cooked buffer overflow

- When `IMAXBEL` is set and a byte is dropped due to cooked buffer full (in non-canonical or raw input), ring the bell (emit BEL `\x07` to output) — matching the existing IMAXBEL behavior for edit buffer overflow in canonical mode.
- This gives the user audible feedback that input is being lost.

### 6.4 Diagnostic counter (optional)

- Add `cooked_overflow_count: u32` to the line discipline state.
- Increment on each dropped byte.
- Expose via a debug ioctl or log message for diagnostics.
- This is optional but useful for debugging flow control issues.

### 6.5 Verification

- Test: cooked buffer full + IMAXBEL set → BEL output on overflow.
- Test: cooked buffer full + IMAXBEL not set → silent drop (current behavior preserved for compatibility).
- Test: `push_cooked()` returns failure when buffer full.
- Test: canonical mode flush never hits cooked overflow (edit buffer fits in cooked buffer).
- Regression: all existing input/output tests pass.
- `just build` + `just test` gate.

### 6.6 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Change `push_cooked()` return type, add IMAXBEL bell on cooked overflow, optional overflow counter |
| `drivers/src/tty/mod.rs` | Update callers of `push_cooked()` to handle failure return, wire IMAXBEL bell output |
| `drivers/src/tty_tests.rs` | Cooked overflow return value tests, IMAXBEL bell tests, canonical flush safety tests |

---

## 7. Phase 4: c_cflag ABI Completion

**Status**: **DONE** ✅

> **Priority**: P1 compatibility — `c_cflag` currently defines only `CREAD`. Any program that reads termios attributes sees missing character size, parity, baud rate, and modem control flags. Programs that roundtrip `tcgetattr`/`tcsetattr` will clear unknown bits.
> **Principle**: These are pure ABI definitions with zero runtime cost. The kernel stores and returns them faithfully. For PTYs and QEMU serial, no hardware action is needed — but userland programs expect the constants to exist and the defaults to be sane.

### 7.1 Character size flags

Add to `ControlFlags` in `abi/src/syscall.rs`:

```
CSIZE  = 0o000060   // Character size mask
CS5    = 0o000000   // 5 bits
CS6    = 0o000020   // 6 bits
CS7    = 0o000040   // 7 bits
CS8    = 0o000060   // 8 bits
```

### 7.2 Parity flags

```
PARENB  = 0o000400   // Enable parity generation/checking
PARODD  = 0o001000   // Odd parity (when PARENB set)
```

### 7.3 Stop bits and modem control

```
CSTOPB  = 0o000100   // 2 stop bits (else 1)
HUPCL   = 0o002000   // Hang up on last close
CLOCAL  = 0o004000   // Ignore modem status lines
```

### 7.4 Hardware flow control

```
CRTSCTS = 0o020000000   // RTS/CTS hardware flow control
```

### 7.5 Baud rate constants

Add standard baud rate constants. These use the Linux encoding (baud rate encoded in c_cflag low bits + `CBAUD` mask, with extended rates via `CBAUDEX`):

```
CBAUD    = 0o010017   // Baud rate mask
B0       = 0o000000   // Hang up
B50      = 0o000001
B75      = 0o000002
B110     = 0o000003
B134     = 0o000004
B150     = 0o000005
B200     = 0o000006
B300     = 0o000007
B600     = 0o000010
B1200    = 0o000011
B1800    = 0o000012
B2400    = 0o000013
B4800    = 0o000014
B9600    = 0o000015
B19200   = 0o000016
B38400   = 0o000017
CBAUDEX  = 0o010000
B57600   = 0o010001
B115200  = 0o010002
B230400  = 0o010003
B460800  = 0o010004
B500000  = 0o010005
B576000  = 0o010006
B921600  = 0o010007
B1000000 = 0o010010
B1152000 = 0o010011
B1500000 = 0o010012
B2000000 = 0o010013
B2500000 = 0o010014
B3000000 = 0o010015
B3500000 = 0o010016
B4000000 = 0o010017
```

### 7.6 Default termios c_cflag

Update the default `Termios` constructor (used for new TTYs, both console and PTY):

```rust
c_cflag: ControlFlags::from_bits_truncate(
    CREAD | CS8 | HUPCL | B38400
)
```

This matches Linux's default: 8 data bits, 1 stop bit, no parity, receiver enabled, hang up on close, 38400 baud.

### 7.7 c_ispeed / c_ospeed population

The `UserTermios` struct already has `c_ispeed` and `c_ospeed` fields. Populate them from the baud rate encoded in `c_cflag` when returning termios to userland:
- Extract baud from `c_cflag & CBAUD`.
- Map to numeric speed (e.g., `B38400` → 38400).
- Set `c_ispeed = c_ospeed = numeric_speed`.

When setting termios from userland:
- If `c_ispeed`/`c_ospeed` are provided, encode back into `c_cflag`.
- If only `c_cflag` baud bits are set, derive speed from those (Linux `termios2` behavior).

### 7.8 Verification

- Test: default termios `c_cflag` contains `CS8 | CREAD | HUPCL | B38400`.
- Test: `tcgetattr` returns all new flag bits correctly.
- Test: `tcsetattr` with `CS7 | PARENB` roundtrips through `tcgetattr` unchanged.
- Test: baud rate constants are defined with correct values (compile-time assertions).
- Test: `c_ispeed`/`c_ospeed` populated correctly from default baud.
- Regression: all existing termios tests pass (new flags don't break existing flag handling).
- `just build` + `just test` gate.

### 7.9 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add CS5-CS8, CSIZE, PARENB, PARODD, CSTOPB, HUPCL, CLOCAL, CRTSCTS, CBAUD, CBAUDEX, all B* constants to `ControlFlags` |
| `drivers/src/tty/ldisc.rs` | Update default termios constructor to set `CS8 \| CREAD \| HUPCL \| B38400` |
| `drivers/src/tty/mod.rs` | Add `c_ispeed`/`c_ospeed` population logic in termios get/set paths |
| `drivers/src/tty_tests.rs` | Default cflag tests, roundtrip tests, baud rate constant value assertions, speed field tests |

---

## 8. Phase 5: Missing Ioctls (TCFLSH, TCSBRK, TCXONC)

**Status**: **DONE** ✅

> **Priority**: P1 compatibility — `TCFLSH` (`tcflush()`) is used by programs that need to discard stale input before reading fresh input (e.g., after a mode change). `TCSBRK` and `TCXONC` are less critical but expected by `stty` and libc.
> **Principle**: Implement `TCFLSH` fully (it's a simple buffer clear). Stub `TCSBRK` and `TCXONC` as harmless no-ops for now — real break signaling and explicit XON/XOFF control are edge cases in a QEMU-only environment.

### 8.1 TCFLSH — Flush queues

Implement `TCFLSH` ioctl (maps to libc `tcflush(fd, queue_selector)`):

| Argument | Action |
|----------|--------|
| `TCIFLUSH` (0) | Flush input buffer: clear edit buffer + cooked ring buffer + reset line_count |
| `TCOFLUSH` (1) | Flush output buffer: clear any pending output (reset `TTY_OUTPUT_INFLIGHT` for the slot) |
| `TCIOFLUSH` (2) | Flush both input and output |

Implementation path:
- Add `TCFLSH` constant to `abi/src/syscall.rs` (value `0x540B`, matching Linux).
- Add `TCIFLUSH`, `TCOFLUSH`, `TCIOFLUSH` constants (0, 1, 2).
- Add `flush_input()` method to line discipline: clears edit buffer, cooked buffer, resets `line_count`, `edit_len`, cooked read/write pointers.
- Add `flush_output()` method: resets `TTY_OUTPUT_INFLIGHT[slot]` to 0.
- Wire through ioctl dispatch in `poll_ioctl_handlers.rs` → service bridge → `tty::tcflush()`.

### 8.2 TCSBRK — Send break (stub)

- Add `TCSBRK` constant to ABI (`0x5409`, matching Linux).
- Implementation: if argument is 0, this is "send break for 0.25 seconds". For QEMU serial, this is a no-op. For PTYs, this is a no-op (Linux also no-ops TCSBRK on PTYs).
- If argument is non-zero, this is `tcdrain()` (wait for output to complete) — delegate to existing output drain logic (`wait_output_idle`).
- Return 0 (success) in all cases.

### 8.3 TCXONC — Start/stop I/O (stub)

- Add `TCXONC` constant to ABI (`0x540A`, matching Linux).
- Arguments: `TCOOFF` (0) = suspend output, `TCOON` (1) = restart output, `TCIOFF` (2) = send XOFF, `TCION` (3) = send XON.
- For now, stub all four cases as no-ops returning 0. Wire to the existing IXON/IXOFF infrastructure if/when needed.
- Document the stub status in code comments.

### 8.4 ABI constant definitions

```rust
pub const TCSBRK:   u32 = 0x5409;
pub const TCXONC:   u32 = 0x540A;
pub const TCFLSH:   u32 = 0x540B;

pub const TCIFLUSH:  i32 = 0;
pub const TCOFLUSH:  i32 = 1;
pub const TCIOFLUSH: i32 = 2;

pub const TCOOFF: i32 = 0;
pub const TCOON:  i32 = 1;
pub const TCIOFF: i32 = 2;
pub const TCION:  i32 = 3;
```

### 8.5 Verification

- Test: `TCFLSH` with `TCIFLUSH` clears input buffer, pending read returns empty.
- Test: `TCFLSH` with `TCOFLUSH` resets output inflight counter.
- Test: `TCFLSH` with `TCIOFLUSH` clears both.
- Test: `TCFLSH` with invalid argument → `-EINVAL`.
- Test: `TCSBRK` with arg=0 returns success (no-op).
- Test: `TCSBRK` with arg>0 waits for output drain (verify drain behavior).
- Test: `TCXONC` with all four arguments returns success (stub behavior).
- Regression: all existing ioctl tests pass.
- `just build` + `just test` gate.

### 8.6 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `TCSBRK`, `TCXONC`, `TCFLSH`, `TCIFLUSH`/`TCOFLUSH`/`TCIOFLUSH`, `TCOOFF`/`TCOON`/`TCIOFF`/`TCION` constants |
| `drivers/src/tty/ldisc.rs` | Add `flush_input()` method: clear edit buffer, cooked ring, reset counters |
| `drivers/src/tty/mod.rs` | Add `tcflush()`, `tcsbrk()`, `tcxonc()` public API methods |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Wire `TCFLSH`, `TCSBRK`, `TCXONC` through ioctl dispatch |
| `lib/src/kernel_services/syscall_services/tty.rs` | Add service bridge methods for new ioctls |
| `drivers/src/syscall_services_init.rs` | Register new service methods |
| `drivers/src/tty_tests.rs` | TCFLSH input/output/both flush tests, TCSBRK drain test, TCXONC stub tests, invalid argument tests |

---

## 9. Phase 6: Edit Buffer Expansion

**Status**: **DONE** ✅

> **Priority**: P2 quality — a single constant change with no architectural impact.
> **Principle**: POSIX requires `MAX_CANON ≥ 255`, and the current 1024 is compliant. But real-world canonical-mode usage (terminal paste, long commands with history expansion, heredocs) regularly exceeds 1024. Linux and RedoxOS both use 4096.

### 9.1 Change

In `drivers/src/tty/ldisc.rs`:

```rust
// Before:
const EDIT_BUF_SIZE: usize = 1024;

// After:
const EDIT_BUF_SIZE: usize = 4096;
```

### 9.2 Memory impact

Each `LineDisc` instance gains 3072 bytes. With 32 TTY slots, the maximum additional memory is 32 × 3072 = 96 KiB. Slots are only allocated when a TTY is opened, so idle slots cost nothing.

For a `no_std` kernel running in QEMU with ≥128 MiB RAM, 96 KiB is negligible.

### 9.3 Verification

- Test: canonical mode input longer than 1024 bytes completes correctly (line boundary, echo, backspace all work).
- Test: paste of 3000-byte line into canonical mode → entire line delivered on read.
- Regression: all existing canonical/non-canonical tests pass (buffer is larger, not smaller — should be fully backward compatible).
- `just build` + `just test` gate.

### 9.4 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Change `EDIT_BUF_SIZE` from 1024 to 4096 |
| `drivers/src/tty_tests.rs` | Add large canonical input test (>1024 bytes) |

---

## 10. Phase 7: Signal Restart Infrastructure (ERESTARTSYS)

**Status**: **DONE** ✅

> **Priority**: P2 architecture — this is a cross-cutting concern that affects every blocking syscall, not just TTY. The TTY subsystem is the primary consumer, but the fix lives in the syscall return path.
> **Principle**: Linux's signal restart mechanism allows blocking reads to be transparently restarted after a signal is delivered, if the signal handler was registered with `SA_RESTART`. Without it, every TTY read in userland needs a manual retry loop around `-EINTR`. Programs like readline, vim, less, and bash depend on `SA_RESTART` working correctly.

### 10.1 Problem statement

When a process is blocked in `tty_read()` and a signal arrives (e.g., `SIGWINCH` from terminal resize, `SIGCHLD` from child exit), the read returns `SignalInterrupt` which maps to `-EINTR`. Userland must then decide whether to retry.

In Linux:
1. TTY read returns `-ERESTARTSYS` (not `-EINTR`).
2. The syscall return path checks: was the interrupting signal's handler registered with `SA_RESTART`?
3. If yes: the kernel transparently restarts the syscall (the userland process never sees `-EINTR`).
4. If no: the kernel converts `-ERESTARTSYS` to `-EINTR` and returns to userland.

### 10.2 Add ERESTARTSYS error code

- Define `ERESTARTSYS` as an internal-only error code (e.g., -512, matching Linux). This value must NEVER reach userland.
- Add it to `TtyError` as a distinct variant, or define it at the ABI level as an internal errno.

### 10.3 TTY read returns ERESTARTSYS

- Change the TTY read path: when interrupted by a signal, return `ERESTARTSYS` instead of `EINTR`.
- The TTY read must be in a restartable state: no partial side effects that prevent restart. This is naturally true for TTY reads — the read either returns data or doesn't.

### 10.4 Syscall return path restart logic

In the syscall dispatch/return path (likely `core/src/syscall/` handlers or the SYSRET trampoline):

```
if syscall_result == -ERESTARTSYS {
    let signal = current_pending_signal();
    if signal_has_sa_restart(signal) {
        // Restart: reset instruction pointer to syscall entry, re-enter syscall
        restart_syscall();
    } else {
        // Convert to EINTR for userland
        syscall_result = -EINTR;
    }
}
```

This requires:
- Access to the signal action table for the interrupting signal.
- Ability to reset the saved instruction pointer to re-execute the `syscall` instruction.
- Knowledge of which signal triggered the interruption.

### 10.5 SA_RESTART in signal actions

- Ensure `SA_RESTART` flag is defined in the signal action flags (`abi/src/signal.rs`).
- Ensure `sigaction()` syscall properly stores `SA_RESTART` in the signal action table.
- Add a helper: `signal_has_sa_restart(signum: u8) -> bool` that checks the current task's signal actions.

### 10.6 Other blocking syscalls

While this phase focuses on TTY, the same mechanism applies to:
- `read()` on pipes
- `read()` on sockets
- `wait()`/`waitpid()`
- `sleep()`/`nanosleep()`

All of these should return `-ERESTARTSYS` when interrupted by a restartable signal. However, only TTY is in scope for this phase. Document the pattern so other subsystems can adopt it.

### 10.7 Verification

- Test: TTY read with `SA_RESTART` set on `SIGWINCH` → read transparently restarts after window resize signal, no `-EINTR` visible to userland.
- Test: TTY read with `SA_RESTART` NOT set → read returns `-EINTR` to userland.
- Test: TTY read interrupted by `SIGINT` (which typically does NOT use `SA_RESTART`) → `-EINTR`.
- Test: partial canonical read (data available + signal) → data returned, no restart needed.
- Test: `ERESTARTSYS` never leaks to userland (assert in syscall return path).
- Regression: all existing signal delivery and TTY read tests pass.
- `just build` + `just test` gate.

### 10.8 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Define `ERESTARTSYS` internal error code (e.g., -512) |
| `abi/src/signal.rs` | Ensure `SA_RESTART` flag constant is defined |
| `drivers/src/tty/mod.rs` | Change signal-interrupted read to return `ERESTARTSYS` instead of `EINTR` |
| `core/src/syscall/core_handlers.rs` or `core/src/syscall/handlers.rs` | Add restart logic in syscall return path: check `SA_RESTART`, either restart or convert to `EINTR` |
| `core/src/scheduler/task.rs` | Expose `signal_has_sa_restart(signum) -> bool` helper |
| `lib/src/kernel_services/driver_runtime.rs` | Potentially add `signal_has_sa_restart` service |
| `drivers/src/tty_tests.rs` | Restart behavior tests, ERESTARTSYS-never-leaks test, SA_RESTART interaction tests |

---

## 11. Phase 8: TCXONC Behavioral Completion (Tier 1)

**Status**: **DONE** ✅ — Added `output_stopped: bool` to `Tty` struct with `TCOOFF`/`TCOON` setting/clearing the flag and blocking/resuming the write path. `TCIOFF`/`TCION` transmit `VSTOP`/`VSTART` control bytes to the terminal device. Write path enforces both ldisc `is_stopped()` (IXON keyboard flow control) and `output_stopped` (TCXONC ioctl flow control) independently. Packet mode events (`TIOCPKT_STOP`/`TIOCPKT_START`) emitted on transitions. Hangup clears `output_stopped` to unblock waiters. 12 regression tests added.

> **Priority**: P0 compatibility/correctness — `tcxonc()` currently validates action codes but performs no behavioral change, so userspace receives success for operations that did not happen.
> **Principle**: Keep SlopOS's simple architecture, but make `TCXONC` semantically honest and useful (Linux-inspired behavior, no copy-paste).

### 11.1 Problem statement

`tcxonc()` in `drivers/src/tty/mod.rs` accepts `TCOOFF`, `TCOON`, `TCIOFF`, `TCION` and returns `Ok(())`, but does not actually pause output, resume output, or inject STOP/START signaling.

### 11.2 Behavioral target

- `TCOOFF`: suspend output writes for the TTY.
- `TCOON`: resume suspended output writes.
- `TCIOFF`: send STOP control byte (software flow-control signal path).
- `TCION`: send START control byte.
- Invalid action: return `InvalidArg` (already implemented, preserve).

### 11.3 Implementation outline

- Add explicit output-stop state to `Tty` (`output_stopped: bool` or equivalent).
- In write path, block (or `EAGAIN` for nonblocking) while `output_stopped` is true.
- Wire `TCIOFF`/`TCION` to existing line-discipline control-byte path (reuse `VSTOP`/`VSTART` handling semantics).
- Wake waiters (`TTY_OUTPUT_WAITERS`, poll waiters) when `TCOON` clears the stop state.

### 11.4 Verification

- Test: `tcxonc(TCOOFF)` pauses writes; blocking writer sleeps.
- Test: `tcxonc(TCOON)` resumes blocked writer.
- Test: nonblocking writer under `TCOOFF` returns `-EAGAIN`.
- Test: `TCIOFF`/`TCION` trigger control-byte behavior path.
- Regression: existing TCXONC argument-validation tests continue to pass.
- `just build` + `just test` gate.

### 11.5 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Implement real `TCXONC` behavior and write-path stop/resume handling |
| `drivers/src/tty/ldisc.rs` | Expose/control START/STOP byte integration hooks if needed |
| `drivers/src/tty_tests.rs` | Add behavioral TCXONC tests (pause/resume/nonblocking) |

---

## 12. Phase 9: Output Queue Visibility (TIOCOUTQ) (Tier 1)

**Status**: **DONE** ✅ — Added `TIOCOUTQ` constant (0x5411) to ABI, implemented `output_queued_bytes()` helper combining `TTY_OUTPUT_INFLIGHT` counter with driver `output_pending()` state, wired through the full ioctl dispatch chain (service bridge → adapter → poll_ioctl_handlers dispatch), 8 regression tests added covering: ABI constant value, idle/inflight/post-flush queue depth, unallocated/invalid index error paths, FIONREAD unchanged regression, and vconsole driver behavior.

> **Priority**: P0 observability/compatibility — `TIOCOUTQ` is expected by terminal tooling and multiplexers for back-pressure-aware writes.
> **Principle**: Linux returns queued output bytes via `tty_chars_in_buffer()`. SlopOS should expose equivalent queue depth using existing counters.

### 12.1 Problem statement

`TIOCOUTQ` is not currently exposed in ABI or ioctl dispatch path. SlopOS already tracks output inflight state (`TTY_OUTPUT_INFLIGHT`) and driver pending state, but userspace cannot query it.

### 12.2 Implementation outline

- Add `TIOCOUTQ` constant to `abi/src/syscall.rs` (Linux-compatible value `0x5411`).
- Add `tty::get_output_queued_bytes(idx)` helper.
- Return queue depth primarily from `TTY_OUTPUT_INFLIGHT[slot]`, with driver pending integration as available.
- Wire ioctl in `core/src/syscall/fs/poll_ioctl_handlers.rs`.

### 12.3 Verification

- Test: immediately after write, `TIOCOUTQ > 0` until drain completes.
- Test: after drain, `TIOCOUTQ == 0`.
- Test: invalid pointer returns `-EFAULT`.
- Test: non-TTY FD returns `-ENOTTY`.
- Regression: `FIONREAD`/`TIOCINQ` behavior unchanged.
- `just build` + `just test` gate.

### 12.4 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `TIOCOUTQ` constant |
| `drivers/src/tty/mod.rs` | Add output-queued query helper |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add `TIOCOUTQ` ioctl handling |
| `drivers/src/tty_tests.rs` | Add output queue depth ioctl tests |

---

## 13. Phase 10: Input Wake Batching (WAKEUP_CHARS-style) (Tier 2)

**Status**: **DONE** ✅

> **Priority**: P1 throughput/perf — current implementation wakes poll/read waiters per input event aggressively.
> **Principle**: Adopt Linux's batching spirit (`WAKEUP_CHARS = 256`) while preserving SlopOS simplicity and correctness.

### 13.1 Problem statement

On high-rate input streams, frequent `wake_all` behavior can produce avoidable scheduler churn. Linux coalesces wakeups based on queued thresholds.

### 13.2 Implementation outline

- Add an input wake batching threshold constant (e.g., `WAKEUP_CHARS = 256`).
- Track pending unread input delta between wakeups.
- In non-canonical mode, wake readers when threshold is crossed, buffer nears full, timeout mode requires it, or hangup/signal occurs.
- In canonical mode, preserve immediate wake on line-completion events.

### 13.3 Verification

- Test: canonical mode still wakes immediately at line boundary.
- Test: non-canonical bulk input wakes at batching threshold, not per-byte.
- Test: no starvation — readers eventually wake under sustained input.
- Benchmark: reduced wake count under large paste/stream workload.
- Regression: poll/read correctness unchanged.
- `just build` + `just test` gate.

### 13.4 Files changed

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Added `WAKEUP_CHARS = 256` constant, `wake_chars_pending` counter to `LineDisc` and `RawDisc`, `should_wake_reader()` method to `LdiscOps` trait + both impls + `dispatch_ldisc!` macro, counter reset in `flush_input()`/`flush_all()`, increment in `push_cooked()`/`input_char()` |
| `drivers/src/tty/mod.rs` | Replaced per-byte `has_data` wake in `push_input()` with batched `should_wake_reader()` policy |
| `drivers/src/tty/pty.rs` | Applied batched wake policy in `slave_write()` |
| `drivers/src/tty_tests.rs` | Added 10 regression tests: constant value, canonical immediate wake, non-canonical batching, near-full wake, flush resets, RawDisc batching, counter reset, EOF wake |
---

## 14. Phase 11: TABDLY/XTABS Output Compatibility (Tier 2)

**Status**: **DONE** ✅ — Added `TABDLY` (0x1800), `TAB0` (0x0000), `TAB3` (0x1800), `XTABS` (0x1800) constants to `OutputFlags` bitflags and as raw `pub const` values in `abi/src/syscall.rs`. Gated tab expansion in `process_output_byte()` through `OutputFlags::TAB3` check: TAB3/XTABS expands tabs to spaces using column tracking, TAB0 passes literal tab through while still tracking column position. Updated default `c_oflag` to include `XTABS` (preserving existing tab expansion behavior). Updated `test_phase12_output_column_tracking_cr` to include XTABS for tab-verification steps. 9 regression tests added covering: ABI constant values, default oflag includes XTABS, XTABS tab-to-spaces expansion, TAB0 literal passthrough, TAB0 column tracking accuracy, XTABS column tracking across CR/LF/TAB mixes, TABDLY termios roundtrip, no-OPOST passthrough, and existing output processing compatibility.

> **Priority**: P1 compatibility — many terminal stacks assume standard tab-delay flag behavior even if only `XTABS` is used in practice.
> **Principle**: Implement the practical subset (`TAB0`/`TAB3`/`XTABS`) and keep legacy delay complexity out.

### 14.1 Problem statement

Current output processing already converts `\t` to spaces, but does not gate behavior through explicit `TABDLY`/`XTABS` flag semantics at ABI/termios level.

### 14.2 Implementation outline

- Add `TABDLY`, `TAB0`, `TAB3`, `XTABS` constants in `abi/src/syscall.rs` (aligned with Linux termbits values used in SlopOS ABI policy).
- In line discipline output processing, apply `TABDLY` mask:
  - `TAB0`: default tab behavior path.
  - `TAB3`/`XTABS`: expand tab to spaces using column tracking.
- Keep `OFILL`/`OFDEL` and other legacy delays out of scope.

### 14.3 Verification

- Test: `OPOST|XTABS` expands tab to expected number of spaces.
- Test: column tracking remains correct across CR/LF/TAB mixes.
- Test: toggling `TABDLY` bits roundtrips through termios get/set.
- Regression: existing output processing tests pass.
- `just build` + `just test` gate.

### 14.4 Files changed

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Added `TABDLY`/`TAB0`/`TAB3`/`XTABS` to `OutputFlags` bitflags (0x1800/0x0000/0x1800/0x1800) and matching raw `pub const` values |
| `drivers/src/tty/ldisc.rs` | Gated `b'\t'` branch in `process_output_byte()` through `OutputFlags::TAB3` — TAB3/XTABS returns `OutputAction::Tab(n)`, TAB0 returns `OutputAction::Emit` with literal tab; updated default `c_oflag` to `OPOST \| ONLCR \| XTABS` |
| `drivers/src/tty_tests.rs` | Added 9 tests (`test_fp11_*`): ABI constants, default oflag, XTABS expansion, TAB0 passthrough, TAB0 column tracking, mixed CR/LF/TAB column tracking, termios roundtrip, no-OPOST passthrough, existing output regression; fixed `test_phase12_output_column_tracking_cr` to include XTABS |

---

## 15. Phase 12: no_room-style Overflow Recovery (Tier 3)

**Status**: **DONE** ✅ — Added `no_room: bool` and `overflow_count: u32` fields to both `LineDisc` and `RawDisc`. `push_cooked()` and `RawDisc::input_char()` set the flag and increment the counter on buffer-full. `check_no_room_recovery()` clears the flag when occupancy falls to `THROTTLE_LOW_WATER` (1024 bytes). `flush_input()` and `flush_all()` reset both fields. Added `no_room()`, `overflow_count()`, and `check_no_room_recovery()` to `LdiscOps` trait with dispatch through `dispatch_ldisc!`. Wired recovery into `tty_read()` at three wake sites: packet-mode drain, normal read drain, and after-lock-drop path — each calls `check_no_room_recovery()` and wakes `TTY_INPUT_WAITERS`/`TTY_POLL_WAITERS` on the local slot. 14 regression tests added.

> **Priority**: P1 resilience — IMAXBEL + throttle prevent most loss, but explicit "buffer has no room" state improves recovery clarity and behavior under sustained pressure.
> **Principle**: Follow Linux's `no_room` concept, adapted to SlopOS's per-slot lock model.

### 15.1 Problem statement

When cooked input is full, bytes are dropped with IMAXBEL feedback. There is no explicit sticky recovery state that records overflow pressure and triggers deterministic recovery wakeup behavior after drain.

### 15.2 Implementation outline

- Add `no_room`-style flag in line discipline or `Tty` state.
- Set flag when cooked queue hits full/drop condition.
- Clear flag when occupancy falls below recovery threshold (aligned with unthrottle low-water).
- On clear, wake relevant waiters and re-arm producer path.
- Add optional overflow counter for diagnostics.

### 15.3 Verification

- Test: full cooked buffer sets `no_room` state.
- Test: drain below threshold clears state and resumes producer progress.
- Test: repeated fill/drain cycles avoid lockup and preserve existing throttle semantics.
- Regression: IMAXBEL behavior preserved.
- `just build` + `just test` gate.

### 15.4 Files changed

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Added `no_room: bool` + `overflow_count: u32` to `LineDisc` and `RawDisc`, set on buffer-full in `push_cooked()`/`input_char()`, cleared in `flush_input()`/`flush_all()`, `check_no_room_recovery()` at `THROTTLE_LOW_WATER`, accessors added to `LdiscOps` trait + both impls + `dispatch_ldisc!` |
| `drivers/src/tty/mod.rs` | Wired `no_room_recovered` into `tty_read()` at 3 wake sites: packet-mode drain, normal read drain, after-lock-drop — calls `check_no_room_recovery()` and wakes `TTY_INPUT_WAITERS`/`TTY_POLL_WAITERS` on recovery |
| `drivers/src/tty_tests.rs` | Added 14 tests (`test_fp12_*`): initial false, set on full, not set before full, overflow count increments, overflow count saturates, clears on drain below threshold, stays above threshold, flush_input clears, flush_all clears, fill/drain cycle, RawDisc no_room, IMAXBEL preserved, RawDisc recovery, LdiscKind dispatch |

---

## 16. Phase 13: Output Drain Semantics Hardening (Tier 3)

**Status**: **DONE** ✅

> **Priority**: P2 semantic clarity — `wait_output_idle()` exists and is strong, but this phase codifies strict `tcdrain`/`tcsbrk` behavior across edge conditions and drivers.
> **Principle**: Keep existing design, tighten guarantees and tests.

### 16.1 Problem statement

Drain logic currently combines `TTY_OUTPUT_INFLIGHT` and `driver.output_pending()`. This is good, but edge-case semantics (hangup/signal races, slot teardown, partial write timing) should be explicitly contract-tested.

### 16.2 Implementation outline

- Document drain contract in `wait_output_idle()` comments (what "idle" means).
- Ensure `tcsbrk(arg>0)` and termios wait modes share one authoritative drain path.
- Add explicit race tests: wake-before-block, hangup during drain, and signal interruption behavior.
- If needed, add helper to expose stronger per-driver pending-byte semantics.

### 16.3 Verification

- Test: `tcsbrk(arg>0)` blocks until inflight + pending are both clear.
- Test: hangup while draining returns expected error and unblocks waiters.
- Test: signal interruption behavior matches tty read/write policy.
- Regression: existing Phase 25/29 drain tests pass unchanged.
- `just build` + `just test` gate.

### 16.4 Implementation summary

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Hardened `wait_output_idle()` with formal drain contract (hangup awareness, single authoritative drain path, edge-case documentation). Hardened `tcsbrk()` with hangup guard and slot validation. Updated `is_output_idle()` to return true for hung-up TTYs. Updated `output_queued_bytes()` to use `output_pending_bytes()`. |
| `drivers/src/tty/driver.rs` | Added `output_pending_bytes()` to `TtyDriver` trait (default: 1-if-pending, 0-if-idle) and `TtyDriverKind` dispatch. Enhanced `output_pending()` doc comments. |
| `drivers/src/tty_tests.rs` | Added 15 tests (`test_fp13_*`): drain idle fast path, hangup vacuously complete, tcsbrk hangup returns error, tcsbrk zero hangup returns error, tcsbrk zero healthy succeeds, tcsbrk and tcsetsw share drain, drain invalid index, drain unallocated slot, PTY drain immediate, console drain synchronous, output pending bytes all drivers, output queued uses pending bytes, TCSETSW hangup returns error, TCSETSF hangup returns error, inflight accounting round trip |

---

## 17. Phase 14: Core Semantic Correctness (Gold Standard Audit)

**Status**: ✅ Done

> **Priority**: P0 — these are the semantic gaps where real programs (bash, vim, tmux, ssh) will break. Identified by a comparative audit against Linux N_TTY, RedoxOS, and Asterinas TTY implementations.
> **Principle**: Fix the places where real software breaks first: input status modeling, signal interruptibility, job control edge semantics, PTY allocation ABI, and per-byte lock churn. Keep all existing architectural strengths (per-slot locking, generation-tagged handles, split-write, deferred signals).

This phase bundles five interconnected improvements that share the same code paths and should land together for coherent testing.

### 14.1 Typed Input Status Record

**Problem**: `ldisc.input_char(c: u8)` can only receive raw bytes. Real serial hardware delivers break/parity/framing status alongside data. Without this, `IGNPAR`, `INPCK`, `PARMRK`, and true `BRKINT` semantics are best-effort approximations based on NUL byte heuristics — not POSIX-correct.

**What Linux does**: `receive_buf(const u8 *cp, const u8 *fp, size_t count)` — separate byte and flag buffers where `fp` carries `TTY_NORMAL`, `TTY_BREAK`, `TTY_PARITY`, `TTY_OVERRUN` per byte.

**Rust-native solution**:

```rust
/// Typed input event replacing raw u8 in the driver→ldisc interface.
/// Carries line-status metadata alongside the data byte so that
/// BRKINT, PARMRK, IGNPAR, and INPCK can be handled correctly.
#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    pub byte: u8,
    pub status: InputStatus,
}

/// Line status for a received byte.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputStatus {
    /// Normal data byte — no error condition.
    Normal = 0,
    /// Break condition detected by hardware.
    Break = 1,
    /// Parity error detected by hardware.
    ParityError = 2,
    /// Framing error detected by hardware.
    FrameError = 3,
    /// Hardware overrun — bytes were lost.
    Overrun = 4,
}
```

**Changes required**:

- Add `InputEvent` and `InputStatus` to `drivers/src/tty/driver.rs`.
- Change `TtyDriver::drain_input()` signature: `fn drain_input(&self, out: &mut [InputEvent]) -> usize` (or keep `u8` buffer + separate status buffer for zero-copy).
- Change `push_input(idx, c: u8)` → `push_input(idx, event: InputEvent)`. Callers that don't have status info (keyboard ISR, PTY master write) construct `InputEvent { byte, status: Normal }`.
- Change `LdiscOps::input_char(c: u8)` → `input_char(event: InputEvent)`. The line discipline can now branch on `event.status` for correct `BRKINT`/`PARMRK`/`IGNPAR`/`INPCK` handling instead of NUL-byte heuristics.
- Update `LineDisc::input_char()` and `RawDisc::input_char()` to handle `InputStatus::Break` (true BRKINT → SIGINT, not guessing from NUL), `InputStatus::ParityError` (IGNPAR → discard, INPCK + PARMRK → 0xFF 0x00 prefix), and `InputStatus::Overrun` (increment diagnostic counter).
- All existing callers that pass raw `u8` wrap it: `InputEvent { byte: c, status: InputStatus::Normal }`.

### 14.2 Interruptible Blocking for Write/Drain Paths

**Problem**: Currently only `tty_read()` returns `ERESTARTSYS` on signal. Several other blocking paths silently hang on signals:

| Blocking path | Current behavior | Correct behavior |
|---|---|---|
| Write throttle wait (PTY back-pressure) | Hangs on signal | Return `ERESTARTSYS` |
| `wait_output_idle()` / `tcdrain()` | Non-interruptible | Return `ERESTARTSYS` |
| `TCSETSW` / `TCSETSF` drain waits | Non-interruptible | Return `ERESTARTSYS` |
| PTY master write when slave throttled | Hangs on signal | Return `ERESTARTSYS` |

Without this, a `SIGWINCH` during `tcdrain()` hangs the process forever. Any interactive program (vim, less, bash) that receives signals while writing will hang.

**Changes required**:

- Audit every `wait_event` / busy-wait loop in `mod.rs` and `pty.rs`.
- Each must check for pending signals in the wait condition.
- Return `TtyError::Restart` (mapping to `ERESTARTSYS`) when interrupted.
- The existing syscall dispatch restart logic (Phase 7) handles `SA_RESTART` vs `EINTR` conversion — no changes needed there.
- Specifically: `wait_output_idle()`, the IXON stopped-output wait in `write()`, the PTY throttle wait in `pty::master_write()`, and the `TCSETSW`/`TCSETSF` drain in `set_termios_wait()`/`set_termios_flush()`.

### 14.3 Job Control Edge Cases

**Problem**: Three POSIX job control edge cases are missing:

**a) Background read when SIGTTIN is blocked/ignored:**
POSIX says: if SIGTTIN is blocked or ignored, or the process group is orphaned, return `EIO` instead of sending the signal. Currently SlopOS always sends `SIGTTIN` regardless.

```rust
// In session.rs check_read() — after determining BackgroundRead:
// Check if SIGTTIN is blocked/ignored for the caller.
// If so, or if the caller's pgrp is orphaned, return DeniedEIO instead.
```

**b) `TIOCSPGRP` / `tcsetpgrp()` from background process:**
POSIX requires sending `SIGTTOU` to the calling process group when a background process tries to set the foreground group. The write-side `SIGTTOU` handling already does this correctly, but `TIOCSPGRP` bypasses it. Fix: apply the same background-write check before `set_foreground_pgrp_checked()` in the ioctl handler.

**c) `poll_events()` ignores `output_stopped`:**
`TCXONC(TCOOFF)` stops output, and `write()` correctly blocks on `output_stopped`, but `poll_events()` still reports `POLLOUT` — misleading userland into thinking the TTY is writable. Fix: mask `POLLOUT` when `output_stopped` is true, matching the IXON `is_stopped()` check already present.

### 14.4 PTY Userland ABI Completeness

**Problem**: The `grantpt()` adapter in `syscall_services_init.rs` is a no-op stub. PTY slaves start locked (correct), but the standard `posix_openpt()` → `grantpt()` → `unlockpt()` → `ptsname()` flow used by tmux, screen, ssh, and the `script` utility expects `grantpt()` to at minimum unlock the slave.

**Changes required**:

- `grantpt()` implementation: at minimum, call `set_pty_lock(master_idx, false)` to unlock the slave. Full ownership/permission semantics are deferred until SlopOS has a permission model — document this explicitly.
- Verify `ptsname()` / `TIOCGPTN` returns a path that `open()` can resolve (i.e., `/dev/pts/N` path resolution works end-to-end).
- Consider adding `TIOCGPTPEER` (Linux 4.13+) — opens the slave FD directly from the master without path resolution. Low priority but modern tmux uses it.

### 14.5 Batched PTY Ingress

**Problem**: `pty::master_write()` pushes byte-by-byte through `push_input()`, taking the slave's slot lock per byte. For high-volume PTY traffic (compiling, log tailing, large outputs), this creates significant lock churn.

**What Linux does**: `receive_buf(const u8 *cp, const u8 *fp, size_t count)` processes an entire buffer under one lock acquisition with one wake decision at the end.

**Rust-native solution**:

```rust
// In ldisc.rs — new batched ingress method on LdiscOps:
fn receive_buf(&mut self, events: &[InputEvent]) -> BatchResult {
    // Process all bytes under one logical operation.
    // Accumulate echo actions, signals, and wake decisions.
    // Return a summary of what the caller needs to do after dropping the lock.
}

pub struct BatchResult {
    pub echo: ArrayVec<u8, 256>,  // Or a small fixed buffer
    pub signal: Option<u8>,
    pub should_wake: bool,
    pub throttle: bool,
}
```

**Changes required**:

- Add `receive_buf()` to `LdiscOps` trait and implement for `LineDisc` and `RawDisc`.
- In `mod.rs`, add `push_input_batch(idx, events: &[InputEvent])` that acquires the lock once, calls `ldisc.receive_buf()`, and performs one wake decision.
- Update `pty::master_write()` to use the batched path: construct `InputEvent` slice from the write buffer, call `push_input_batch()`.
- Keep the existing `push_input(idx, event)` for single-byte sources (keyboard ISR, serial polling) — it can internally delegate to the batch path with a 1-element slice.
- Pairs naturally with 14.1 (InputEvent) since the batch buffer is typed.

### 14.6 Additional Fixes (Small)

These are small targeted fixes that naturally land alongside the above:

- **`B0` baud rate hangup**: When `c_cflag` baud is set to `B0`, POSIX requires the modem control lines to be deasserted (effectively a hangup). Currently `B0` is decoded but has no hangup semantics. Fix: in `set_termios()`, check for `B0` and call `hangup()`.
- **`set_termios()` ignores `c_ispeed`/`c_ospeed`**: The get path correctly synthesizes speed fields from `c_cflag`, but the set path ignores incoming speed fields. Fix: merge `c_ispeed`/`c_ospeed` back into `c_cflag` baud bits when they're non-zero, matching Linux `termios2` behavior. This makes `cfsetispeed()`/`cfsetospeed()` roundtrip correctly.

### 14.7 Verification

- Test: `InputEvent` with `InputStatus::Break` + `BRKINT` set → `SIGINT` delivered (not NUL heuristic).
- Test: `InputEvent` with `InputStatus::ParityError` + `PARMRK` + `INPCK` → 0xFF 0x00 prefix marker inserted.
- Test: `InputEvent` with `InputStatus::ParityError` + `IGNPAR` → byte discarded.
- Test: `InputEvent` with `InputStatus::Normal` → identical to current `u8` behavior (regression).
- Test: `SIGWINCH` during `tcdrain()` → returns `-EINTR` (not hang).
- Test: `SIGWINCH` during PTY master write (throttled) → returns `-EINTR`.
- Test: `TCSETSW` interrupted by signal → returns `-EINTR` or restarts (per `SA_RESTART`).
- Test: Background process `tcsetpgrp()` → receives `SIGTTOU`.
- Test: Background read with `SIGTTIN` blocked → returns `-EIO` (not signal).
- Test: `poll_events()` with `output_stopped` set → `POLLOUT` NOT reported.
- Test: `grantpt()` unlocks slave → subsequent `open("/dev/pts/N")` succeeds.
- Test: `B0` in `tcsetattr()` → triggers hangup.
- Test: `cfsetospeed()` roundtrip — set speed via `c_ospeed` field, read back via `c_cflag` baud bits.
- Test: batched PTY ingress — master writes 1000 bytes, slave reads all 1000 (no loss, same as current).
- Test: batched ingress with mixed signals — `SIGINT` char in middle of batch → signal delivered, remaining bytes after signal char discarded (ISIG+!NOFLSH behavior).
- Regression: all existing 630+ TTY tests pass unchanged.
- `just build` + `just test` gate.

### 14.8 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/driver.rs` | Add `InputEvent`, `InputStatus` types. Update `TtyDriver::drain_input()` signature or add `drain_input_events()` variant. Update `TtyDriverKind` dispatch. |
| `drivers/src/tty/ldisc.rs` | Change `input_char(u8)` → `input_char(InputEvent)` on `LdiscOps` + both impls. Add `receive_buf()` batched method. Update BRKINT/PARMRK/IGNPAR/INPCK handling to use typed status. |
| `drivers/src/tty/mod.rs` | Change `push_input(idx, u8)` → `push_input(idx, InputEvent)`. Add `push_input_batch()`. Make `wait_output_idle()`, write throttle wait, IXON wait interruptible. Fix `poll_events()` to mask `POLLOUT` when `output_stopped`. Add `B0` hangup in `set_termios()`. Fix `c_ispeed`/`c_ospeed` merge in set path. |
| `drivers/src/tty/pty.rs` | Update `master_write()` to use batched ingress path. Update `push_input` calls to use `InputEvent`. Make throttle wait interruptible. |
| `drivers/src/tty/session.rs` | Add `DeniedEIO` variant to `ForegroundCheck` for blocked/ignored SIGTTIN case. Update `check_read()` to accept signal-blocked info. |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add SIGTTOU check before `TIOCSPGRP` dispatch. |
| `drivers/src/syscall_services_init.rs` | Implement real `grantpt()` (unlock slave). |
| `drivers/src/tty_tests/` | Tests for all items: typed input status, interruptible waits, job control edges, grantpt, batched ingress, B0 hangup, speed field roundtrip. |

---

## 18. Phase 15: VConsole Unicode & Broadened Xterm Emulation

**Status**: **DONE** ✅ — Changed cell model from `u8` to `u32` (Unicode codepoint per cell). Added UTF-8 decoder to `VtParser` ground state with 4-byte accumulator, overlong/surrogate rejection, and U+FFFD replacement for invalid sequences. Widened `VtAction::Print(u8)` to `VtAction::Print(u32)`. Added `SgrAttr::Foreground256(u8)`, `Background256(u8)`, `ForegroundRgb(u8,u8,u8)`, `BackgroundRgb(u8,u8,u8)` with full 6×6×6 cube + grayscale ramp → RGB mapping. Added DEC private mode tracking: DECCKM (mode 1), DECOM (mode 6), DECAWM (mode 7), bracketed paste (mode 2004). Added `is_double_width()` CJK range detection and continuation marker (`0xFFFF_FFFF`) for double-width cell handling. Added `get_glyph_for_codepoint(u32)` with replacement character diamond glyph. 32 regression tests added.

> **Priority**: P1 usability — the local framebuffer console currently handles only ASCII printable bytes and basic SGR (8+8 colors). Modern terminal programs (vim, tmux, bat, delta, less) depend on UTF-8 rendering, 256-color/truecolor, and several DEC private modes.
> **Principle**: Upgrade from "basic VGA terminal" to "usable xterm-class console". NOT a full xterm reimplementation — focus on the subset that real programs actually use. If SlopOS eventually gets a full userland terminal emulator, the kernel PTY/TTY path stays byte-transport focused while the compositor owns "full xterm" behavior.

### 15.1 Design Decision: Cell Model

**Must decide before implementation**: What does one "cell" in the VConsole grid represent?

| Model | Pros | Cons |
|---|---|---|
| **Byte** (current) | Simple, fast | Can't render any non-ASCII |
| **Codepoint** (recommended) | Handles ~99% of real-world Unicode | Doesn't handle grapheme clusters (flag emoji, ZWJ sequences) |
| **Grapheme cluster** | Fully correct | Complex, needs Unicode segmentation tables (~20KB data) |

**Recommendation**: Codepoint model with `u32` cells. Store one Unicode codepoint per cell. Characters wider than one cell (CJK, some emoji) occupy two cells with a "continuation" marker in the second. This is what Linux's fbcon, xterm, and most terminal emulators use.

```rust
// Replace current u8 cell with:
#[derive(Clone, Copy)]
pub(crate) struct Cell {
    pub codepoint: u32,       // Unicode codepoint (0 = empty, 0xFFFF_FFFF = continuation)
    pub attrs: CellAttributes,
}
```

**Memory impact**: Each cell grows from 1 byte (`u8`) + 8 bytes (`CellAttributes`) = 9 bytes to 4 bytes (`u32`) + 8 bytes = 12 bytes. For 240×80 grid: 230 KB → 230 KB (negligible — `CellAttributes` already dominates).

### 15.2 UTF-8 Decode in VtParser Ground State

Currently `vtparser.rs` ignores bytes > 127 in ground state (`VtAction::None` for non-ASCII). Need a UTF-8 decoder:

- Add a small UTF-8 accumulator to `VtParser` (4-byte buffer + byte count + expected length).
- When a byte with high bit set arrives in ground state, start accumulating.
- When the codepoint is complete, emit `VtAction::Print(codepoint)` (change `Print(u8)` to `Print(u32)` or `PrintUnicode(u32)`).
- Invalid sequences: emit U+FFFD (replacement character) and re-sync.

### 15.3 256-Color and Truecolor SGR

Modern SGR sequences used by vim, bat, delta, and most colorized CLI tools:

| Sequence | Meaning | Priority |
|---|---|---|
| `\e[38;5;Nm` | 256-color foreground | High — vim, bat, ls --color |
| `\e[48;5;Nm` | 256-color background | High |
| `\e[38;2;R;G;Bm` | Truecolor (24-bit) foreground | Medium — delta, bat |
| `\e[48;2;R;G;Bm` | Truecolor (24-bit) background | Medium |

Changes to `vtparser.rs`:
- In SGR parsing, handle `38;5;N` and `48;5;N` (consume 3 params).
- Handle `38;2;R;G;B` and `48;2;R;G;B` (consume 5 params).
- Map 256-color indices 0–7 → standard colors, 8–15 → bright colors, 16–231 → 6×6×6 cube, 232–255 → grayscale ramp.
- Add `SgrAttr::Foreground256(u8)`, `SgrAttr::Background256(u8)`, `SgrAttr::ForegroundRgb(u8,u8,u8)`, `SgrAttr::BackgroundRgb(u8,u8,u8)`.

Changes to `vconsole.rs`:
- Map 256-color to nearest RGB for framebuffer.
- Store truecolor directly in `CellAttributes` (already `u32` RGB).

### 15.4 Bracketed Paste Mode

Used by bash 5+, fish, zsh, vim to distinguish typed input from pasted text:

- `\e[?2004h` — enable bracketed paste mode.
- `\e[?2004l` — disable bracketed paste mode.
- When enabled, paste events are wrapped in `\e[200~` ... `\e[201~` markers.

This is a DEC private mode. Add a `bracketed_paste: bool` flag to `VtParser` or `VConsoleState`. The TTY input path checks this flag when injecting paste data.

### 15.5 Additional DEC Private Modes

Modes used by vim, tmux, less, and similar programs:

| Mode | Sequence | Used by | Priority |
|---|---|---|---|
| DECCKM (cursor key mode) | `\e[?1h/l` | vim, tmux | High |
| DECOM (origin mode) | `\e[?6h/l` | less, vim | Medium |
| DECAWM (auto-wrap) | `\e[?7h/l` | most programs | High |
| DECTCEM (cursor visibility) | `\e[?25h/l` | vim, tmux | High (may already exist) |
| Alt screen buffer | `\e[?1049h/l` | vim, tmux, less | High (may already exist) |
| Mouse tracking (basic) | `\e[?1000h/l` | tmux | Low |

Audit `vtparser.rs` for which are already implemented, add the missing ones.

### 15.6 Double-Width Character Handling

CJK characters (U+2E80–U+9FFF, U+F900–U+FAFF, etc.) and some emoji are "fullwidth" — they occupy 2 cell columns. When rendering:
- The left cell stores the codepoint.
- The right cell stores a continuation marker (e.g., `codepoint = 0xFFFF_FFFF`).
- Cursor advances by 2 columns.
- Backspace over a double-width char erases both cells.

Use a lookup table or Unicode `East_Asian_Width` property to determine width. The `unicode-width` crate pattern (a ~2KB bitset lookup) can be embedded in `no_std`.

### 15.7 Verification

- Test: UTF-8 "Héllo" renders correctly (é = 2-byte sequence).
- Test: CJK character "中" occupies 2 cells, cursor advances by 2.
- Test: 256-color SGR `\e[38;5;196m` sets foreground to red.
- Test: Truecolor SGR `\e[38;2;255;128;0m` sets foreground to orange.
- Test: Bracketed paste mode enable/disable roundtrip.
- Test: DECAWM on → line wraps at column limit; DECAWM off → cursor stays at last column.
- Test: DECTCEM hide/show cursor.
- Test: Alt screen switch and restore.
- Test: Invalid UTF-8 byte → U+FFFD replacement character.
- Test: Mixed ASCII + UTF-8 + escape sequences in one write → all render correctly.
- Regression: all existing VT100 parser tests pass (ASCII behavior unchanged).
- `just build` + `just test` gate.

### 15.8 Files changed

| File | Change |
|------|--------|
| `drivers/src/tty/vtparser.rs` | Added `State::Utf8` with 4-byte accumulator, widened `VtAction::Print(u8)` → `Print(u32)`, added `SgrAttr::Foreground256`/`Background256`/`ForegroundRgb`/`BackgroundRgb` variants with proper sub-parameter consumption, added DEC mode tracking (`bracketed_paste`, `cursor_key_mode`, `origin_mode`, `auto_wrap`) for modes 1/6/7/2004 |
| `drivers/src/tty/vconsole.rs` | Changed `cells: [[u8; ...]; ...]` → `[[u32; ...]; ...]` (codepoint model), renamed `print_char` → `print_codepoint`, added `color256_to_rgb()` with 6×6×6 cube + grayscale mapping, added `CONTINUATION_CODEPOINT` marker for double-width CJK, added 256-color/truecolor branches in `apply_sgr`, backspace clears continuation cells, double-width at last column renders space |
| `abi/src/font.rs` | Added `REPLACEMENT_GLYPH` (filled diamond), `get_glyph_for_codepoint(u32)` with ASCII fast-path and replacement fallback, `is_double_width(cp: u32)` with CJK/Hangul/Fullwidth Unicode ranges |
| `drivers/src/tty_tests/test_vconsole.rs` | Added 32 tests (`test_fp15_*`): UTF-8 2/3/4-byte decode, invalid/truncated/overlong rejection, 256-color fg/bg SGR, truecolor fg/bg SGR, vconsole 256-color/truecolor rendering, bracketed paste toggle, DECAWM/DECCKM/DECOM toggle, DECTCEM/alt screen regression, u32 cell model, "Héllo" rendering, CJK double-width, replacement char, mixed ASCII+UTF-8+escapes, color cube mapping, grayscale mapping, width range checks, fuzz test, glyph existence |
| `drivers/src/tty_tests/test_ldisc.rs` | Updated existing tests for `u32` cell comparisons (`b'A'` → `b'A' as u32`) and `VtAction::Print(u32)` pattern matches |
| `drivers/src/tty_tests/mod.rs` | Updated `boxed_vconsole_state()` helper for `u32` cells, registered 32 new `test_fp15_*` tests in suite |

---

## 19. Phase 16: mod.rs Module Decomposition

**Status**: **DONE** ✅ — Decomposed `mod.rs` from 2543 lines into 5 focused sub-modules: `io.rs` (~1073 lines: read, write, push_input, drain, data queries, idle callback), `termios.rs` (~722 lines: termios get/set, winsize, ldisc, ioctls, drain), `job_control.rs` (~259 lines: session, foreground pgrp, controlling terminal), `lifecycle.rs` (~238 lines: open/close, hangup, active TTY, init), `poll.rs` (~205 lines: poll readiness, poll sleep, compositor focus). `mod.rs` slimmed to ~239 lines (struct, error enum, re-exports). Pure refactor with zero behavioral changes. All 1556 pre-existing tests pass unchanged. 10 new regression tests added verifying public API surface preservation (function signature checks, error variant stability, constant value checks, smoke tests). `cargo fmt` applied.

> **Priority**: P2 maintainability — `mod.rs` at 2,596 lines mixes core I/O, termios policy, job control, lifecycle, hangup, and poll in one file. The code is correct, but navigation and future changes are harder than they need to be.
> **Principle**: Split into focused modules with clear responsibilities. No behavioral changes — pure refactor. **Do this AFTER Phases 14 and 15**, since those phases modify `mod.rs` heavily. Refactoring code that's about to change wastes effort.

### 19.1 Problem statement

`mod.rs` is the largest file in the TTY subsystem (2,596 lines as of Phase 13 completion, likely ~2,800+ after Phase 14). It contains:

- The `Tty` struct definition and field access helpers
- `TtyError` enum and `to_errno()` mapping
- `TtyIndex` re-export and `MAX_TTYS` constant
- Read path (~400 lines including VMIN/VTIME, canonical/non-canonical, packet mode)
- Write path (~200 lines including split-write, IXON check, output processing)
- `push_input()` and hardware drain logic (~150 lines)
- Termios get/set/wait/flush (~200 lines)
- Window size management (~50 lines)
- Line discipline get/set (~50 lines)
- Job control (foreground pgrp, session attach/detach, SIGTTIN/SIGTTOU, controlling terminal acquire/release/detach) (~300 lines)
- Lifecycle (open_ref, close_ref, hangup, vhangup) (~200 lines)
- Poll events and poll sleep (~100 lines)
- Active TTY management and compositor focus (~100 lines)
- Miscellaneous ioctls (tcflush, tcsbrk, tcxonc, FIONREAD, TIOCOUTQ) (~150 lines)

### 19.2 Proposed module split

```
drivers/src/tty/
├── mod.rs          (~200 lines)  — Tty struct, TtyError, TtyIndex, MAX_TTYS, re-exports
├── io.rs           (~600 lines)  — read(), write(), push_input(), push_input_batch(),
│                                   drain_hw_input_locked(), split-write helpers
├── termios.rs      (~400 lines)  — get/set_termios, set_termios_wait/flush,
│                                   winsize, line discipline, tcflush, tcsbrk, tcxonc,
│                                   FIONREAD, TIOCOUTQ, wait_output_idle
├── job_control.rs  (~300 lines)  — session attach/detach, foreground pgrp,
│                                   controlling terminal acquire/release/detach,
│                                   SIGTTIN/SIGTTOU enforcement, detach_session_by_id
├── lifecycle.rs    (~200 lines)  — open_ref, close_ref, hangup, vhangup,
│                                   is_hung_up, active TTY management
├── poll.rs         (~150 lines)  — poll_events, poll_sleep_on, poll_sleep,
│                                   compositor focus
├── ldisc.rs        (unchanged)
├── driver.rs       (unchanged)
├── pty.rs          (unchanged)
├── session.rs      (unchanged)
├── table.rs        (unchanged)
├── vconsole.rs     (unchanged)
├── vtparser.rs     (unchanged)
└── ringbuf.rs      (unchanged)
```

### 19.3 Implementation guidelines

- **Pure refactor**: no behavioral changes, no API changes, no new features.
- Use `pub(crate)` for internal functions that are only called within the `tty` module.
- Keep all `pub` functions accessible from the same path (`crate::tty::read`, etc.) via re-exports in `mod.rs`.
- The `Tty` struct stays in `mod.rs` since every sub-module needs it.
- Helper methods on `Tty` move to the module that owns their concern (e.g., `Tty::check_fg_read()` → `job_control.rs`).
- Each new module gets a doc comment explaining its responsibility.
- Run `just build` after each file move to catch import issues immediately.

### 19.4 Verification

- `just build` compiles cleanly (this is the primary gate — it's a pure refactor).
- `just test` — all 630+ existing tests pass unchanged.
- No `pub` API changes visible to code outside the `tty` module.
- Grep confirms no function moved between files has a changed signature.
- Each new file has a module doc comment.

### 19.5 Files changed

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Slimmed from 2543 to ~239 lines: Tty struct, TtyError enum, MAX_TTYS constant, module declarations, re-exports preserving full public API surface |
| `drivers/src/tty/io.rs` | **NEW** (~1073 lines) — `impl Tty { drain_hw_input_locked }`, push_input/push_input_batch, read/read_with_attach, write, has_data, bytes_available, output_queued_bytes, input_available_cb, register_idle_callback, PTY re-exports |
| `drivers/src/tty/termios.rs` | **NEW** (~722 lines) — TermiosSetMode enum, cflag_to_speed, get/set_termios, set_termios_wait/flush, wait_output_idle, is_output_idle, get/set_ldisc, get/set_winsize, tcflush, tcsbrk, tcxonc |
| `drivers/src/tty/job_control.rs` | **NEW** (~259 lines) — get/set_foreground_pgrp, set_foreground_pgrp_checked, get_session_id, attach_session, acquire/release/detach_controlling_terminal, detach_session |
| `drivers/src/tty/lifecycle.rs` | **NEW** (~238 lines) — ACTIVE_TTY/DEFAULT_CONSOLE_TTY statics, active_tty, set_active_tty, switch_active_tty, set_default_console_tty, default_console_tty, init, open_ref, close_ref, hangup, is_hung_up, vhangup |
| `drivers/src/tty/poll.rs` | **NEW** (~205 lines) — set/get_compositor_focus, poll_events, poll_sleep_on, poll_sleep |
| `drivers/src/tty_tests/test_ldisc.rs` | Added 10 regression tests (`test_fp16_*`): API re-export verification (io, termios, job_control, lifecycle, poll, pty), struct field accessibility, error variant stability, MAX_TTYS constant check, smoke test |
| `drivers/src/tty_tests/mod.rs` | Registered 10 new `test_fp16_*` tests in suite |

---

## 20. Phase 17: POSIX Controlling Terminal Semantics

**Status**: **DONE** ✅ — Added `can_be_controlling_terminal()` to `TtyDriverKind` rejecting PTY masters from becoming controlling terminals. Guarded `acquire_controlling_terminal()` with the new check so `TIOCSCTTY` on a master FD returns `PermissionDenied`. O_NOCTTY enforcement was already implemented in `fileio.rs::maybe_acquire_controlling_tty_on_open()` (verified). Added POSIX controlling-terminal check in `TIOCSPGRP` ioctl handler — the FD's TTY index must match the caller's `controlling_tty` or the ioctl returns `ENOTTY`. Added wake calls in `set_foreground_pgrp()` and `set_foreground_pgrp_checked()` — after changing `fg_pgrp`, both `TTY_INPUT_WAITERS[slot]` and `TTY_POLL_WAITERS[slot]` are woken so blocked readers re-evaluate foreground status and receive `SIGTTIN` promptly. 13 regression tests added.

> **Priority**: P0 — these are POSIX semantic bugs that will break real shell programs (bash, tmux, ssh, daemon processes). Bundled because all four fixes share the same code paths (session management, job control, ioctl dispatch) and should land together for coherent testing.
> **Principle**: Fix the places where POSIX programs will break. Do not redesign — harden the existing architecture with precise, minimal guards.

This phase bundles four interconnected controlling terminal / job control fixes.

### 17.1 PTY master cannot become controlling terminal

**Problem**: `acquire_controlling_terminal()` in `job_control.rs:136` does not check whether the TTY is a PTY master. POSIX says only the **slave** side of a PTY pair (or a real terminal) should become a controlling terminal. If a terminal emulator (or `ssh`) calls `TIOCSCTTY` on the master FD, it gets a controlling terminal on the wrong end — breaking shell session management.

**What Linux does**: Linux's `tiocsctty()` in `tty_jobctrl.c` has an explicit check: the `tty->driver->type` must not be `TTY_DRIVER_TYPE_PTY` with `subtype == PTY_TYPE_MASTER`.

**Fix**:
- Add a `can_be_controlling_terminal()` method to `TtyDriverKind`:
  ```rust
  impl TtyDriverKind {
      pub fn can_be_controlling_terminal(&self) -> bool {
          !matches!(self, TtyDriverKind::PtyMaster { .. })
      }
  }
  ```
- Guard `acquire_controlling_terminal()`: return `Err(TtyError::PermissionDenied)` if `!tty.driver.can_be_controlling_terminal()`.
- Guard the `TIOCSCTTY` ioctl path in `poll_ioctl_handlers.rs` to also check this before calling `acquire_controlling_terminal`.

### 17.2 O_NOCTTY enforcement in open path

**Problem**: POSIX requires that opening a terminal device with `O_NOCTTY` prevents it from becoming the controlling terminal. Currently there is no evidence of `O_NOCTTY` checking in the TTY open path. Every `open("/dev/pts/N")` by a session leader without a controlling terminal would automatically acquire it — breaking daemon processes that deliberately avoid controlling terminals via `O_NOCTTY`.

**What Linux does**: `tty_open()` in `tty_io.c` checks `filp->f_flags & O_NOCTTY` and skips controlling terminal assignment if set.

**Fix**:
- Add `O_NOCTTY` constant to `abi/src/syscall.rs` (value `0o400`, matching Linux).
- Thread the open flags through to the TTY open path (likely via `fileio.rs` → `open_ref` or a new `open_with_flags` variant).
- When `O_NOCTTY` is set, skip the controlling terminal auto-acquisition step.
- Ensure existing daemon/service startup paths that set `O_NOCTTY` work correctly.

### 17.3 TIOCSPGRP must verify controlling terminal

**Problem**: POSIX requires that `tcsetpgrp()` (via `TIOCSPGRP`) only works on the caller's **controlling terminal**. Currently `set_foreground_pgrp_checked()` only checks session match, but does not verify that the FD refers to the caller's actual controlling terminal. A process could change the foreground group on any TTY in its session, not just its controlling terminal.

**What Linux does**: `tty_check_change()` and the `TIOCSPGRP` handler in `tty_jobctrl.c` verify that `tty == current->signal->tty` (the calling process's controlling terminal).

**Fix**:
- The `TIOCSPGRP` ioctl handler in `poll_ioctl_handlers.rs` must compare the TTY index from the FD with the caller's `controlling_tty` from the task struct.
- If they don't match, return `-ENOTTY` (POSIX: "The file is not the controlling terminal of the calling process").
- This requires the ioctl handler to have access to the current task's controlling terminal index.

### 17.4 Foreground pgrp changes must wake blocked readers

**Problem**: In `io.rs`, the blocking read wait loop uses `wait_event` which only rechecks on TTY events (input arrival, hangup). If a process gets moved to background via `tcsetpgrp()` while blocked in read, it won't receive `SIGTTIN` until the next TTY event. POSIX says background processes should receive `SIGTTIN` promptly when they lose the foreground.

**What Linux does**: `__proc_set_tty()` and `tiocspgrp()` call `tty_pgrp_notified = true` and wake the read waiters so they re-evaluate their foreground status.

**Fix**:
- In `set_foreground_pgrp()` and `set_foreground_pgrp_checked()` (both in `job_control.rs`), after changing `fg_pgrp`, wake `TTY_INPUT_WAITERS[slot]` and `TTY_POLL_WAITERS[slot]`.
- This causes blocked readers to re-check `check_read()` and either continue (if still foreground) or get `SIGTTIN` (if now background).

### 17.5 Verification

- Test: `TIOCSCTTY` on PTY master returns `-EPERM` / `PermissionDenied`.
- Test: `TIOCSCTTY` on PTY slave succeeds (regression — existing behavior preserved).
- Test: `TIOCSCTTY` on serial/vconsole succeeds.
- Test: `open("/dev/pts/N", O_NOCTTY)` does not acquire controlling terminal.
- Test: `open("/dev/pts/N", 0)` by session leader DOES acquire controlling terminal (regression).
- Test: `TIOCSPGRP` on a TTY that is not the caller's controlling terminal returns `-ENOTTY`.
- Test: `TIOCSPGRP` on the caller's controlling terminal succeeds (regression).
- Test: blocked read + `tcsetpgrp(other_pgrp)` → reader receives `SIGTTIN` promptly (not stuck until next input).
- Regression: all existing session/job-control/PTY tests pass unchanged.
- `just build` + `just test` gate.

### 17.6 Files changed

| File | Change |
|------|--------|
| `drivers/src/tty/driver.rs` | Added `can_be_controlling_terminal()` method to `TtyDriverKind` — returns `false` for `PtyMaster`, `true` for all others |
| `drivers/src/tty/job_control.rs` | Guarded `acquire_controlling_terminal()` with `can_be_controlling_terminal()` check. Added `TTY_INPUT_WAITERS[slot].wake_all()` + `TTY_POLL_WAITERS[slot].wake_all()` in both `set_foreground_pgrp()` and `set_foreground_pgrp_checked()` after fg_pgrp change. Imported `scheduler_is_enabled`, `TTY_INPUT_WAITERS`, `TTY_POLL_WAITERS`. |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Added controlling-terminal validation in `TIOCSPGRP` handler — checks `task.controlling_tty == Some(tty_idx)` before allowing the operation, returns `ENOTTY` otherwise |
| `abi/src/syscall.rs` | `O_NOCTTY` already present (`0x100`) — no change needed |
| `fs/src/fileio.rs` | `O_NOCTTY` enforcement already implemented in `maybe_acquire_controlling_tty_on_open()` — no change needed |
| `drivers/src/tty_tests/test_ldisc.rs` | Added 13 tests (`test_ctty_*`): `can_be_controlling_terminal` for all 5 driver kinds, acquire ctty on PTY master/slave/serial/vconsole, O_NOCTTY constant value, set_foreground_pgrp wake (both direct and checked variants), PTY master no session attachment after rejected acquire |
| `drivers/src/tty_tests/mod.rs` | Registered 13 new `test_ctty_*` tests in suite |

---

## 21. Phase 18: TIOCOUTQ Byte Accounting & Packet Mode Edge Fix

**Status**: Pending

> **Priority**: P1 — `TIOCOUTQ` currently returns wrong values (counts operations, not bytes). Packet mode has an edge case with 1-byte read buffers. Both are bugs that will confuse programs but won't crash them.
> **Principle**: Fix two distinct output/read accounting bugs. Bundled because both are small, self-contained fixes in `io.rs` / `table.rs`.

### 18.1 TIOCOUTQ returns operations, not bytes

**Problem**: `TTY_OUTPUT_INFLIGHT` in `table.rs:79` is incremented/decremented per `write_driver_unlocked()` **call**, not per byte. A single `write()` of 256 bytes registers as `inflight = 1`, not `256`. `output_queued_bytes()` in `io.rs:1013` treats this count as bytes, so `TIOCOUTQ` returns `1` instead of `256`. Programs using `TIOCOUTQ` for back-pressure (tmux, screen) get incorrect queue depth.

**What Linux does**: `tty_chars_in_buffer()` calls the driver's `chars_in_buffer()` method which returns the actual number of queued bytes.

**Fix**:
- Change `TTY_OUTPUT_INFLIGHT` from a "number of active writes" counter to a **byte counter**.
- When entering the split-write path, increment by the number of bytes being written (not `+= 1`).
- When the write completes, decrement by the actual bytes consumed.
- Alternatively, remove `TTY_OUTPUT_INFLIGHT` entirely and make `output_queued_bytes()` query the driver directly via `output_pending_bytes()` (the Phase 13 addition), which already exists as a trait method on `TtyDriver`.
- The second approach is cleaner — it eliminates the redundant counter and relies on the authoritative source (the driver) for queue depth.

### 18.2 Packet mode read with buf.len() == 1

**Problem**: In `io.rs:325`, when packet mode is active and the caller's buffer is exactly 1 byte, the code reserves space for the packet-mode prefix byte plus payload. With `buf.len() == 1`, there's only room for the prefix byte — no room for any payload. The code falls through to the wait logic despite potentially having pending packet events that could be returned as a 1-byte prefix-only read.

**What Linux does**: `n_tty_read()` returns just the packet-mode status byte when there are pending events, even if the user buffer is exactly 1 byte.

**Fix**:
- When `buf.len() == 1` and there are pending `packet_events`, return the packet event byte immediately (consume the events, write the status byte to `buf[0]`, return 1).
- When `buf.len() == 1` and there are no pending packet events but there is data, return 0 bytes with a comment explaining that the buffer is too small for prefix+payload.
- Add explicit documentation for this edge case in the packet-mode read path.

### 18.3 Verification

- Test: `TIOCOUTQ` returns byte count, not operation count — write 256 bytes, query immediately, get ≥ 256 (or driver-reported queue depth).
- Test: `TIOCOUTQ` returns 0 after drain completes.
- Test: packet mode read with `buf.len() == 1` and pending events → returns 1 byte (status byte).
- Test: packet mode read with `buf.len() == 1` and no events → correct behavior (wait or return 0).
- Regression: all existing TIOCOUTQ and packet mode tests pass.
- `just build` + `just test` gate.

### 18.4 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/table.rs` | Change `TTY_OUTPUT_INFLIGHT` to byte-granularity (or remove entirely if driver-based approach used) |
| `drivers/src/tty/io.rs` | Update `write_driver_unlocked` to track bytes not calls; fix packet mode `buf.len() == 1` edge case in read path |
| `drivers/src/tty/termios.rs` | Update `output_queued_bytes()` if switching to pure driver-based accounting |
| `drivers/src/tty_tests.rs` | Byte-granularity TIOCOUTQ tests, packet mode 1-byte buffer tests |

---

## 22. Phase 19: Missing Ioctls (TIOCGSID, TIOCEXCL) & HUPCL Enforcement

**Status**: Pending

> **Priority**: P1 — `TIOCGSID` is POSIX-required. `TIOCEXCL` is used by serial tools (minicom, screen). `HUPCL` enforcement is expected by POSIX but currently not enforced. Bundled because all three are small, independent additions with no shared code paths.
> **Principle**: Add the missing ioctl support and enforce `HUPCL` semantics. Each sub-item is self-contained — the phase bundles them to avoid plan sprawl.

### 19.1 TIOCGSID — Get session ID

**Problem**: `tcgetsid()` in libc requires `TIOCGSID` to return the session ID for the controlling terminal. Currently not in the ioctl dispatch. Programs that call `tcgetsid()` get `-ENOTTY` or `-EINVAL`.

**What Linux does**: `tiocgsid()` in `tty_jobctrl.c` returns `tty->session->pid` after verifying the TTY is the caller's controlling terminal.

**Fix**:
- Add `TIOCGSID` constant to `abi/src/syscall.rs` (value `0x5429`, matching Linux).
- Add handler in ioctl dispatch that calls `tty::get_session_id(idx)`.
- Verify the TTY is the caller's controlling terminal before returning (POSIX requirement: `ENOTTY` if not).
- Wire through service bridge.

### 19.2 TIOCEXCL / TIOCNXCL / TIOCGEXCL — Exclusive mode

**Problem**: Serial terminal programs like `minicom`, `screen`, and `picocom` use `TIOCEXCL` to prevent other processes from opening the same TTY. Without it, multiple processes can fight over a serial port.

**What Linux does**: Sets `TTY_EXCLUSIVE` flag on `tty_struct.flags`. `tty_open()` checks this flag and returns `-EBUSY` for non-root opens.

**Fix**:
- Add `exclusive: bool` field to `Tty` struct (or include in the `TtyFlags` bitflags from Phase 20).
- Add `TIOCEXCL` (`0x540C`), `TIOCNXCL` (`0x540D`), `TIOCGEXCL` (`0x80045440`) constants to ABI.
- `TIOCEXCL`: set `exclusive = true` (no arguments, no privilege check — matches Linux).
- `TIOCNXCL`: clear `exclusive = false`.
- `TIOCGEXCL`: return `exclusive` as `i32` (0 or 1).
- In `open_ref()`, check `exclusive` and return `TtyError::DeviceBusy` for opens when `exclusive == true` and `open_count > 0` (exempt root/CAP_SYS_ADMIN if capability system exists, otherwise just first opener holds exclusive).

### 19.3 HUPCL enforcement on last close

**Problem**: POSIX says: when `HUPCL` is set in `c_cflag` and the last process closes the terminal, the modem control lines should be deasserted (triggering hangup). Currently `close_ref()` in `lifecycle.rs` doesn't check `HUPCL`. For PTYs, this means the slave should get a hangup when `HUPCL` is set and the last FD closes — important for `ssh` session cleanup.

**What Linux does**: `tty_port_close_start()` checks `HUPCL | CLOCAL` and calls `tty_port_lower_dtr_rts()` and `tty_port_shutdown()` on last close.

**Fix**:
- In `close_ref()` (lifecycle.rs), after `open_count` reaches 0 for non-PTY terminals:
  - Check `tty.ldisc.termios().control_flags().contains(ControlFlags::HUPCL)`.
  - If set, call `hangup(idx)` to assert hangup semantics.
- For PTY slave: the existing master-close → slave-hangup path already handles this. Verify HUPCL doesn't double-hangup.
- For serial/vconsole: the HUPCL hangup is the new behavior — flush buffers, detach session, signal.

### 19.4 Verification

- Test: `TIOCGSID` returns correct session ID for controlling terminal.
- Test: `TIOCGSID` on non-controlling TTY returns `-ENOTTY`.
- Test: `TIOCGSID` on unallocated slot returns error.
- Test: `TIOCEXCL` prevents second open (returns `-EBUSY`).
- Test: `TIOCNXCL` clears exclusive mode, second open succeeds.
- Test: `TIOCGEXCL` returns correct exclusive state.
- Test: console TTY with `HUPCL` set, last close → session receives `SIGHUP`.
- Test: console TTY without `HUPCL`, last close → no hangup (buffers flushed, session detached, but no signal).
- Test: PTY slave close with `HUPCL` → no double hangup (master-close path already handles it).
- Regression: all existing lifecycle/ioctl tests pass.
- `just build` + `just test` gate.

### 19.5 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `TIOCGSID`, `TIOCEXCL`, `TIOCNXCL`, `TIOCGEXCL` constants |
| `drivers/src/tty/mod.rs` | Add `exclusive: bool` field to `Tty` struct (or defer to Phase 20 `TtyFlags`) |
| `drivers/src/tty/lifecycle.rs` | Add HUPCL check in `close_ref()` for non-PTY terminals; add exclusive check in `open_ref()` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Wire `TIOCGSID`, `TIOCEXCL`, `TIOCNXCL`, `TIOCGEXCL` through ioctl dispatch |
| `lib/src/kernel_services/syscall_services/tty.rs` | Add service bridge for new ioctls |
| `drivers/src/tty_tests.rs` | TIOCGSID, exclusive mode, HUPCL enforcement tests |

---

## 23. Phase 20: Rust Encapsulation & Type Safety

**Status**: Pending

> **Priority**: P2 — no runtime bugs, but the current `pub` field exposure makes invariant violations trivial to introduce. Rust's type system should enforce state-transition correctness.
> **Principle**: Make invalid states unrepresentable. Use `pub(crate)` + domain methods instead of raw field access. Consolidate scattered boolean flags into a typed flags type. Remove redundant state modeling.

This phase is a pure refactor — no behavioral changes, no new features.

### 20.1 Privatize `Tty` struct fields

**Problem**: Every field on `Tty` is `pub`, meaning any code in the crate can mutate `hung_up`, `open_count`, `throttled`, etc. without going through proper state transition methods. This makes invariant violations easy to introduce (e.g., setting `hung_up = true` without flushing buffers or signaling the session).

**Fix**:
- Change all fields on `Tty` to `pub(crate)` (they're only accessed within the `drivers` crate).
- For fields that represent state transitions with side effects (`hung_up`, `throttled`, `output_stopped`, `peer_closed`), add domain methods that enforce the required side effects:
  ```rust
  impl Tty {
      /// Mark this TTY as hung up. Clears output_stopped so blocked
      /// writers unblock and see the hung_up flag. Does NOT flush
      /// buffers or signal — caller must do that outside the lock.
      pub(crate) fn mark_hung_up(&mut self) {
          self.hung_up = true;
          self.output_stopped = false;
      }
  }
  ```
- **Do NOT** create a sea of trivial getters/setters. Only add methods where state transitions have invariants to enforce.

### 20.2 Privatize `TtySession` fields

**Problem**: `TtySession` in `session.rs` has public fields that allow bypass of session management invariants. Code outside the module can set `session_id` without going through `attach()`/`detach()`.

**Fix**: Change `TtySession` fields to `pub(crate)` and ensure all access goes through the existing methods (`attach`, `detach`, `session_id_raw`, `fg_pgrp_raw`, etc.).

### 20.3 Consolidate boolean flags into `TtyFlags` bitflags

**Problem**: The six boolean fields on `Tty` (`hung_up`, `peer_closed`, `slave_locked`, `packet_mode`, `throttled`, `output_stopped`) are scattered state that's easy to get into invalid combinations. For example, a TTY should never be both `hung_up` and `output_stopped` (hangup clears output_stopped).

**Fix**:
- Define a `TtyFlags` bitflags type:
  ```rust
  bitflags! {
      #[derive(Clone, Copy, Debug, Default)]
      pub(crate) struct TtyFlags: u16 {
          const HUNG_UP        = 1 << 0;
          const PEER_CLOSED    = 1 << 1;
          const SLAVE_LOCKED   = 1 << 2;
          const PACKET_MODE    = 1 << 3;
          const THROTTLED      = 1 << 4;
          const OUTPUT_STOPPED = 1 << 5;
          const EXCLUSIVE      = 1 << 6;  // from Phase 19
      }
  }
  ```
- Replace the six `bool` fields with a single `flags: TtyFlags` field.
- State transitions use `.insert()`, `.remove()`, `.contains()` — same semantics, better ergonomics.
- Add a compile-time assertion or runtime debug check: `HUNG_UP` and `OUTPUT_STOPPED` are mutually exclusive.

### 20.4 Convert `packet_events` to bitflags

**Problem**: `packet_events: u8` uses raw `|=` operations with `TIOCPKT_*` constants. No type safety — invalid combinations or wrong constants could be OR'd in silently.

**Fix**:
- Define `PacketEvents` bitflags type wrapping the `TIOCPKT_*` constants:
  ```rust
  bitflags! {
      #[derive(Clone, Copy, Debug, Default)]
      pub(crate) struct PacketEvents: u8 {
          const FLUSHREAD   = TIOCPKT_FLUSHREAD as u8;
          const FLUSHWRITE  = TIOCPKT_FLUSHWRITE as u8;
          const STOP        = TIOCPKT_STOP as u8;
          const START       = TIOCPKT_START as u8;
          const NOSTOP      = TIOCPKT_NOSTOP as u8;
          const DOSTOP      = TIOCPKT_DOSTOP as u8;
          const IOCTL       = TIOCPKT_IOCTL as u8;
      }
  }
  ```
- Replace `packet_events: u8` with `packet_events: PacketEvents`.

### 20.5 Remove `TtyDriverKind::None` + `active` redundancy

**Problem**: `TTY_SLOTS` is `[IrqMutex<Option<Tty>>; MAX_TTYS]` — the `Option` already encodes "slot is empty." But `TtyDriverKind::None` at `driver.rs:125` and `active: bool` at `mod.rs:94` redundantly encode the same thing. This is more C-like than Rust-like — in Rust, `Option` **is** the empty state.

**Fix**:
- Remove `TtyDriverKind::None` variant. A `Tty` always has a real driver.
- Remove `active: bool` field. If a slot contains `Some(tty)`, it's active. If `None`, it's not.
- Audit all code that checks `tty.active` or matches `TtyDriverKind::None` and replace with `Option` pattern matching on the slot itself.
- If `TtyDriverKind::None` is used as a placeholder during construction, use a builder pattern or two-phase init instead.

### 20.6 Verification

- `just build` — primary gate (this is a pure refactor, no behavioral changes).
- `just test` — all existing tests pass unchanged.
- Grep confirms no `pub` fields remain on `Tty` or `TtySession` (only `pub(crate)` or private).
- Grep confirms no raw `packet_events |=` operations remain (all through `PacketEvents` methods).
- Grep confirms no `TtyDriverKind::None` or `tty.active` references remain.
- Regression: zero behavioral changes — only visibility and type changes.

### 20.7 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Privatize all `Tty` fields to `pub(crate)`, replace booleans with `TtyFlags`, replace `packet_events: u8` with `PacketEvents`, remove `active: bool`, add state-transition domain methods |
| `drivers/src/tty/session.rs` | Privatize `TtySession` fields to `pub(crate)` |
| `drivers/src/tty/driver.rs` | Remove `TtyDriverKind::None` variant |
| `drivers/src/tty/io.rs` | Update field access to use `pub(crate)` methods / `TtyFlags` / `PacketEvents` |
| `drivers/src/tty/lifecycle.rs` | Update `active` checks to use `Option` presence, update hangup to use `TtyFlags` |
| `drivers/src/tty/poll.rs` | Update field access for `TtyFlags` |
| `drivers/src/tty/termios.rs` | Update field access for `TtyFlags` |
| `drivers/src/tty/pty.rs` | Update field access for `TtyFlags`, `PacketEvents` |
| `drivers/src/tty/table.rs` | Remove any `TtyDriverKind::None` init patterns |
| `drivers/src/tty_tests.rs` | Update direct field access in tests to use `pub(crate)` access patterns, update flag comparisons |

---

## 24. Phase 21: Deferred Actions RAII & Boilerplate Reduction

**Status**: Pending

> **Priority**: P2 — ergonomics and maintainability. The repeated "capture signal inside lock → deliver after lock drop" pattern appears ~8 times in `io.rs` alone. The `LdiscOps` trait has ~200 lines of pure forwarding boilerplate. The write path acquires the slot lock 3-4 separate times per iteration.
> **Principle**: Use Rust RAII patterns to eliminate error-prone manual deferred-action sequences. Reduce trait boilerplate without losing the enum dispatch advantage. Cache invariant values to reduce redundant lock acquisitions.

This phase is a pure refactor — no behavioral changes.

### 21.1 `PostLockWork` RAII helper for deferred actions

**Problem**: The pattern of "capture signal/IXOFF/packet-event inside lock → deliver after lock drop" is repeated ~8 times in `io.rs` and appears in `poll.rs`, `lifecycle.rs`, and `termios.rs`. Each instance has identical boilerplate:
```rust
let deferred_signal = { /* ... work under lock ... */ };
// Deliver outside lock:
if let Some((pgid, sig)) = deferred_signal {
    if pgid != 0 {
        let _ = signal_process_group(pgid, sig);
    }
}
```

This is error-prone — it's easy to forget the post-lock delivery, or to add a new deferred action without updating all sites.

**Fix**:
- Define a `PostLockWork` struct that accumulates deferred work:
  ```rust
  #[derive(Default)]
  pub(crate) struct PostLockWork {
      signal: Option<(u32, u8)>,          // (pgid, signum)
      ixoff_byte: Option<(TtyIndex, u8)>, // (target_idx, xoff/xon byte)
      packet_event: Option<(TtyIndex, u8)>,
      wake_slots: [bool; MAX_TTYS],       // slots to wake
  }

  impl PostLockWork {
      /// Execute all accumulated deferred work. Call AFTER dropping locks.
      pub fn execute(self) {
          if let Some((pgid, sig)) = self.signal {
              if pgid != 0 {
                  let _ = signal_process_group(pgid, sig);
              }
          }
          // ... ixoff, packet events, wakes ...
      }
  }
  ```
- All code that currently does manual deferred delivery instead builds a `PostLockWork`, and calls `.execute()` after the lock drops.
- This is a Rust RAII pattern that C kernels can't easily replicate — it's one of the places where Rust genuinely improves over Linux's approach.

### 21.2 Write path lock consolidation

**Problem**: In `io.rs:write()`, each iteration of the write loop acquires the slot lock separately for: (a) peer slot resolution for throttle check, (b) output-stop check, (c) output processing via ldisc, plus potentially the peer slot lock for throttle check. For high-throughput PTY traffic (tmux rendering, compilation output), this creates measurable overhead.

**Fix**:
- Cache `peer_slave_slot` / `peer_master_slot` once at the start of the write function. PTY peer relationships don't change for the lifetime of the FD — they're set at allocation and fixed until the pair is freed.
- Merge the output-stop check and ldisc output processing into a single lock acquisition per loop iteration (they both operate on the same slot lock).
- The throttle check on the peer slot is a separate lock — that remains separate but uses the cached peer index.

### 21.3 `LdiscOps` trait boilerplate reduction

**Problem**: The `LdiscOps` trait has 20+ methods, and both `LineDisc` and `RawDisc` have manual `impl LdiscOps for ...` blocks that forward to identically-named inherent methods — lines 1767-1884 and 2067-2158 in `ldisc.rs` (~200 lines of pure `#[inline] fn foo(&self) -> T { self.foo() }` forwarding).

**Fix options** (choose one):
- **Option A — Derive macro**: Write a simple `#[derive(LdiscOps)]` proc macro that generates the forwarding impls. More complex to build but zero ongoing maintenance.
- **Option B — Remove inherent methods**: Move the logic directly into the `impl LdiscOps` blocks and delete the inherent methods. This means the implementations live in the trait impl, not in `impl LineDisc`. Slightly less ergonomic for internal callers but eliminates all forwarding.
- **Option C — Accept the boilerplate**: The `dispatch_ldisc!` macro already handles `LdiscKind → variant` dispatch. The trait forwarding is verbose but not wrong. Mark it with a `// Forwarding boilerplate — see LdiscOps trait` comment block and move on.

Recommendation: **Option B** — it's the simplest reduction and stays idiomatic. The inherent methods don't add value when the trait impls are the actual interface.

### 21.4 Verification

- `just build` + `just test` — primary gate (pure refactor).
- Benchmark: write throughput on PTY path should improve measurably (fewer lock acquisitions per iteration).
- Code review: grep for manual `signal_process_group` calls outside `PostLockWork::execute()` — should be zero after migration.
- Regression: all existing tests pass unchanged.

### 21.5 Files expected to change

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Define `PostLockWork` struct |
| `drivers/src/tty/io.rs` | Refactor all deferred-action patterns to use `PostLockWork`; cache peer slot index in write path |
| `drivers/src/tty/poll.rs` | Use `PostLockWork` for deferred signal delivery in `poll_events()` |
| `drivers/src/tty/lifecycle.rs` | Use `PostLockWork` in hangup path |
| `drivers/src/tty/termios.rs` | Use `PostLockWork` where applicable |
| `drivers/src/tty/ldisc.rs` | Reduce `LdiscOps` boilerplate (Option A, B, or C) |
| `drivers/src/tty_tests.rs` | Regression tests, optional throughput benchmark |

---

## 25. Phase 22: Forward-Looking — TIOCGPTPEER & Flip-Buffer Architecture

**Status**: Pending

> **Priority**: P3 — nice-to-have improvements that prepare the TTY subsystem for future growth (modern terminal emulators, async serial drivers). Neither is needed for current functionality.
> **Principle**: Build the foundation now so future work (USB-serial, real UART interrupts, modern tmux) doesn't require architectural rework.

### 22.1 TIOCGPTPEER ioctl

**Problem**: `TIOCGPTPEER` (added in Linux 4.13) allows opening the slave side of a PTY directly from the master's file descriptor, without needing to resolve the `/dev/pts/N` path. Modern `tmux` and container runtimes use this for race-free PTY slave opens.

**What Linux does**: `ioctl(master_fd, TIOCGPTPEER, flags)` → returns an open file descriptor to the corresponding slave, equivalent to `open(ptsname(master_fd), flags)` but atomic.

**Fix**:
- Add `TIOCGPTPEER` constant to ABI (`0x5441`, matching Linux).
- In the ioctl handler, verify the FD is a PTY master.
- Resolve the peer slave index via the existing `PtyPeerHandle`.
- Allocate a new file descriptor for the slave and return it.
- Apply `flags` argument (O_RDWR, O_NOCTTY, etc.) to the new FD.
- This requires the ioctl handler to be able to create file descriptors — check if the existing SlopOS syscall infrastructure supports this.

### 22.2 Flip-buffer architecture for interrupt-driven input

**Problem**: Currently `drain_hw_input_locked()` uses a 64-byte stack buffer and synchronous polling. When SlopOS adds real UART interrupt-driven serial or USB-serial, the ISR can't call `input_char()` directly (it holds the interrupt lock, and `input_char()` needs the slot lock — deadlock). Linux solves this with `tty_flip_buffer_push()` — a lock-free producer/consumer buffer between ISR context and process context.

**Current state**: The current drivers (PS/2 keyboard, QEMU serial via port I/O) are polling-based, so this isn't needed today. But adding USB-serial or real UART interrupts will require this infrastructure.

**Design sketch** (do not implement until needed):
- Define a `FlipBuf` per-TTY: a lock-free single-producer single-consumer ring buffer.
- ISR path (producer): push raw `InputEvent`s into `FlipBuf` without any locks.
- `drain_hw_input_locked()` path (consumer): drain `FlipBuf` into the line discipline under the slot lock.
- The `FlipBuf` size should be at least 512 bytes (Linux uses `TTY_FLIPBUF_SIZE = 512`).
- Use `core::sync::atomic` for the producer/consumer indices — no locks needed.

**Implementation approach**:
- Add a `FlipBuf<const N: usize>` struct to a new `drivers/src/tty/flipbuf.rs` module.
- Each `Tty` optionally holds a `FlipBuf` (only for interrupt-driven drivers, not for PTYs or polling drivers).
- The `TtyDriver` trait gains a `uses_flip_buffer() -> bool` method (default: `false`).
- `drain_hw_input_locked()` checks if the driver uses flip buffers and drains from there instead of calling `driver.read()`.

### 22.3 Verification

- Test: `TIOCGPTPEER` on PTY master returns valid slave FD.
- Test: `TIOCGPTPEER` on non-PTY-master returns `-ENOTTY`.
- Test: slave FD from `TIOCGPTPEER` is usable for read/write.
- Test: `TIOCGPTPEER` with `O_NOCTTY` does not acquire controlling terminal.
- Test: FlipBuf producer/consumer correctness under concurrent access (unit test, not full kernel test).
- Test: FlipBuf overflow behavior (oldest data dropped or producer blocks).
- Regression: all existing PTY tests pass.
- `just build` + `just test` gate.

### 22.4 Files expected to change

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Add `TIOCGPTPEER` constant |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Wire `TIOCGPTPEER` with FD allocation |
| `drivers/src/tty/pty.rs` | Helper for peer-FD creation from master |
| `drivers/src/tty/flipbuf.rs` | **NEW** — Lock-free SPSC ring buffer for ISR→ldisc data flow |
| `drivers/src/tty/mod.rs` | Optional `FlipBuf` field on `Tty`, module declaration |
| `drivers/src/tty/driver.rs` | `uses_flip_buffer()` method on `TtyDriver` trait |
| `drivers/src/tty/io.rs` | `drain_hw_input_locked()` flip-buffer drain path |
| `drivers/src/tty_tests.rs` | TIOCGPTPEER tests, FlipBuf unit tests |

---

## 26. File Inventory (Phases 1–16)

### Files modified (all complete)

| File | Phases | Nature of change |
|------|--------|-----------------|
| `drivers/src/tty/mod.rs` | 1, 2, 3, 4, 5, 7, 8, 9, 10, 12, 13 | Poll wake targeting, throttle state + checks, push_cooked caller updates, c_ispeed/c_ospeed population, ioctl wiring, ERESTARTSYS, TCXONC behavior, output queue query, wake batching, no_room recovery, drain hardening |
| `drivers/src/tty/table.rs` | 1, 2, 9, 13 | Replace POLL_NOTIFY with per-slot poll waiters, potential throttle waitqueue, output queue accounting hardening |
| `drivers/src/tty/ldisc.rs` | 2, 3, 4, 6, 10, 11, 12 | Expose cooked buffer occupancy, push_cooked return value + IMAXBEL, default c_cflag update, EDIT_BUF_SIZE change, wake batching support, TABDLY/XTABS handling, no_room recovery hooks |
| `drivers/src/tty/pty.rs` | 1, 2, 8, 13 | PTY cross-wake per-slot targeting, master write back-pressure, TCXONC stop/start integration, drain semantics audit |
| `drivers/src/tty_tests.rs` | 1–13 | New regression tests for every phase, including Tier 1-3 additions |
| `abi/src/syscall.rs` | 4, 5, 7, 9, 11 | c_cflag constants, ioctl constants, ERESTARTSYS, TIOCOUTQ, TABDLY/XTABS constants |
| `abi/src/signal.rs` | 7 | Ensure SA_RESTART defined |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | 1, 5, 9 | Per-slot poll registration, TCFLSH/TCSBRK/TCXONC dispatch, TIOCOUTQ dispatch |
| `fs/src/fileio.rs` | 1 | Poll routing with TTY index for per-slot registration |
| `lib/src/kernel_services/syscall_services/tty.rs` | 5 | Service bridge for new ioctls |
| `drivers/src/syscall_services_init.rs` | 5 | Register new ioctl service methods |
| `core/src/syscall/core_handlers.rs` | 7 | Syscall return path restart logic |
| `core/src/scheduler/task.rs` | 7 | SA_RESTART helper |

### No new files expected

All changes are modifications to existing files. The TTY module structure is complete.

---

## 27. Appendix: Review Findings Reference

### Comparative review methodology

The gaps in this plan were identified through:

1. **Full source read** of all 8 TTY implementation files (~6000 lines total)
2. **Complete ABI audit** of `abi/src/syscall.rs` termios types and constants
3. **Comparison against Linux `drivers/tty/n_tty.c`** (~3500 lines) for buffer architecture, flow control, signal restart, and poll implementation
4. **Comparison against RedoxOS `ptyd`** for PTY patterns and Rust-idiomatic approaches
5. **Grep for TODO/FIXME/HACK** — none found (the overhaul was thorough)

### What was explicitly NOT recommended

The review identified several Linux/RedoxOS patterns that were **rejected** as unnecessary for SlopOS:

| Pattern | Why rejected |
|---------|-------------|
| Separate `tty_port` abstraction | Only needed for USB-serial hotplug. Current `TtyDriverKind` is cleaner for QEMU-only. |
| Separate echo buffer | Cosmetic improvement only. Synchronous echo via `InputAction::Echo` works correctly. |
| Async write buffer | Only matters for real 9600 baud serial. QEMU virtio-serial has negligible latency. |
| Output delay flags (OFILL/OFDEL) | Legacy terminal mechanisms. Almost nothing uses them. Not worth the code. |
| FLUSHO flag | Removed from modern Linux. Dead feature. |
| TIOCSTI ioctl | Security-sensitive (inject input). Linux restricts to `CAP_SYS_ADMIN`. Defer indefinitely. |

### Things done well (preserve these)

These patterns from the overhaul should NOT be changed:

- Per-slot `IrqMutex` with "never hold two slot locks" invariant
- Generation-tagged `PtyPeerHandle` (better than Linux)
- Type-safe bitflags + `CcIndex` enum (better than Linux)
- `NonZeroU32` newtypes for `SessionId`/`ProcessGroupId`
- `TtyError` with exhaustive variants + `to_errno()`
- `dispatch_ldisc!` macro for `LdiscKind` dispatch
- Split-write pattern (process under lock, hardware IO unlocked)
- Deferred signal delivery outside locks
- Clean vtparser/vconsole separation
