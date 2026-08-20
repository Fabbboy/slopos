//! Kernel logging subsystem.
//!
//! All kernel log output funnels through a single **backend** function pointer.
//! Before the serial driver is ready the backend writes directly to COM1 via
//! [`crate::early_console`]; the driver then registers itself as the backend.

use crate::lock_class;
use core::ffi::c_int;
use core::fmt;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

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

/// Signature of a klog backend.
///
/// The backend must write the formatted text **and** a trailing newline under a
/// single lock acquisition, so lines from different CPUs do not interleave.
pub type KlogBackend = fn(fmt::Arguments<'_>);

/// Stored as a raw pointer; `null` means "use early-boot fallback".
static BACKEND: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Monotonic-clock source for [`RateLimit`], registered once the timebase is
/// up. `null` before then, which makes every window the first one.
static CLOCK_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the monotonic-nanosecond source the rate limiter windows on.
pub fn klog_register_clock(hook: fn() -> u64) {
    CLOCK_HOOK.store(hook as *mut (), Ordering::Release);
}

fn monotonic_ns_hook() -> Option<u64> {
    let ptr = CLOCK_HOOK.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `klog_register_clock` only stores a `fn() -> u64`, which is the
    // same size as `*mut ()` on x86_64.
    let hook: fn() -> u64 = unsafe { core::mem::transmute(ptr) };
    Some(hook())
}

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
    // A dropped line is preferable to a stall in a path that runs from IRQ
    // context.
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

/// Logical bytes the ring currently holds — the offset to pass [`klog_read`]
/// to mean "from here on".
pub fn klog_len() -> usize {
    KLOG_RING.lock().len
}

/// Register a backend that replaces the early-boot COM1 fallback.
pub fn klog_register_backend(backend: KlogBackend) {
    let _ = klog_swap_backend(Some(backend));
}

/// Atomically install `new` and return whatever was active before; `None` is
/// the early-boot null-pointer fallback.
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

/// Force the backend back to the early-boot fallback: a `CaptureGuard` whose
/// frame is destroyed by something other than an ordinary return leaves the
/// buffering backend installed, and nothing else would take it out.
pub fn klog_force_restore_default() {
    BACKEND.store(core::ptr::null_mut(), Ordering::Release);
}

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

/// Emit one line regardless of the current level, for output an operator asked
/// for directly.
///
/// Raising [`CURRENT_LEVEL`] around the caller instead would change every other
/// CPU's logging and race a concurrent [`klog_set_level`]; emitting at `Error`
/// would poison error triage.
pub fn log_forced(args: fmt::Arguments<'_>) {
    ring_capture(args);
    dispatch(args);
}

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

/// Per-call-site token bucket.
///
/// A log line is written byte-at-a-time through a polled UART while a cli-held
/// lock is held, so a userland-reachable site with no limiter lets one process
/// stall every CPU that touches the log for as long as it loops.
pub struct RateLimit {
    /// Start of the current window, in ms; `0` before the first emit.
    window_start_ms: AtomicU64,
    emitted: AtomicU32,
    suppressed: AtomicU32,
}

impl RateLimit {
    pub const fn new() -> Self {
        Self {
            window_start_ms: AtomicU64::new(0),
            emitted: AtomicU32::new(0),
            suppressed: AtomicU32::new(0),
        }
    }

    /// Whether this site may emit now, and how many lines it swallowed since
    /// it last did.
    pub fn allow(&self, now_ms: u64, window_ms: u64, burst: u32) -> Option<u32> {
        let start = self.window_start_ms.load(Ordering::Relaxed);
        if start == 0 || now_ms.wrapping_sub(start) >= window_ms {
            self.window_start_ms.store(now_ms.max(1), Ordering::Relaxed);
            self.emitted.store(1, Ordering::Relaxed);
            return Some(self.suppressed.swap(0, Ordering::Relaxed));
        }
        if self.emitted.fetch_add(1, Ordering::Relaxed) < burst {
            return Some(self.suppressed.swap(0, Ordering::Relaxed));
        }
        self.suppressed.fetch_add(1, Ordering::Relaxed);
        None
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::new()
    }
}

/// Monotonic milliseconds for the limiter, or 0 before the clock is up.
pub fn ratelimit_now_ms() -> u64 {
    monotonic_ns_hook().map(|ns| ns / 1_000_000).unwrap_or(0)
}

/// Emit at most `burst` lines per `window_ms` from this call site, reporting
/// the suppressed count when it resumes.
///
/// The state is a per-site `static`: no allocation, no lock, and a site that
/// is never hit costs nothing.
#[macro_export]
macro_rules! klog_ratelimited {
    ($level:expr, $window_ms:expr, $burst:expr, $($arg:tt)*) => {{
        static __SLOPOS_RATE_LIMIT: $crate::klog::RateLimit = $crate::klog::RateLimit::new();
        let __now = $crate::klog::ratelimit_now_ms();
        if let Some(__suppressed) = __SLOPOS_RATE_LIMIT.allow(__now, $window_ms, $burst) {
            if __suppressed > 0 {
                $crate::klog::log_args(
                    $level,
                    ::core::format_args!(
                        "[{} suppressed by rate limit]",
                        __suppressed
                    ),
                );
            }
            $crate::klog::log_args($level, ::core::format_args!($($arg)*));
        }
    }};
}

/// Ten lines a second from one site, the default for a userland-reachable
/// message.
#[macro_export]
macro_rules! klog_warn_ratelimited {
    ($($arg:tt)*) => {
        $crate::klog_ratelimited!($crate::klog::KlogLevel::Warn, 1000, 10, $($arg)*)
    };
}
