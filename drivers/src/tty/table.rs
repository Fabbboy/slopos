//! Global TTY table — the central registry of all terminal instances.
//!
//! # Lock Architecture
//!
//! Each TTY slot has its own `IrqMutex`, enabling fully independent
//! operations on different TTYs.  There is no global table lock — each slot
//! in `TTY_SLOTS` is independently locked.
//!
//! This replaces the previous `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>`
//! where a single lock protected **all** 8 TTY slots.  Under the old scheme,
//! any operation on TTY 0 blocked all operations on TTY 1–7.  A 1 KB serial
//! write held the global lock for ~86 ms.
//!
//! ## Lock Ordering Rules
//!
//! Strict lock hierarchy to prevent deadlock:
//!
//! 1. **`TTY_SLOTS[i]`** (per-TTY) — held for ldisc/session/termios
//!    operations.  **Never hold two per-TTY locks simultaneously.**
//! 2. **`TTY_INPUT_WAITERS[i]`** — never hold a per-TTY slot lock while
//!    performing a blocking wait.  The `wait_event` condition closure may
//!    transiently acquire the same per-TTY lock (this is safe because
//!    `wait_event` releases its internal lock before calling the closure).
//!
//! Rule: **Never acquire `TTY_SLOTS[j]` while holding `TTY_SLOTS[i]`**
//!       (for `i ≠ j`).  Functions that iterate all slots (like
//!       `detach_session_by_id`) acquire and release each lock in turn.
//!
//! `TTY_INPUT_WAITERS` is a **separate** static array of `WaitQueue`s — one
//! per TTY slot.  They live outside `TTY_SLOTS` so that `read()` can call
//! `wait_event(|| ...)` without holding the slot lock (the condition closure
//! locks the slot internally to check for data).

use core::sync::atomic::AtomicU32;

use super::driver::{SerialConsoleDriver, TtyDriverKind, VConsoleDriver};
use super::ldisc::{LdiscKind, LineDisc};
use super::pty::PtyPeerHandle;
use super::session::TtySession;
use super::{MAX_TTYS, PacketEvents, Tty, TtyFlags, TtyIndex};
use slopos_abi::syscall::UserWinsize;
use slopos_lib::IrqMutex;
use slopos_lib::WaitQueue;

// ---------------------------------------------------------------------------
// Per-TTY slots
// ---------------------------------------------------------------------------

/// Per-TTY locked slots.  Each element is an independently-locked
/// `Option<Tty>` — operations on TTY 0 never contend with TTY 1–7.
///
/// Slots 0 and 1 are pre-allocated at init time:
/// - 0 → serial console (COM1)
/// - 1 → virtual console (PS/2 keyboard + framebuffer)
///
/// The remaining slots are reserved for future PTY support.
///
/// Access a slot by index: `TTY_SLOTS[idx].lock()`.
pub static TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS] = [const { IrqMutex::new(None) }; MAX_TTYS];

/// Per-TTY input wait queues — separate from TTY_SLOTS to avoid lock ordering
/// issues (read() needs to block on the wait queue while the condition closure
/// independently locks TTY_SLOTS[idx] to check for data).
pub static TTY_INPUT_WAITERS: [WaitQueue; MAX_TTYS] = [const { WaitQueue::new() }; MAX_TTYS];

/// Per-TTY output wait queues — used by `write()` to block when IXON flow
/// control has stopped output (Ctrl+S).  Writers sleep on this queue and are
/// woken when output is resumed (Ctrl+Q or any key with IXON set).
pub static TTY_OUTPUT_WAITERS: [WaitQueue; MAX_TTYS] = [const { WaitQueue::new() }; MAX_TTYS];

/// Per-TTY poll/select notification queues.  Tasks blocked in `poll()` or
/// `select()` register on the specific slot's queue instead of a single
/// global `WaitQueue`.  Any event that could change poll readiness (input
/// arrival, hangup, IXON resume, peer close) wakes only the waiters on
/// the affected slot — eliminating the thundering-herd problem of the old
/// global `POLL_NOTIFY`.
pub static TTY_POLL_WAITERS: [WaitQueue; MAX_TTYS] = [const { WaitQueue::new() }; MAX_TTYS];

/// Per-TTY output-in-flight **byte** counter.  Tracks the number of
/// bytes that have been processed through the line discipline but have
/// not yet completed the unlocked hardware write.  Used by
/// `wait_output_idle()` to block `TCSETSW` / `TCSETSF` until all
/// in-flight output reaches the hardware, and by `TIOCOUTQ` to report
/// accurate queue depth.
///
/// Increment by the chunk byte count **before** `write_driver_unlocked`,
/// decrement by the same count **after**, then wake
/// `TTY_OUTPUT_WAITERS` so drain waiters re-check.
pub static TTY_OUTPUT_INFLIGHT: [AtomicU32; MAX_TTYS] = [const { AtomicU32::new(0) }; MAX_TTYS];

/// Per-TTY generation counter.  Incremented each time a slot transitions
/// from allocated → free (`*slot = None`).  Used by `PtyPeerHandle` to
/// detect stale references after rapid PTY free/reuse cycles.
///
/// A write to a peer whose generation no longer matches is silently
/// discarded — the peer slot may have been freed and reallocated to an
/// unrelated PTY pair.
pub static TTY_GENERATIONS: [AtomicU32; MAX_TTYS] = [const { AtomicU32::new(0) }; MAX_TTYS];

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialise the TTY table.  Must be called once during early boot, after
/// the serial port is ready.
///
/// Allocates:
/// - TTY 0  → SerialConsoleDriver (COM1)
/// - TTY 1  → VConsoleDriver (PS/2 + framebuffer)
pub fn tty_table_init() {
    // Clear all slots first so that tests calling tty_table_init() get a
    // clean table regardless of prior test state (e.g. leftover PTY pairs).
    for i in 0..MAX_TTYS {
        let mut slot = TTY_SLOTS[i].lock();
        *slot = None;
    }

    {
        let mut slot = TTY_SLOTS[0].lock();
        *slot = Some(Tty::new(
            TtyIndex(0),
            TtyDriverKind::SerialConsole(SerialConsoleDriver),
        ));
    }
    {
        let mut slot = TTY_SLOTS[1].lock();
        *slot = Some(Tty::new(
            TtyIndex(1),
            TtyDriverKind::VConsole(VConsoleDriver),
        ));
    }
}

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Execute a closure with a mutable reference to the `Tty` at `idx`, if it
/// exists.  Returns `None` if the slot is empty or index is out of range.
///
/// The per-TTY lock is held for the duration of the closure.
pub fn with_tty<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&mut Tty) -> R,
{
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return None;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    guard.as_mut().map(f)
}

/// Execute a closure with an immutable reference to the `Tty` at `idx`.
pub fn with_tty_ref<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&Tty) -> R,
{
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return None;
    }
    let guard = TTY_SLOTS[slot].lock();
    guard.as_ref().map(f)
}

impl Tty {
    /// Create a new TTY with the given index and driver backend.
    pub fn new(index: TtyIndex, driver: TtyDriverKind) -> Self {
        Self {
            index,
            ldisc: LdiscKind::NTty(LineDisc::new()),
            driver,
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            open_count: 0,
            flags: TtyFlags::empty(),
            packet_events: PacketEvents::empty(),
        }
    }

    pub fn new_pty_master(index: TtyIndex, peer: PtyPeerHandle) -> Self {
        Self {
            index,
            ldisc: LdiscKind::Raw(super::ldisc::RawDisc::new()),
            driver: TtyDriverKind::PtyMaster { peer },
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            open_count: 0,
            flags: TtyFlags::empty(),
            packet_events: PacketEvents::empty(),
        }
    }

    pub fn new_pty_slave(index: TtyIndex, peer: PtyPeerHandle) -> Self {
        Self {
            index,
            ldisc: LdiscKind::NTty(LineDisc::new()),
            driver: TtyDriverKind::PtySlave { peer },
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            open_count: 0,
            flags: TtyFlags::SLAVE_LOCKED,
            packet_events: PacketEvents::empty(),
        }
    }
}

pub fn find_free_slot() -> Option<usize> {
    (2..MAX_TTYS).find(|&slot| TTY_SLOTS[slot].lock().is_none())
}

pub fn find_free_slot_excluding(excluded: usize) -> Option<usize> {
    (2..MAX_TTYS).find(|&slot| slot != excluded && TTY_SLOTS[slot].lock().is_none())
}
