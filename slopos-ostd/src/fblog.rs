//! Framebuffer kernel-log console — capture ring + control state.
//!
//! On real hardware there is no serial port to read, so the only way to see
//! kernel output is to render it onto the Limine framebuffer. This module is
//! the kernel-side half of that: it captures the **full serial byte stream**
//! (kernel `klog` *and* userland TTY output — both funnel through
//! [`crate::early_console::write_bytes`]) into an in-memory ring, and drives an
//! optional on-screen renderer registered by the `video` crate.
//!
//! **ESC during boot** toggles it — built in, no flag. ESC reveals the log
//! while the splash / Wheel of Fate is up (and *pauses* the wheel rather than
//! skipping it); once the compositor presents its first frame (the desktop is
//! up) ESC belongs to applications again. This mirrors Plymouth, which only
//! grabs ESC during boot.
//!
//! The renderer is invoked from the scheduler timer tick (so it works even
//! when userland is wedged) and never from inside a log call, avoiding
//! re-entrancy. Capture and read-back use `try_lock`, so this can never
//! deadlock a panic, IRQ, or log writer.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

#[cfg(target_os = "none")]
use crate::lock_class;
#[cfg(target_os = "none")]
use crate::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

// ---------------------------------------------------------------------------
// Capture ring
// ---------------------------------------------------------------------------

// The capture ring lives only on the kernel target — host builds no-op
// `capture`/`ring_copy_tail`, so the ring + its `SpinLock` are absent there.
#[cfg(target_os = "none")]
const RING_SIZE: usize = 64 * 1024;

#[cfg(target_os = "none")]
struct Ring {
    buf: [u8; RING_SIZE],
    /// Total bytes ever written (monotonic). The live data is the last
    /// `min(written, RING_SIZE)` bytes.
    written: u64,
}

#[cfg(target_os = "none")]
static RING: SpinLock<Ring> = SpinLock::new(
    Ring {
        buf: [0u8; RING_SIZE],
        written: 0,
    },
    lock_class!("fblog.RING", LOCK_LEVEL_UNORDERED),
);

/// Lock-free mirror of `Ring::written` so the renderer can cheaply detect
/// "nothing new" without taking the ring lock.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Append raw serial bytes to the capture ring.
///
/// Called from [`crate::early_console::write_bytes`] — the single sink every
/// serial write funnels through. Uses `try_lock`, so a contended ring (another
/// CPU mid-append, or a panic writer) simply drops this fragment rather than
/// blocking.
#[cfg(target_os = "none")]
pub fn capture(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut ring = match RING.try_lock() {
        Some(r) => r,
        None => return,
    };
    for &b in bytes {
        let idx = (ring.written % RING_SIZE as u64) as usize;
        ring.buf[idx] = b;
        ring.written = ring.written.wrapping_add(1);
    }
    SEQ.store(ring.written, Ordering::Relaxed);
}

/// Host builds (unit tests / KernMiri) have no framebuffer ring; capture is
/// a no-op so the serial path doesn't take the kernel `SpinLock`, whose
/// gs-relative preempt asm the host cannot execute.
#[cfg(not(target_os = "none"))]
pub fn capture(_bytes: &[u8]) {}

/// Total bytes ever captured — a lock-free change detector for the renderer.
pub fn ring_seq() -> u64 {
    SEQ.load(Ordering::Relaxed)
}

/// Copy the most-recent bytes into `out` in chronological order. Returns the
/// number copied, or `0` if the ring lock is momentarily contended.
#[cfg(target_os = "none")]
pub fn ring_copy_tail(out: &mut [u8]) -> usize {
    let ring = match RING.try_lock() {
        Some(r) => r,
        None => return 0,
    };
    let avail = ring.written.min(RING_SIZE as u64) as usize;
    let n = avail.min(out.len());
    let start = ring.written - n as u64;
    for (i, slot) in out[..n].iter_mut().enumerate() {
        *slot = ring.buf[((start + i as u64) % RING_SIZE as u64) as usize];
    }
    n
}

/// Host stub: no framebuffer ring to read back. See [`capture`].
#[cfg(not(target_os = "none"))]
pub fn ring_copy_tail(_out: &mut [u8]) -> usize {
    0
}

// ---------------------------------------------------------------------------
// Console control
// ---------------------------------------------------------------------------

/// The log is currently drawn on screen (renders + suppresses compositor flips).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set once the compositor presents its first frame — the boot phase is over,
/// so ESC stops toggling the log and is delivered to userland applications.
static DESKTOP_PRESENTED: AtomicBool = AtomicBool::new(false);
/// Set when the log is dismissed: the kernel painted the whole screen, so the
/// next compositor present must be full-screen (the compositor otherwise only
/// repaints damaged regions and would leave the stale log behind the desktop).
static FORCE_FULL_PRESENT: AtomicBool = AtomicBool::new(false);
/// Set on every ESC toggle so the renderer repaints on the next tick even if
/// presses interleaved between ticks (rapid spamming would otherwise leave the
/// renderer's skip-on-unchanged heuristic stuck, never repainting the log).
static RENDER_DIRTY: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU32 = AtomicU32::new(0);
static RENDER_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// ~100 Hz scheduler tick → cap the full-screen redraw at ~16 Hz.
const RENDER_EVERY_N_TICKS: u32 = 6;

pub type RenderHook = fn();

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Called by the compositor's first presented frame. After this, ESC belongs
/// to userland applications rather than the boot-log toggle.
pub fn notify_desktop_presented() {
    DESKTOP_PRESENTED.store(true, Ordering::Relaxed);
}

/// Consume the "force a full-screen present" request set when the log is
/// dismissed. The compositor flip path calls this to repaint the whole screen
/// once, erasing the dismissed log instead of doing an incremental update.
pub fn take_force_full_present() -> bool {
    FORCE_FULL_PRESENT.swap(false, Ordering::Relaxed)
}

/// Consume the "an ESC toggle happened, repaint now" request. The renderer
/// calls this so it never skips a frame after a toggle (however the presses
/// interleaved with ticks).
pub fn take_render_dirty() -> bool {
    RENDER_DIRTY.swap(false, Ordering::Relaxed)
}

/// Register the framebuffer renderer (implemented in the `video` crate).
pub fn register_renderer(hook: RenderHook) {
    RENDER_HOOK.store(hook as *mut (), Ordering::Release);
}

fn invoke_render() {
    let ptr = RENDER_HOOK.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    // SAFETY: `register_renderer` is the only writer of `RENDER_HOOK` and only
    // ever stores a valid `RenderHook` fn pointer (same size as `*mut ()` on
    // x86_64), mirroring the klog backend pattern.
    let hook: RenderHook = unsafe { core::mem::transmute(ptr) };
    hook();
}

/// Drive the renderer from the scheduler timer tick (call on CPU 0 only).
///
/// A single relaxed atomic load on the inactive fast path; throttled to
/// ~16 Hz when the log is shown.
pub fn on_timer_tick() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let n = TICKS.fetch_add(1, Ordering::Relaxed);
    if n % RENDER_EVERY_N_TICKS != 0 {
        return;
    }
    invoke_render();
}

/// Handle an ESC key-press seen in the keyboard IRQ.
///
/// During boot (before the desktop is up) ESC toggles the on-screen log and is
/// consumed (`true`). Once the compositor has presented a frame, ESC belongs to
/// userland applications and this is a no-op (`false`, so the key is delivered
/// normally) — unless the log is currently showing, in which case ESC still
/// dismisses it. Pure atomic flip; the redraw happens on the next timer tick,
/// so this is safe to call with the keyboard lock held.
pub fn handle_esc_press() -> bool {
    if DESKTOP_PRESENTED.load(Ordering::Relaxed) && !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let now_active = !ACTIVE.load(Ordering::Relaxed);
    ACTIVE.store(now_active, Ordering::Relaxed);
    // Repaint on the next tick regardless of how presses interleaved with ticks.
    RENDER_DIRTY.store(true, Ordering::Relaxed);
    if !now_active {
        // Dismissed: make the next compositor present full-screen so it paints
        // over the log instead of leaving it behind a damage-only update.
        FORCE_FULL_PRESENT.store(true, Ordering::Relaxed);
    }
    true
}
