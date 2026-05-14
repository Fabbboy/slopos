//! Per-CPU klog capture rings.
//!
//! `begin()` resets CPU0's ring and swaps the global klog backend for a
//! buffering one that routes each write to `RINGS[current_cpu_id()]`. The
//! `CaptureGuard` returned restores the prior backend on drop. Foreign-CPU
//! klog writes during a test land in their own per-CPU ring without
//! contending with CPU0's writes.
//!
//! Sizing: `PER_CPU_RING_BYTES = 8 KiB`, MAX_CPUS = 256 (`karch::pcr`),
//! total 2 MiB of `.bss`. Plan §4 specified 64 KiB per CPU at MAX_CPUS=8
//! (≈512 KiB); shrinking the per-CPU bucket lets us preserve per-CPU
//! semantics at the kernel's real `MAX_CPUS=256` without ballooning to
//! 16 MiB. CPU0 (where the harness loop runs) is the typical heavy
//! writer — 8 KiB holds dozens of typical klog lines.

use core::cell::SyncUnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_arch::pcr::{current_cpu_id, MAX_CPUS};
use slopos_utils::{klog_swap_backend, KlogBackend};

const PER_CPU_RING_BYTES: usize = 8 * 1024;

struct RingSlot {
    buf: SyncUnsafeCell<[u8; PER_CPU_RING_BYTES]>,
    len: AtomicUsize,
    truncated: AtomicBool,
    dropped: AtomicUsize,
    lock: AtomicBool,
}

impl RingSlot {
    const fn new() -> Self {
        Self {
            buf: SyncUnsafeCell::new([0u8; PER_CPU_RING_BYTES]),
            len: AtomicUsize::new(0),
            truncated: AtomicBool::new(false),
            dropped: AtomicUsize::new(0),
            lock: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        // Acquire lock to make the reset atomic w.r.t. concurrent writers.
        while self.lock.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        self.len.store(0, Ordering::Relaxed);
        self.truncated.store(false, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.lock.store(false, Ordering::Release);
    }

    fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        while self.lock.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        let len = self.len.load(Ordering::Relaxed);
        let remaining = PER_CPU_RING_BYTES.saturating_sub(len);
        let take = bytes.len().min(remaining);
        if take > 0 {
            // SAFETY: `len + take <= PER_CPU_RING_BYTES`; the `lock` guard
            // serialises writers on this slot.
            unsafe {
                let dst = (self.buf.get() as *mut u8).add(len);
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, take);
            }
            self.len.store(len + take, Ordering::Relaxed);
        }
        let dropped = bytes.len() - take;
        if dropped > 0 {
            self.truncated.store(true, Ordering::Relaxed);
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        self.lock.store(false, Ordering::Release);
    }

    fn slice(&self) -> &[u8] {
        let len = self.len.load(Ordering::Acquire).min(PER_CPU_RING_BYTES);
        // Bytes [0..len) were written under `lock` before `len` was
        // updated; reading via Acquire pairs with the Release in `append`.
        slopos_ostd::util::ptr_buf::borrow_buf(self.buf.get() as *const u8, len)
    }

    fn dropped_bytes(&self) -> usize {
        if self.truncated.load(Ordering::Relaxed) {
            self.dropped.load(Ordering::Relaxed)
        } else {
            0
        }
    }
}

static RINGS: [RingSlot; MAX_CPUS] = [const { RingSlot::new() }; MAX_CPUS];

struct RingWriter {
    cpu: usize,
}

impl fmt::Write for RingWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        RINGS[self.cpu].append(s.as_bytes());
        Ok(())
    }
}

fn buffering_backend(args: fmt::Arguments<'_>) {
    let cpu = current_cpu_id().min(MAX_CPUS - 1);
    let mut writer = RingWriter { cpu };
    let _ = fmt::write(&mut writer, args);
    // klog contract: backend appends a trailing newline.
    RINGS[cpu].append(b"\n");
}

/// RAII handle that restores the prior klog backend on drop.
pub struct CaptureGuard {
    prev: Option<KlogBackend>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = klog_swap_backend(self.prev);
    }
}

/// Reset CPU0's ring (where the harness writes) and install the buffering
/// backend. Foreign-CPU rings are NOT reset — verbose-mode `drain_all`
/// emits them as accumulated context for the running test.
pub fn begin() -> CaptureGuard {
    RINGS[0].reset();
    // Reset foreign-CPU rings too: per-test logs should not include a
    // prior test's foreign-CPU output.
    for i in 1..MAX_CPUS {
        RINGS[i].reset();
    }
    let prev = klog_swap_backend(Some(buffering_backend as KlogBackend));
    CaptureGuard { prev }
}

/// Bytes captured into CPU0's ring since the last `begin`. CPU0 runs the
/// harness, so this is the primary log stream for the current test.
pub fn drain_cpu0() -> &'static [u8] {
    RINGS[0].slice()
}

/// Bytes captured into the current CPU's ring since the last `begin`.
///
/// Used by callers that may run on a non-BSP CPU — notably the userland
/// test runner, which is invoked from `/sbin/init`'s syscall context and
/// can therefore execute on any CPU. Pairs with `drain_cpu0` for kernel
/// tests, which always run on CPU 0 via the `SchedFixture` BSP-parking.
pub fn drain_current_cpu() -> &'static [u8] {
    let cpu = current_cpu_id().min(MAX_CPUS - 1);
    RINGS[cpu].slice()
}

/// Iterate every per-CPU ring that has bytes in it. Used by the verbose
/// emit path to surface foreign-CPU klog (e.g., from interrupts that
/// fired during a test).
pub fn drain_all() -> impl Iterator<Item = (usize, &'static [u8])> {
    (0..MAX_CPUS).filter_map(|i| {
        let slice = RINGS[i].slice();
        if slice.is_empty() {
            None
        } else {
            Some((i, slice))
        }
    })
}

/// Number of bytes that overflowed CPU0's ring during the last capture
/// window.
pub fn truncated_bytes() -> usize {
    RINGS[0].dropped_bytes()
}
