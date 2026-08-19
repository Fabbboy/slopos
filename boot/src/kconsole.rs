//! Diagnostic-console commands that take the machine down. They live in `boot`
//! because registration must happen in a crate userland does not link.

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
