//! Safe wrappers around x86_64 segment / GDTR / TR register reads.
//!
//! `boot/src/tests/gdt_tests.rs` reads CS / DS / ES / FS / GS / SS / TR
//! and the GDTR descriptor to verify boot-stage segment configuration.
//! Every read is an `unsafe { asm!(...) }` block today. These wrappers
//! fold each unsafe inline-asm site into a one-line `pub fn` here.
//!
//! All wrappers are read-only and side-effect-free:
//! - `nomem` — no memory operands (except GDTR which writes a 10-byte
//!   buffer; `read_gdtr` returns the parsed limit+base instead),
//! - `nostack` — does not touch the stack,
//! - `preserves_flags` — does not clobber `EFLAGS`.
//!
//! The CPU is assumed to be in long mode (true by the time any kernel
//! test runs).

//!
//! Miri behaviour: every `asm!` site is gated `#[cfg(not(miri))]`; the
//! `#[cfg(miri)]` stub returns zero so tests calling these compile but
//! observe deterministic placeholder values. Tests that depend on real
//! segment / GDTR state must additionally `#[cfg_attr(miri, ignore)]`.

#[cfg(not(miri))]
use core::arch::asm;

/// GDTR descriptor parsed from `sgdt`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GdtrDescriptor {
    pub limit: u16,
    pub base: u64,
}

/// Read the GDTR (Global Descriptor Table Register) via `sgdt`.
#[cfg(not(miri))]
#[inline]
pub fn read_gdtr() -> GdtrDescriptor {
    let mut buf: [u8; 10] = [0; 10];
    // SAFETY: `sgdt` writes a 10-byte limit+base pair into the operand.
    // The buffer is stack-allocated and exclusively owned.
    unsafe {
        asm!(
            "sgdt [{}]",
            in(reg) buf.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
    let limit = u16::from_le_bytes([buf[0], buf[1]]);
    let base = u64::from_le_bytes([
        buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
    ]);
    GdtrDescriptor { limit, base }
}

#[cfg(miri)]
#[inline]
pub fn read_gdtr() -> GdtrDescriptor {
    GdtrDescriptor { limit: 0, base: 0 }
}

#[cfg(not(miri))]
#[inline]
pub fn read_cs() -> u16 {
    let v: u16;
    // SAFETY: pure register-to-register move; no memory or flag effects.
    unsafe {
        asm!("mov {:x}, cs", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_cs() -> u16 {
    0
}

#[cfg(not(miri))]
#[inline]
pub fn read_ds() -> u16 {
    let v: u16;
    // SAFETY: see read_cs.
    unsafe {
        asm!("mov {:x}, ds", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_ds() -> u16 {
    0
}

#[cfg(not(miri))]
#[inline]
pub fn read_es() -> u16 {
    let v: u16;
    // SAFETY: see read_cs.
    unsafe {
        asm!("mov {:x}, es", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_es() -> u16 {
    0
}

#[cfg(not(miri))]
#[inline]
pub fn read_fs() -> u16 {
    let v: u16;
    // SAFETY: see read_cs.
    unsafe {
        asm!("mov {:x}, fs", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_fs() -> u16 {
    0
}

#[cfg(not(miri))]
#[inline]
pub fn read_gs() -> u16 {
    let v: u16;
    // SAFETY: see read_cs.
    unsafe {
        asm!("mov {:x}, gs", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_gs() -> u16 {
    0
}

#[cfg(not(miri))]
#[inline]
pub fn read_ss() -> u16 {
    let v: u16;
    // SAFETY: see read_cs.
    unsafe {
        asm!("mov {:x}, ss", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_ss() -> u16 {
    0
}

/// Read the Task Register (TR) via `str`.
#[cfg(not(miri))]
#[inline]
pub fn read_tr() -> u16 {
    let v: u16;
    // SAFETY: `str` reads the visible portion of the task register; no
    // memory or flag side effects.
    unsafe {
        asm!("str {:x}", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(miri)]
#[inline]
pub fn read_tr() -> u16 {
    0
}
