# SlopOS TTY Finishing Touches Plan

> **Status**: 📋 All 7 phases planned — post-overhaul hardening & gold-standard completion
> **Predecessor**: TTY Overhaul Plan (42 phases, all complete) — the foundational rewrite from global singleton to per-terminal subsystem
> **Target**: Close the remaining architectural gaps identified in the Linux N_TTY / RedoxOS comparative review, bringing the TTY subsystem to production-grade quality
> **Current**: `drivers/src/tty/` — 8 files, ~6000 lines. Clean per-TTY API, PTY with generation-safe peer handles, per-slot locking, full POSIX termios flag coverage (c_iflag, c_oflag, c_lflag, c_cc), session/job control, VT100 emulation, packet mode, EXTPROC, vhangup. Zero TODO/FIXME/HACK comments.

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
11. [File Inventory](#11-file-inventory)
12. [Appendix: Review Findings Reference](#12-appendix-review-findings-reference)

---

## 1. Executive Summary

The 42-phase TTY overhaul transformed SlopOS from a global singleton line discipline behind a single `IrqMutex` into a proper per-terminal subsystem with PTY support, session/job control, VT100 emulation, and near-complete POSIX termios coverage. The subsystem is genuinely well-engineered — generation-tagged PTY peer handles, type-safe bitflags, deferred signal delivery outside locks, and a clean split-write pattern are all production-quality patterns.

A comparative review against Linux N_TTY and RedoxOS identified **7 remaining gaps** ranging from critical (silent data loss on PTY overflow) to architectural (signal restart). This plan addresses each gap as an independent phase, ordered by priority.

### Summary of phases

| Phase | What | Priority | Effort | Status |
|-------|------|----------|--------|--------|
| 1 | Per-TTY poll notification (replace thundering herd) | P0 | Small | **DONE** |
| 2 | PTY flow control / throttle mechanism | P0 | Medium | **TODO** |
| 3 | Cooked buffer overflow hardening | P1 | Small | **TODO** |
| 4 | c_cflag ABI completion (constants + defaults) | P1 | Small | **TODO** |
| 5 | Missing ioctls (TCFLSH, TCSBRK, TCXONC) | P1 | Small | **TODO** |
| 6 | Edit buffer expansion (1024 → 4096) | P2 | Trivial | **TODO** |
| 7 | Signal restart infrastructure (ERESTARTSYS) | P2 | Large | **DONE** ✅ |

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
| c_cflag | **1/30+** | ⚠️ Only CREAD defined — biggest ABI gap |
| c_cc | **17/17** | All control character indices implemented |
| Ioctls | **21/25** | Missing TCFLSH, TCSBRK, TCXONC, TIOCSTI |

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

## 11. File Inventory

### Files to modify

| File | Phases | Nature of change |
|------|--------|-----------------|
| `drivers/src/tty/mod.rs` | 1, 2, 3, 4, 5, 7 | Poll wake targeting, throttle state + checks, push_cooked caller updates, c_ispeed/c_ospeed population, ioctl wiring, ERESTARTSYS |
| `drivers/src/tty/table.rs` | 1, 2 | Replace POLL_NOTIFY with per-slot poll waiters, potential throttle waitqueue |
| `drivers/src/tty/ldisc.rs` | 2, 3, 4, 6 | Expose cooked buffer occupancy, push_cooked return value + IMAXBEL, default c_cflag update, EDIT_BUF_SIZE change |
| `drivers/src/tty/pty.rs` | 1, 2 | PTY cross-wake per-slot targeting, master write back-pressure |
| `drivers/src/tty_tests.rs` | 1–7 | New regression tests for every phase |
| `abi/src/syscall.rs` | 4, 5, 7 | c_cflag constants, ioctl constants, ERESTARTSYS |
| `abi/src/signal.rs` | 7 | Ensure SA_RESTART defined |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | 1, 5 | Per-slot poll registration, TCFLSH/TCSBRK/TCXONC dispatch |
| `fs/src/fileio.rs` | 1 | Poll routing with TTY index for per-slot registration |
| `lib/src/kernel_services/syscall_services/tty.rs` | 5 | Service bridge for new ioctls |
| `drivers/src/syscall_services_init.rs` | 5 | Register new ioctl service methods |
| `core/src/syscall/core_handlers.rs` | 7 | Syscall return path restart logic |
| `core/src/scheduler/task.rs` | 7 | SA_RESTART helper |

### No new files expected

All changes are modifications to existing files. The TTY module structure is complete.

---

## 12. Appendix: Review Findings Reference

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
