//! Diagnostic-console commands that take the machine down.
//!
//! Both are `KCMD_DESTRUCTIVE`, so both are refused unless the policy mask
//! names that bit — which the default does not. They are registered anyway, so
//! the help listing shows them and marks them refused, rather than leaving an
//! operator to guess whether the key exists.
//!
//! `o` is the one that earns its keep. [`crate::shutdown::kernel_shutdown`]
//! flushes filesystem write-back caches with interrupts still enabled before
//! it halts, so a wedged compositor on a machine with no serial port can still
//! be brought down without losing what was written. A power button cannot do
//! that.
//!
//! They live here because `boot` owns the shutdown paths and nothing but the
//! kernel binary links `boot` — registration must happen in a crate userland
//! does not link.

use slopos_ostd::kconsole::{KCMD_DESTRUCTIVE, KConsole};
use slopos_ostd::kline;

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

fn run_poweroff(kc: &mut KConsole<'_>) {
    kline!(kc, "kconsole: poweroff requested from the console");
    crate::shutdown::kernel_shutdown(c"kconsole poweroff".as_ptr())
}

fn run_reboot(kc: &mut KConsole<'_>) {
    kline!(kc, "kconsole: reboot requested from the console");
    crate::shutdown::kernel_reboot(c"kconsole reboot".as_ptr())
}
