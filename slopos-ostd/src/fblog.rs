//! Framebuffer kernel-log console — capture ring + control state.
//!
//! Captures the **full serial byte stream** — kernel `klog` and userland TTY
//! output both funnel through [`crate::early_console::write_bytes`] — into an
//! in-memory ring, and drives an optional on-screen renderer registered by the
//! `video` crate.
//!
//! ESC toggles the log while the boot splash is up; once the compositor
//! presents its first frame, ESC belongs to applications again.
//!
//! The renderer is invoked from the scheduler timer tick, so it works even
//! when userland is wedged, and never from inside a log call. Capture and
//! read-back use `try_lock`, so this can never deadlock a panic, IRQ, or log
//! writer.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

#[cfg(target_os = "none")]
use crate::lock_class;
#[cfg(target_os = "none")]
use crate::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

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
/// Uses `try_lock`, so a contended ring — another CPU mid-append, or a panic
/// writer — drops this fragment rather than blocking.
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

/// Host builds have no framebuffer ring; capture is a no-op so the serial path
/// never takes the kernel `SpinLock`, whose gs-relative preempt asm the host
/// cannot execute.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogChar {
    Char(u8),
    Newline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    None,
    Introducer,
    Csi,
}

/// Drops the SGR sequences the ring carries from the console. The `[`
/// introducer is itself inside the CSI final-byte range, so ending a sequence
/// at the first byte in that range would print the parameters.
pub fn for_each_log_char(tail: &[u8], mut emit: impl FnMut(LogChar)) {
    let mut escape = EscapeState::None;
    for &byte in tail {
        match escape {
            EscapeState::None => {}
            EscapeState::Introducer => {
                escape = if byte == b'[' {
                    EscapeState::Csi
                } else {
                    EscapeState::None
                };
                continue;
            }
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    escape = EscapeState::None;
                }
                continue;
            }
        }
        match byte {
            0x1b => escape = EscapeState::Introducer,
            b'\n' => emit(LogChar::Newline),
            b'\r' => {}
            0x20..=0x7e => emit(LogChar::Char(byte)),
            _ => {}
        }
    }
}

#[cfg(target_os = "none")]
const PANIC_TAIL_BYTES: usize = 4096;

#[cfg(target_os = "none")]
struct PanicTail {
    buf: [u8; PANIC_TAIL_BYTES],
    len: usize,
}

#[cfg(target_os = "none")]
static PANIC_TAIL: SpinLock<PanicTail> = SpinLock::new(
    PanicTail {
        buf: [0u8; PANIC_TAIL_BYTES],
        len: 0,
    },
    lock_class!("fblog.PANIC_TAIL", LOCK_LEVEL_UNORDERED),
);

#[cfg(target_os = "none")]
static PANIC_TAIL_TAKEN: AtomicBool = AtomicBool::new(false);

/// Must run before the panic writes its own report, which shares this capture
/// path and would scroll out the lines that explain it. Both locks are
/// `try_lock`, so a panicking CPU cannot wedge here.
#[cfg(target_os = "none")]
pub fn snapshot_tail_for_panic() {
    if PANIC_TAIL_TAKEN.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(mut tail) = PANIC_TAIL.try_lock() else {
        return;
    };
    tail.len = ring_copy_tail(&mut tail.buf);
}

#[cfg(target_os = "none")]
pub fn with_panic_tail(f: impl FnOnce(&[u8])) {
    let Some(tail) = PANIC_TAIL.try_lock() else {
        return;
    };
    let len = tail.len;
    f(&tail.buf[..len]);
}

#[cfg(not(target_os = "none"))]
pub fn snapshot_tail_for_panic() {}

#[cfg(not(target_os = "none"))]
pub fn with_panic_tail(f: impl FnOnce(&[u8])) {
    f(&[]);
}

/// The log is currently drawn on screen (renders + suppresses compositor flips).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set once the compositor presents its first frame — the boot phase is over,
/// so ESC stops toggling the log and is delivered to userland applications.
static DESKTOP_PRESENTED: AtomicBool = AtomicBool::new(false);
/// Set when the log is dismissed: the kernel painted the whole screen, and the
/// compositor otherwise repaints only damaged regions, leaving the stale log
/// behind the desktop.
static FORCE_FULL_PRESENT: AtomicBool = AtomicBool::new(false);
/// Set on every ESC toggle so the renderer repaints on the next tick however
/// the presses interleaved with ticks, rather than sticking on its
/// skip-on-unchanged heuristic.
static RENDER_DIRTY: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU32 = AtomicU32::new(0);
static RENDER_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// ~100 Hz scheduler tick → cap the full-screen redraw at ~16 Hz.
const RENDER_EVERY_N_TICKS: u32 = 6;

pub type RenderHook = fn();

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Called by the compositor's first presented frame.
pub fn notify_desktop_presented() {
    DESKTOP_PRESENTED.store(true, Ordering::Relaxed);
}

/// Consume the "force a full-screen present" request, from the compositor flip
/// path.
pub fn take_force_full_present() -> bool {
    FORCE_FULL_PRESENT.swap(false, Ordering::Relaxed)
}

/// Consume the "an ESC toggle happened, repaint now" request, from the
/// renderer.
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
    // x86_64).
    let hook: RenderHook = unsafe { core::mem::transmute(ptr) };
    hook();
}

/// Drive the renderer from the scheduler timer tick (call on CPU 0 only).
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
/// Returns whether the key was consumed: during boot ESC toggles the on-screen
/// log, and once the compositor has presented a frame it belongs to userland
/// unless the log is still showing, which ESC dismisses.
///
/// Pure atomic flip — the redraw happens on the next timer tick — so this is
/// safe to call with the keyboard lock held.
pub fn handle_esc_press() -> bool {
    if DESKTOP_PRESENTED.load(Ordering::Relaxed) && !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let now_active = !ACTIVE.load(Ordering::Relaxed);
    ACTIVE.store(now_active, Ordering::Relaxed);
    RENDER_DIRTY.store(true, Ordering::Relaxed);
    if !now_active {
        FORCE_FULL_PRESENT.store(true, Ordering::Relaxed);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{LogChar, for_each_log_char};

    fn render(input: &[u8]) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for_each_log_char(input, |c| match c {
            LogChar::Char(b) => out.push(b as char),
            LogChar::Newline => out.push('\n'),
        });
        out
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(render(b"hello\nworld"), "hello\nworld");
    }

    #[test]
    fn carriage_returns_are_dropped() {
        assert_eq!(render(b"a\r\nb"), "a\nb");
    }

    #[test]
    fn sgr_sequences_are_consumed_whole() {
        assert_eq!(render(b"\x1b[38;2;198;120;221mSLOP\x1b[0m"), "SLOP");
    }

    #[test]
    fn two_byte_escapes_end_at_the_introducer() {
        assert_eq!(render(b"a\x1bMb"), "ab");
    }

    #[test]
    fn an_unterminated_sequence_swallows_the_rest() {
        assert_eq!(render(b"ok\x1b[38;2;1"), "ok");
    }

    #[test]
    fn control_bytes_are_dropped() {
        assert_eq!(render(b"a\x07\x00b"), "ab");
    }
}
