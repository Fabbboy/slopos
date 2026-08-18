//! Global TTY table — the central registry of all terminal instances.
//!
//! Locking: each slot in [`TTY_SLOTS`] is independently locked; there is no
//! global table lock, and two per-TTY locks are never held at once.
//! `super::output`'s `TTY_WRITE_LOCKS[i]` is strictly **outside** every
//! `TTY_SLOTS[j]`, so every output path copies the `DriverId` out from under the
//! slot lock, drops it, and only then emits. Blocking waits go through the
//! kernel event bus; a slot lock is never held across one.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use slopos_ostd::lock_class;

use super::backing::TtyBacking;
use super::driver::{SerialConsoleDriver, TtyDriverKind, VConsoleDriver};
use super::ldisc::{LdiscKind, LineDisc};
use super::session::TtySession;
use super::{MAX_TTYS, PacketEvents, Tty, TtyFlags, TtyIndex};
use slopos_abi::event::{KernelEvent, TtySlot};
use slopos_abi::syscall::UserWinsize;
use slopos_ostd::KWeak;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{AllocError, PinBox};
use slopos_ostd::{AtomicBitmap, words_for};

/// Input-side event for a TTY slot: cooked input or a readable status change.
#[inline]
pub(crate) fn tty_input_event(slot: usize) -> KernelEvent {
    KernelEvent::TtyInput {
        tty: TtySlot(slot as u32),
    }
}

/// Output-side event for a TTY slot: flow control resumed, or a writable
/// status change.
#[inline]
pub(crate) fn tty_output_event(slot: usize) -> KernelEvent {
    KernelEvent::TtyOutput {
        tty: TtySlot(slot as u32),
    }
}

/// Lockdep class of every [`TTY_SLOTS`] instance. Named so its ordering can be
/// declared at init rather than discovered from whichever direction runs first.
pub(crate) const TTY_SLOTS_CLASS: &slopos_ostd::sync::LockClassKey =
    lock_class!("TTY_SLOTS", LOCK_LEVEL_RESOURCE);

/// Per-TTY locked slots. Slot 0 is the serial console (COM1) and slot 1 the
/// virtual console; the rest are PTY ends.
pub static TTY_SLOTS: [SpinLock<Option<PinBox<Tty>>>; MAX_TTYS] =
    [const { SpinLock::new(None, TTY_SLOTS_CLASS) }; MAX_TTYS];

/// Per-TTY output-in-flight **byte** counter: bytes through the line discipline
/// but not yet through the unlocked hardware write. Read by `wait_output_idle()`
/// (`TCSETSW`/`TCSETSF`) and `TIOCOUTQ`; maintained by `output::InflightGuard`.
pub static TTY_OUTPUT_INFLIGHT: [AtomicU32; MAX_TTYS] = [const { AtomicU32::new(0) }; MAX_TTYS];

/// Per-slot weak handle to the live [`TtyBacking`] — the open-by-index registry
/// (`/dev/pts/N`, `/dev/tty`, bootstrap console fds). Weak by design: the
/// registry must never keep a TTY open.
///
/// Lock ordering: `TTY_BACKINGS[i]` → `TTY_SLOTS[j]` (never the reverse).
pub(crate) static TTY_BACKINGS: [SpinLock<KWeak<TtyBacking>>; MAX_TTYS] = [const {
    SpinLock::new(
        KWeak::new(),
        lock_class!("TTY_BACKINGS", LOCK_LEVEL_REGISTRY),
    )
}; MAX_TTYS];

/// Per-slot weak handle to the shared [`TtySlaveOpen`] — alive while any
/// slave fd is open. Same ordering rules as [`TTY_BACKINGS`].
pub(crate) static TTY_SLAVE_OPENS: [SpinLock<KWeak<super::backing::TtySlaveOpen>>; MAX_TTYS] = [const {
    SpinLock::new(
        KWeak::new(),
        lock_class!("TTY_SLAVE_OPENS", LOCK_LEVEL_REGISTRY),
    )
};
    MAX_TTYS];

/// Free a PTY slot after its backing dropped. The `Tty` drop runs outside the
/// lock and the slot is marked reusable last, so an allocator that wins the bit
/// sees a fully-empty slot.
pub(crate) fn free_slot(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let taken = { TTY_SLOTS[slot].lock().take() };
    drop(taken);
    *TTY_BACKINGS[slot].lock() = KWeak::new();
    *TTY_SLAVE_OPENS[slot].lock() = KWeak::new();
    mark_slot_free(slot);
}

/// Allocation bitmap — bit N is set when slot N contains a live `Tty`.
/// Bits 0–1 are always set after init (serial + vconsole).
pub(crate) static TTY_ALLOC_BITMAP: AtomicBitmap<{ words_for(MAX_TTYS) }> = AtomicBitmap::new();

/// Initialise the TTY table: TTY 0 → `SerialConsoleDriver` (COM1), TTY 1 →
/// `VConsoleDriver`. Runs once, during early boot, after the serial port is
/// ready; later calls return without touching the table.
///
/// The once-only guard is load-bearing: the body drops every live `Tty` and
/// clears the whole allocation bitmap while every idle CPU is sweeping those
/// same slots through `input_available_cb`, and there is no point after boot at
/// which that is safe.
pub fn tty_table_init() {
    static INITIALISED: AtomicBool = AtomicBool::new(false);
    if INITIALISED.swap(true, Ordering::AcqRel) {
        return;
    }

    // Before the first TTY lock, so a driver write with a slot guard live is a
    // finding on any boot, not only one that took the legal direction first.
    if let Err(err) =
        slopos_ostd::sync::declare_order(super::output::TTY_WRITE_CLASS, TTY_SLOTS_CLASS)
    {
        panic!("TTY lock order rejected by the validator: {err:?}");
    }

    for i in 0..MAX_TTYS {
        *TTY_BACKINGS[i].lock() = KWeak::new();
        *TTY_SLAVE_OPENS[i].lock() = KWeak::new();
        let mut slot = TTY_SLOTS[i].lock();
        *slot = None;
        TTY_ALLOC_BITMAP.clear(i);
    }

    {
        let mut slot = TTY_SLOTS[0].lock();
        *slot = Some(
            Tty::new(
                TtyIndex(0),
                TtyDriverKind::SerialConsole(SerialConsoleDriver),
            )
            .expect("kernel OOM during serial-console Tty init"),
        );
    }
    {
        let mut slot = TTY_SLOTS[1].lock();
        *slot = Some(
            Tty::new(TtyIndex(1), TtyDriverKind::VConsole(VConsoleDriver))
                .expect("kernel OOM during vconsole Tty init"),
        );
    }
    TTY_ALLOC_BITMAP.set(0);
    TTY_ALLOC_BITMAP.set(1);
}

/// Run `f` with the `Tty` at `idx`, holding its per-TTY lock for the duration.
/// `None` if the slot is empty or the index is out of range.
pub fn with_tty<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&mut Tty) -> R,
{
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return None;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    guard.as_deref_mut().map(f)
}

pub fn with_tty_ref<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&Tty) -> R,
{
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return None;
    }
    let guard = TTY_SLOTS[slot].lock();
    guard.as_deref().map(f)
}

impl Tty {
    /// Heap-allocated through `PinBox` so the struct never materialises on a
    /// caller's stack; `LineDisc` (~12 KiB) dominates the allocation.
    pub fn new(index: TtyIndex, driver: TtyDriverKind) -> Result<PinBox<Self>, AllocError> {
        let ldisc = LdiscKind::NTty(LineDisc::new_pinned()?);
        PinBox::try_new(Self {
            index,
            ldisc,
            driver,
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            flags: TtyFlags::empty(),
            packet_events: PacketEvents::empty(),
        })
    }

    pub fn new_pty_master(
        index: TtyIndex,
        peer: KWeak<TtyBacking>,
    ) -> Result<PinBox<Self>, AllocError> {
        let ldisc = LdiscKind::Raw(super::ldisc::RawDisc::new_pinned()?);
        PinBox::try_new(Self {
            index,
            ldisc,
            driver: TtyDriverKind::PtyMaster { peer },
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            flags: TtyFlags::empty(),
            packet_events: PacketEvents::empty(),
        })
    }

    pub fn new_pty_slave(
        index: TtyIndex,
        peer: KWeak<TtyBacking>,
    ) -> Result<PinBox<Self>, AllocError> {
        let ldisc = LdiscKind::NTty(LineDisc::new_pinned()?);
        PinBox::try_new(Self {
            index,
            ldisc,
            driver: TtyDriverKind::PtySlave { peer },
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            flags: TtyFlags::SLAVE_LOCKED,
            packet_events: PacketEvents::empty(),
        })
    }
}

pub fn find_free_slot() -> Option<usize> {
    let word = TTY_ALLOC_BITMAP.load_word(0);
    let free = !word & !0b11usize;
    if free == 0 {
        return None;
    }
    let slot = free.trailing_zeros() as usize;
    if slot >= MAX_TTYS { None } else { Some(slot) }
}

pub fn find_free_slot_excluding(excluded: usize) -> Option<usize> {
    let word = TTY_ALLOC_BITMAP.load_word(0);
    let mut free = !word & !0b11usize;
    if excluded < MAX_TTYS {
        free &= !(1usize << excluded);
    }
    if free == 0 {
        return None;
    }
    let slot = free.trailing_zeros() as usize;
    if slot >= MAX_TTYS { None } else { Some(slot) }
}

#[inline]
pub(crate) fn mark_slot_allocated(slot: usize) {
    TTY_ALLOC_BITMAP.set(slot);
}

#[inline]
pub(crate) fn mark_slot_free(slot: usize) {
    TTY_ALLOC_BITMAP.clear(slot);
}

#[inline]
pub(crate) fn active_slots_bitmap() -> usize {
    TTY_ALLOC_BITMAP.load_word(0)
}
