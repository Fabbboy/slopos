//! The TTY output boundary — the only place a byte reaches a TTY driver.
//!
//! # Lock ordering
//!
//! `TTY_WRITE_LOCKS[i]` → `TTY_SLOTS[j]`, never the reverse: a driver write
//! for a PTY end delivers into the *peer's* slot. [`TTY_WRITE_LOCKS`] is
//! private to this module and neither `Tty` nor `TtyDriverKind` exposes a
//! byte-emitting method, so a frame holding a slot guard — the only way to
//! reach a `&mut Tty` — has no path to a driver. Echo the line discipline
//! produces under that guard is staged in its own queue and drained here.

use core::sync::atomic::Ordering;

use slopos_ostd::lock_class;
use slopos_ostd::sync::{BUS, LOCK_LEVEL_RESOURCE, LockClassKey, SpinLock, SpinLockGuard};

use super::driver::DriverId;
use super::table::{TTY_OUTPUT_INFLIGHT, TTY_SLOTS, tty_output_event};
use super::{MAX_TTYS, vconsole};
use crate::serial;

/// Subclass for the *peer's* acquisition of [`TTY_WRITE_LOCKS`].
///
/// A PTY master write holds the master's write lock and, pushing the bytes
/// into the slave as input, takes the slave's to emit the slave's echo of
/// them. Both are instances of one declaration, so without a subclass the pair
/// is indistinguishable from an unordered same-class nesting. `0 -> 1` is the
/// only legal direction and lockdep enforces it.
const TTY_WRITE_PEER_SUBCLASS: u8 = 1;

/// Lockdep class of every [`TTY_WRITE_LOCKS`] instance.
pub(crate) const TTY_WRITE_CLASS: &LockClassKey =
    lock_class!("TTY_WRITE_LOCKS", LOCK_LEVEL_RESOURCE);

/// Per-TTY write serialization locks.
///
/// Serializes all output to a TTY's driver — both echo and user writes.
/// Without this they interleave at the driver level and corrupt terminal
/// output; POSIX §11.1.9 requires echo to be indistinguishable from
/// terminal-generated output, which implies the serialisation.
///
/// The analogue of n_tty's `ldata->output_lock`, the lock held across driver
/// emission on both paths. Linux's `atomic_write_lock` sits above the line
/// discipline and serialises whole `write(2)` calls, a different job.
static TTY_WRITE_LOCKS: [SpinLock<()>; MAX_TTYS] =
    [const { SpinLock::new((), TTY_WRITE_CLASS) }; MAX_TTYS];

/// Bytes handed to the driver per call while draining echo. Bounds the
/// interrupts-off window on the write lock: polled serial is ~86 µs/byte, so
/// one chunk is ~5.5 ms.
const ECHO_CHUNK: usize = 64;

/// Chunks one flush will move. A larger queue drains across several flush
/// points rather than in one interrupts-off run; the idle sweep re-offers
/// whatever is left.
const MAX_FLUSH_ROUNDS: usize = 16;

/// Which acquisition of [`TTY_WRITE_LOCKS`] an emission is.
///
/// `PeerNested` is the slave-side write a PTY master's write reaches while
/// still holding the master's own. Declared by the caller rather than inferred
/// from the held stack, so a caller that is not nested cannot claim it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WriteNesting {
    Toplevel,
    PeerNested,
}

// ---------------------------------------------------------------------------
// In-flight accounting
// ---------------------------------------------------------------------------

/// Decrements `TTY_OUTPUT_INFLIGHT[slot]` on drop. Every emission is wrapped
/// in one, so `tcdrain` / `TCSETSW` / `TIOCOUTQ` observe the bytes between the
/// line discipline and the hardware.
struct InflightGuard {
    slot: usize,
    count: u32,
}

impl InflightGuard {
    #[inline]
    fn new(slot: usize, count: usize) -> Self {
        let count = count as u32;
        TTY_OUTPUT_INFLIGHT[slot].fetch_add(count, Ordering::Release);
        Self { slot, count }
    }
}

impl Drop for InflightGuard {
    #[inline]
    fn drop(&mut self) {
        // Underflow-safe decrement: a concurrent flush (`store(0)` in the
        // signal-flush / TCOFLUSH / TCIOFLUSH paths) can zero the counter
        // between this guard's `fetch_add` and this `Drop`. A plain
        // `fetch_sub` would then wrap the `AtomicU32` to ~u32::MAX, wedging
        // `wait_output_idle()` (tcdrain / TCSETSW / TCSETSF) and poisoning
        // `output_queued_bytes()` (TIOCOUTQ). Saturate at 0 instead — the
        // flush already accounts for the discarded output.
        let mut cur = TTY_OUTPUT_INFLIGHT[self.slot].load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(self.count);
            match TTY_OUTPUT_INFLIGHT[self.slot].compare_exchange_weak(
                cur,
                next,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Driver dispatch
// ---------------------------------------------------------------------------

/// Whether a write to `driver` can leave work that must run outside the write
/// lock. Sampled before the write, which consumes the id.
fn defers_console_work(driver: &DriverId) -> bool {
    matches!(driver, DriverId::VConsole)
}

/// Run console work an emission deferred.
///
/// Call with `TTY_WRITE_LOCKS[slot]` released. That lock serialises byte
/// streams and disables interrupts; a full-screen vconsole repaint takes the
/// console lock in bands precisely so interrupts are not masked across the
/// whole screen, which holds only if the write lock is not wrapped around it.
fn settle_console_output() {
    vconsole::run_pending_repaint();
}

/// Send `data` to the backend `driver` names.
///
/// The serial and vconsole arms take the klog ticket so their bytes never
/// interleave with concurrent `klog_*!` output.
fn write_to_driver(driver: DriverId, data: &[u8]) -> usize {
    match driver {
        DriverId::SerialConsole => {
            serial::serial_locked_write_bytes(data);
            data.len()
        }
        DriverId::VConsole => {
            vconsole::write(data);
            if vconsole::serial_mirror_enabled() {
                serial::serial_locked_write_bytes(data);
            }
            data.len()
        }
        DriverId::PtyMaster { peer } => super::pty::master_write(&peer, data),
        DriverId::PtySlave { peer } => super::pty::slave_write(&peer, data),
    }
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

#[inline]
fn write_guard(slot: usize, nesting: WriteNesting) -> SpinLockGuard<'static, ()> {
    match nesting {
        WriteNesting::Toplevel => TTY_WRITE_LOCKS[slot].lock(),
        WriteNesting::PeerNested => TTY_WRITE_LOCKS[slot].lock_nested(TTY_WRITE_PEER_SUBCLASS),
    }
}

/// Emit `bytes` under `TTY_WRITE_LOCKS[slot]`, accounted as in-flight. The
/// caller settles console work and publishes the output event, both of which
/// must run with the write lock released.
fn emit_locked(slot: usize, driver: DriverId, bytes: &[u8], nesting: WriteNesting) -> usize {
    debug_check_emit_contract(slot, nesting);
    let _write = write_guard(slot, nesting);
    let _inflight = InflightGuard::new(slot, bytes.len());
    write_to_driver(driver, bytes)
}

/// Emit already-processed output for `slot`. The caller must have released
/// `TTY_SLOTS[slot]`; `driver` is the id it cloned out from under that guard.
pub(crate) fn write_processed(
    slot: usize,
    driver: DriverId,
    bytes: &[u8],
    nesting: WriteNesting,
) -> usize {
    if slot >= MAX_TTYS {
        return 0;
    }
    let settle = defers_console_work(&driver);
    let written = emit_locked(slot, driver, bytes, nesting);
    if settle {
        settle_console_output();
    }
    BUS.publish(tty_output_event(slot));
    written
}

/// Drain `slot`'s staged echo to its driver.
///
/// Reached only from `PostLockWork::execute`, and only for the slot whose own
/// guard the caller just dropped — never a peer's. Flushing a peer would take
/// its write lock while holding this slot's, which is the inverse of the one
/// legal nesting direction.
pub(crate) fn flush_echo(slot: usize, nesting: WriteNesting) {
    if slot >= MAX_TTYS {
        return;
    }
    let Some(_claim) = EchoDrain::acquire(slot) else {
        return;
    };

    let mut chunk = [0u8; ECHO_CHUNK];
    let mut settle = false;
    let mut wrote_any = false;

    for _ in 0..MAX_FLUSH_ROUNDS {
        let (n, driver) = {
            let mut guard = TTY_SLOTS[slot].lock();
            let Some(tty) = guard.as_mut() else { break };
            let n = tty.ldisc.echo_take(&mut chunk);
            if n == 0 {
                break;
            }
            (n, tty.driver.id())
        };
        settle |= defers_console_work(&driver);
        let written = emit_locked(slot, driver, &chunk[..n], nesting);
        wrote_any = true;
        if written < n {
            // The peer's input queue is full. Put the tail back and stop; its
            // reader publishes this slot's output event as it drains, and the
            // idle sweep re-offers every non-empty queue regardless.
            let mut guard = TTY_SLOTS[slot].lock();
            if let Some(tty) = guard.as_mut() {
                tty.ldisc.echo_unread(&chunk[written..n]);
            }
            break;
        }
    }

    if settle {
        settle_console_output();
    }
    if wrote_any {
        BUS.publish(tty_output_event(slot));
    }
}

/// One drainer per slot: two CPUs would take alternate chunks and race for the
/// write lock, reordering bytes on the wire. A flag under the slot lock rather
/// than a lock of its own, so it adds no class and cannot join a cycle.
struct EchoDrain {
    slot: usize,
}

impl EchoDrain {
    fn acquire(slot: usize) -> Option<Self> {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = guard.as_mut()?;
        if tty.ldisc.echo_is_empty() || !tty.ldisc.echo_claim_drain() {
            return None;
        }
        Some(Self { slot })
    }
}

impl Drop for EchoDrain {
    fn drop(&mut self) {
        let mut guard = TTY_SLOTS[self.slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.ldisc.echo_release_drain();
        }
    }
}

// ---------------------------------------------------------------------------
// The emit contract, checked at first execution
// ---------------------------------------------------------------------------

/// Assert no `TTY_SLOTS` guard is live, and that the only write lock already
/// held is the peer's under [`WriteNesting::PeerNested`].
///
/// Lockdep reports this pair only once it has learned the opposite edge, which
/// depends on what ran earlier in the boot; this fires on first execution
/// regardless of graph history. Comparing recorded addresses against
/// `&TTY_SLOTS[i]` is sound because `SpinLock<T>` is `#[repr(C)]` with its
/// `LockCore` at offset 0.
#[cfg(debug_assertions)]
#[inline(never)]
fn debug_check_emit_contract(slot: usize, nesting: WriteNesting) {
    let mut held = [0u64; 16];
    let n = slopos_ostd::sync::held_lock_addrs(&mut held);
    let mut write_locks_held = 0usize;
    for &addr in &held[..n] {
        for i in 0..MAX_TTYS {
            debug_assert!(
                addr != &TTY_SLOTS[i] as *const _ as u64,
                "TTY output with TTY_SLOTS[{i}] held: the write lock is outside every slot lock"
            );
            if addr == &TTY_WRITE_LOCKS[i] as *const _ as u64 {
                write_locks_held += 1;
            }
        }
    }
    match nesting {
        WriteNesting::Toplevel => debug_assert!(
            write_locks_held == 0,
            "top-level TTY output for slot {slot} with a write lock already held"
        ),
        WriteNesting::PeerNested => debug_assert!(
            write_locks_held == 1,
            "peer-nested TTY output for slot {slot} with {write_locks_held} write locks held"
        ),
    }
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn debug_check_emit_contract(_slot: usize, _nesting: WriteNesting) {}
