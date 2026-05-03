//! SSE/FPU initialization.

use super::control_regs::{Cr0Flags, Cr4Flags, read_cr0, read_cr4, write_cr0, write_cr4};

/// Enable SSE instructions by configuring CR0 and CR4.
pub fn enable_sse() {
    let mut cr0 = read_cr0();
    cr0 &= !Cr0Flags::EM.bits();
    cr0 &= !Cr0Flags::TS.bits();
    cr0 |= Cr0Flags::MP.bits();
    write_cr0(cr0);

    let mut cr4 = read_cr4();
    cr4 |= Cr4Flags::OSFXSR.bits();
    cr4 |= Cr4Flags::OSXMMEXCPT.bits();
    write_cr4(cr4);
}
