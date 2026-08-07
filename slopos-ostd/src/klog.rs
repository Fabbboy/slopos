//! Kernel logging subsystem.
//!
//! All kernel log output funnels through a single **backend** function pointer.
//! During early boot (before the serial driver is ready) the backend writes
//! directly to COM1 via [`crate::early_console`].  Once the serial driver
//! initialises it registers itself as the backend, and all subsequent output
//! goes through the driver's `SpinLock`-protected path — giving us proper
//! locking, FIFO awareness, and `\n → \r\n` conversion for free.
//!
//! # Backend contract
//!
//! The backend receives the pre-formatted arguments for a **single log line**
//! and is responsible for:
//!
//! 1. Writing the formatted text **atomically** (no interleaving from other
//!    CPUs).
//! 2. Appending a trailing newline after the text.
//!
//! The early-boot fallback satisfies (1) trivially (single-threaded boot) and
//! handles (2) by emitting `\n` (which `early_console` expands to `\r\n`).
//!
//! # Registration
//!
//! ```ignore
//! // In your serial driver init:
//! slopos_ostd::klog::klog_register_backend(my_backend_fn);
//! ```

use crate::lock_class;
use core::ffi::c_int;
use core::fmt;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

use crate::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

// ---------------------------------------------------------------------------
// Log levels
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KlogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl KlogLevel {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => KlogLevel::Error,
            1 => KlogLevel::Warn,
            2 => KlogLevel::Info,
            3 => KlogLevel::Debug,
            _ => KlogLevel::Trace,
        }
    }
}

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(KlogLevel::Info as u8);

#[inline(always)]
fn is_enabled(level: KlogLevel) -> bool {
    level as u8 <= CURRENT_LEVEL.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Backend dispatch
// ---------------------------------------------------------------------------

/// Signature of a klog backend.
///
/// The backend must write the formatted text **and** a trailing newline,
/// all under a single lock acquisition (if applicable) so that log lines
/// from different CPUs do not interleave.
pub type KlogBackend = fn(fmt::Arguments<'_>);

/// Stored as a raw pointer; `null` means "use early-boot fallback".
static BACKEND: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

fn early_backend(args: fmt::Arguments<'_>) {
    struct EarlyWriter;

    impl fmt::Write for EarlyWriter {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            crate::early_console::write_bytes(s.as_bytes());
            Ok(())
        }
    }

    let _ = fmt::write(&mut EarlyWriter, args);
    crate::early_console::write_bytes(b"\n");
}

/// Dispatch a log line through the active backend.
///
/// If no backend has been registered yet the early-boot fallback is used.
#[inline]
fn dispatch(args: fmt::Arguments<'_>) {
    let ptr = BACKEND.load(Ordering::Acquire);
    if ptr.is_null() {
        early_backend(args);
    } else {
        // SAFETY: `klog_register_backend` only stores valid `KlogBackend` fn
        // pointers, which are the same size as `*mut ()` on all supported
        // targets (x86_64).
        let backend: KlogBackend = unsafe { core::mem::transmute(ptr) };
        backend(args);
    }
}

// ---------------------------------------------------------------------------
// In-memory log ring buffer (userland-readable via /dev/kmsg)
//
// Every emitted log line is also captured here so it can be read back from
// userland, which is the only log sink available with no serial console.
//
// The ring engages only once a real backend is registered (after the serial
// driver and the per-CPU record are up), so it never touches a `SpinLock`
// during early single-threaded boot. Writers use `try_lock` and skip on
// contention — a dropped line is preferable to a stall in a path that runs
// from IRQ context.
// ---------------------------------------------------------------------------

// Sized so a full boot log plus steady-state logging fits without wrapping:
// `/dev/kmsg` reads the ring by byte offset, so a wrap during a `cat` shifts
// the window under the reader and garbles the stream.
const KLOG_RING_SIZE: usize = 256 * 1024;

struct KlogRing {
    buf: [u8; KLOG_RING_SIZE],
    /// Next write index.
    head: usize,
    /// Number of valid bytes (saturates at `KLOG_RING_SIZE`).
    len: usize,
}

impl KlogRing {
    const fn new() -> Self {
        Self {
            buf: [0; KLOG_RING_SIZE],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, b: u8) {
        self.buf[self.head] = b;
        self.head = (self.head + 1) % KLOG_RING_SIZE;
        if self.len < KLOG_RING_SIZE {
            self.len += 1;
        }
    }

    /// Copy logical bytes `[offset ..]` (offset 0 = oldest retained byte)
    /// into `out`, returning the count. `0` means end-of-log.
    fn read_at(&self, offset: usize, out: &mut [u8]) -> usize {
        if offset >= self.len {
            return 0;
        }
        // Oldest logical byte sits at index 0 until the ring wraps, then at
        // `head`.
        let start = if self.len < KLOG_RING_SIZE {
            0
        } else {
            self.head
        };
        let n = (self.len - offset).min(out.len());
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.buf[(start + offset + i) % KLOG_RING_SIZE];
        }
        n
    }
}

static KLOG_RING: SpinLock<KlogRing> = SpinLock::new(
    KlogRing::new(),
    lock_class!("KLOG_RING", LOCK_LEVEL_UNORDERED),
);

struct RingWriter<'a>(&'a mut KlogRing);

impl fmt::Write for RingWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.0.push(b);
        }
        Ok(())
    }
}

/// Capture one log line into the ring (text + trailing newline). No-op
/// until a backend is registered (keeps early boot lock-free).
fn ring_capture(args: fmt::Arguments<'_>) {
    if BACKEND.load(Ordering::Acquire).is_null() {
        return;
    }
    if let Some(mut ring) = KLOG_RING.try_lock() {
        let _ = fmt::write(&mut RingWriter(&mut ring), args);
        ring.push(b'\n');
    }
}

/// Read buffered kernel log bytes starting at logical `offset` into `out`,
/// returning the number copied (`0` = end-of-log). Backs `/dev/kmsg`.
pub fn klog_read(offset: usize, out: &mut [u8]) -> usize {
    KLOG_RING.lock().read_at(offset, out)
}

/// Logical bytes the ring currently holds.
///
/// The offset a reader passes to [`klog_read`] to mean "from here on", so a
/// caller can bracket a window of output rather than re-reading the whole ring.
pub fn klog_len() -> usize {
    KLOG_RING.lock().len
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register a backend that replaces the early-boot COM1 fallback.
///
/// Typically called once by the serial driver during its initialisation.
pub fn klog_register_backend(backend: KlogBackend) {
    let _ = klog_swap_backend(Some(backend));
}

/// Atomically install `new` and return whatever was active before.
///
/// `None` represents the early-boot null-pointer fallback. The test harness
/// uses this to stash the prior backend, install a buffering capture backend
/// for the duration of one test, and restore the original on drop.
pub fn klog_swap_backend(new: Option<KlogBackend>) -> Option<KlogBackend> {
    let new_ptr = match new {
        Some(b) => b as *mut (),
        None => core::ptr::null_mut(),
    };
    let prev = BACKEND.swap(new_ptr, Ordering::AcqRel);
    if prev.is_null() {
        None
    } else {
        // SAFETY: `BACKEND` only ever holds pointers we put there, which are
        // valid `KlogBackend` fn pointers (same size as `*mut ()` on x86_64).
        Some(unsafe { core::mem::transmute::<*mut (), KlogBackend>(prev) })
    }
}

/// Force the backend back to the early-boot fallback.
///
/// Intended for the panic-recovery cleanup path: a `CaptureGuard` whose
/// frame is destroyed by something other than an ordinary return leaves the
/// buffering backend installed, and nothing else would take it out.
pub fn klog_force_restore_default() {
    BACKEND.store(core::ptr::null_mut(), Ordering::Release);
}

/// Initialise klog (sets default level).  Called very early in boot.
pub fn klog_init() {
    CURRENT_LEVEL.store(KlogLevel::Info as u8, Ordering::Relaxed);
}

pub fn klog_set_level(level: KlogLevel) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn klog_get_level() -> KlogLevel {
    KlogLevel::from_raw(CURRENT_LEVEL.load(Ordering::Relaxed))
}

pub fn klog_is_enabled(level: KlogLevel) -> c_int {
    if is_enabled(level) { 1 } else { 0 }
}

pub fn is_enabled_level(level: KlogLevel) -> bool {
    is_enabled(level)
}

/// Emit a formatted log line at the given level.
///
/// The backend appends a trailing newline — callers should **not** include
/// one in their format string.
pub fn log_args(level: KlogLevel, args: fmt::Arguments<'_>) {
    if !is_enabled(level) {
        return;
    }
    ring_capture(args);
    dispatch(args);
}

/// Emit one line regardless of the current level.
///
/// For output an operator asked for directly, where the level filter is not
/// the right question: a diagnostic dump that prints nothing because the boot
/// left the level at `Error` is indistinguishable from a broken one.
///
/// Raising [`CURRENT_LEVEL`] around the caller would be the other way to do
/// it, and is wrong twice over: it is a global, so it changes every other
/// CPU's logging for the duration, and restoring it races a concurrent
/// [`klog_set_level`]. Emitting at `Error` instead would be wrong once — a
/// dump is not an error, and counting it as one poisons error triage.
pub fn log_forced(args: fmt::Arguments<'_>) {
    ring_capture(args);
    dispatch(args);
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

#[macro_export]
macro_rules! klog {
    ($level:expr, $($arg:tt)*) => {{
        $crate::klog::log_args($level, ::core::format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! klog_error {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Error, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Warn, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Info, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_debug {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Debug, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_trace {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Trace, ::core::format_args!($($arg)*))
    };
}
