#![allow(bad_asm_style)]

/// Kernel entry points exposed to the Limine assembly trampoline.
pub type BootEntry = extern "C" fn();

pub const BOOT_ENTRY: BootEntry = slopos_boot::kernel_main;
