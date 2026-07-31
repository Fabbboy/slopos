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

use core::fmt;

use slopos_arch::pcr::{current_cpu_id, MAX_CPUS};
use slopos_ostd::sync::append_log::AppendLog;
use slopos_ostd::{klog_swap_backend, KlogBackend};

const PER_CPU_RING_BYTES: usize = 8 * 1024;

type RingSlot = AppendLog<PER_CPU_RING_BYTES>;

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
/// backend. Every ring is reset, so a test's log block never carries a
/// prior test's foreign-CPU output.
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

/// Read CPU0's ring — the harness loop's own stream — under its lock.
///
/// `f` must not klog into the same ring; callers run it after the
/// [`CaptureGuard`] has restored the previous backend, so nothing appends.
pub fn with_cpu0_log<R>(f: impl FnOnce(&[u8]) -> R) -> R {
    RINGS[0].with_bytes(f)
}

/// Read one CPU's ring under its lock.
pub fn with_log<R>(cpu: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    RINGS[cpu.min(MAX_CPUS - 1)].with_bytes(f)
}

/// The CPU the harness is currently running on.
///
/// Userland-test thunks run from `/sbin/init`'s syscall context and can
/// execute on any CPU, so their klog lands in that CPU's ring rather than
/// CPU 0's.
pub fn current_cpu() -> usize {
    current_cpu_id().min(MAX_CPUS - 1)
}

/// CPUs whose ring holds bytes. Takes each ring's lock in turn, so callers
/// must not already hold one.
pub fn nonempty_cpus() -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(|&i| !RINGS[i].is_empty())
}

/// Number of bytes that overflowed CPU0's ring during the last capture
/// window.
pub fn truncated_bytes() -> usize {
    RINGS[0].dropped_bytes()
}
