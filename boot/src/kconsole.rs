//! Diagnostic-console commands that take the machine down. They live in `boot`
//! because registration must happen in a crate userland does not link.

use slopos_ostd::kconsole::{KCMD_DESTRUCTIVE, KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;

slopos_ostd::kcommand! {
    name = blit,
    key = b'f',
    help = "per-CPU framebuffer blit throughput (WC vs WT evidence)",
    flags = KCMD_INFORMATIONAL,
    run = run_blit,
}

/// Bytes per 1000 TSC cycles rather than MB/s: no TSC frequency is known here,
/// and the cross-CPU ratio is what matters.
fn run_blit(kc: &mut KConsole<'_>) {
    let mut seen = 0u32;
    let mut best = 0u64;
    let mut worst = u64::MAX;
    for cpu in 0..slopos_ostd::cpu::x86_64::pcr::get_cpu_count() {
        let Some(s) = slopos_video::blit_census::stats(cpu) else {
            continue;
        };
        seen += 1;
        let rate = if s.cycles == 0 {
            0
        } else {
            s.bytes.saturating_mul(1000) / s.cycles
        };
        if rate > best {
            best = rate;
        }
        if rate < worst {
            worst = rate;
        }
        kline!(
            kc,
            "  cpu {:>2}: {} frames, {} KiB, {} cycles, {} B/1k-cycles",
            cpu,
            s.frames,
            s.bytes / 1024,
            s.cycles,
            rate
        );
    }
    if seen == 0 {
        kline!(
            kc,
            "  no blits recorded yet -- the compositor has not presented"
        );
        return;
    }
    if seen == 1 {
        kline!(
            kc,
            "only one CPU has blitted; no cross-CPU comparison available"
        );
        return;
    }
    kline!(
        kc,
        "spread: fastest {} vs slowest {} B/1k-cycles ({}x)",
        best,
        worst,
        if worst == 0 { 0 } else { best / worst }
    );
}

slopos_ostd::kcommand! {
    name = poweroff,
    key = b'o',
    help = "clean shutdown: flush filesystems, then ACPI S5",
    flags = KCMD_DESTRUCTIVE,
    run = run_poweroff,
}

slopos_ostd::kcommand! {
    name = reboot,
    key = b'b',
    help = "reboot via UEFI, then ACPI, then CF9, then triple fault",
    flags = KCMD_DESTRUCTIVE,
    run = run_reboot,
}

// Kernel-initiated: the trigger is a key on the physical console, which no
// userland process can forge and which carries no credential to check. The
// authority is `being the kernel`, minted through the tracked seam rather than
// derived from a caller.

fn run_poweroff(kc: &mut KConsole<'_>) {
    kline!(kc, "kconsole: poweroff requested from the console");
    let cap = slopos_ostd::platform::power::kernel_authority();
    slopos_ostd::platform::power::shutdown(&cap, c"kconsole poweroff".as_ptr())
}

fn run_reboot(kc: &mut KConsole<'_>) {
    kline!(kc, "kconsole: reboot requested from the console");
    let cap = slopos_ostd::platform::power::kernel_authority();
    slopos_ostd::platform::power::reboot(&cap, c"kconsole reboot".as_ptr())
}
