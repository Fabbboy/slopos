//! Per-CPU klog capture rings.
//!
//! `begin()` swaps the global klog backend for one that routes each write to
//! `RINGS[current_cpu_id()]`, so a foreign CPU's klog during a test lands in
//! its own ring rather than contending with CPU0's.
//!
//! 8 KiB per ring across `MAX_CPUS = 256` costs 2 MiB of `.bss`.

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

pub struct CaptureGuard {
    prev: Option<KlogBackend>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = klog_swap_backend(self.prev);
    }
}

/// Reset every ring and install the buffering backend, so a test's log block
/// never carries a prior test's output.
pub fn begin() -> CaptureGuard {
    RINGS[0].reset();
    for i in 1..MAX_CPUS {
        RINGS[i].reset();
    }
    let prev = klog_swap_backend(Some(buffering_backend as KlogBackend));
    CaptureGuard { prev }
}

/// Read CPU0's ring under its lock. `f` must not klog into the same ring.
pub fn with_cpu0_log<R>(f: impl FnOnce(&[u8]) -> R) -> R {
    RINGS[0].with_bytes(f)
}

/// Read one CPU's ring under its lock.
pub fn with_log<R>(cpu: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    RINGS[cpu.min(MAX_CPUS - 1)].with_bytes(f)
}

/// The CPU the harness is running on. Userland thunks run in `/sbin/init`'s
/// syscall context on any CPU, so their klog lands in that CPU's ring.
pub fn current_cpu() -> usize {
    current_cpu_id().min(MAX_CPUS - 1)
}

/// CPUs whose ring holds bytes. Takes each ring's lock in turn, so callers
/// must not already hold one.
pub fn nonempty_cpus() -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(|&i| !RINGS[i].is_empty())
}

/// Bytes lost to CPU0's ring overflow during the last capture window.
pub fn truncated_bytes() -> usize {
    RINGS[0].dropped_bytes()
}
