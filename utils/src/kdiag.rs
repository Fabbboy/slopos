use core::arch::asm;
use core::ffi::c_int;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::stacktrace::{self, StacktraceEntry};
use slopos_arch::cpu;
use slopos_arch::tsc;

// ---------------------------------------------------------------------------
// Register snapshot — only used by kdiag_dump_cpu_state() below.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RegSnapshot {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

#[inline(never)]
fn snapshot_regs() -> RegSnapshot {
    let (rax, rbx, rcx, rdx): (u64, u64, u64, u64);
    let (rsi, rdi, rbp, rsp): (u64, u64, u64, u64);
    let (r8, r9, r10, r11): (u64, u64, u64, u64);
    let (r12, r13, r14, r15): (u64, u64, u64, u64);
    unsafe {
        asm!(
            "mov {0}, rax",
            "mov {1}, rbx",
            "mov {2}, rcx",
            "mov {3}, rdx",
            out(reg) rax,
            out(reg) rbx,
            out(reg) rcx,
            out(reg) rdx,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "mov {0}, rsi",
            "mov {1}, rdi",
            "mov {2}, rbp",
            "mov {3}, rsp",
            out(reg) rsi,
            out(reg) rdi,
            out(reg) rbp,
            out(reg) rsp,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "mov {0}, r8",
            "mov {1}, r9",
            "mov {2}, r10",
            "mov {3}, r11",
            out(reg) r8,
            out(reg) r9,
            out(reg) r10,
            out(reg) r11,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "mov {0}, r12",
            "mov {1}, r13",
            "mov {2}, r14",
            "mov {3}, r15",
            out(reg) r12,
            out(reg) r13,
            out(reg) r14,
            out(reg) r15,
            options(nomem, nostack, preserves_flags)
        );
    }
    RegSnapshot {
        rax,
        rbx,
        rcx,
        rdx,
        rsi,
        rdi,
        rbp,
        rsp,
        r8,
        r9,
        r10,
        r11,
        r12,
        r13,
        r14,
        r15,
    }
}

pub const KDIAG_STACK_TRACE_DEPTH: usize = 16;

pub use slopos_arch::InterruptFrame;

fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "Divide Error",
        1 => "Debug",
        2 => "NMI",
        3 => "Breakpoint",
        4 => "Overflow",
        5 => "Bound Range",
        6 => "Invalid Opcode",
        7 => "Device Not Available",
        8 => "Double Fault",
        10 => "Invalid TSS",
        11 => "Segment Not Present",
        12 => "Stack Fault",
        13 => "General Protection",
        14 => "Page Fault",
        16 => "FPU Error",
        17 => "Alignment Check",
        18 => "Machine Check",
        19 => "SIMD FP Exception",
        _ => "Unknown",
    }
}

static MONOTONIC_TIME: AtomicU64 = AtomicU64::new(0);
static LAST_TSC: AtomicU64 = AtomicU64::new(0);
pub fn kdiag_timestamp() -> u64 {
    let tsc = tsc::rdtsc();
    let last = LAST_TSC.load(Ordering::Relaxed);
    if tsc > last {
        let delta = tsc - last;
        MONOTONIC_TIME.fetch_add(delta, Ordering::Relaxed);
        LAST_TSC.store(tsc, Ordering::Relaxed);
    }
    MONOTONIC_TIME.load(Ordering::Relaxed)
}
pub fn kdiag_dump_cpu_state() {
    let regs = snapshot_regs();
    let rflags = cpu::read_rflags();
    let cr0 = cpu::read_cr0();
    let cr2 = cpu::read_cr2();
    let cr3 = cpu::read_cr3();
    let cr4 = cpu::read_cr4();

    let (cs, ds, es, fs, gs, ss): (u16, u16, u16, u16, u16, u16);
    unsafe {
        core::arch::asm!("mov {0:x}, cs", out(reg) cs);
        core::arch::asm!("mov {0:x}, ds", out(reg) ds);
        core::arch::asm!("mov {0:x}, es", out(reg) es);
        core::arch::asm!("mov {0:x}, fs", out(reg) fs);
        core::arch::asm!("mov {0:x}, gs", out(reg) gs);
        core::arch::asm!("mov {0:x}, ss", out(reg) ss);
    }

    crate::klog_info!("=== CPU STATE DUMP ===");
    crate::klog_info!(
        "General Purpose Registers:\n  RAX: 0x{:x}  RBX: 0x{:x}  RCX: 0x{:x}  RDX: 0x{:x}\n  RSI: 0x{:x}  RDI: 0x{:x}  RBP: 0x{:x}  RSP: 0x{:x}\n  R8 : 0x{:x}  R9 : 0x{:x}  R10: 0x{:x}  R11: 0x{:x}\n  R12: 0x{:x}  R13: 0x{:x}  R14: 0x{:x}  R15: 0x{:x}",
        regs.rax,
        regs.rbx,
        regs.rcx,
        regs.rdx,
        regs.rsi,
        regs.rdi,
        regs.rbp,
        regs.rsp,
        regs.r8,
        regs.r9,
        regs.r10,
        regs.r11,
        regs.r12,
        regs.r13,
        regs.r14,
        regs.r15
    );
    crate::klog_info!(
        "Flags Register:\n  RFLAGS: 0x{:x} [CF:{} PF:{} AF:{} ZF:{} SF:{} TF:{} IF:{} DF:{} OF:{}]",
        rflags,
        ((rflags & (1 << 0)) != 0) as i32,
        ((rflags & (1 << 2)) != 0) as i32,
        ((rflags & (1 << 4)) != 0) as i32,
        ((rflags & (1 << 6)) != 0) as i32,
        ((rflags & (1 << 7)) != 0) as i32,
        ((rflags & (1 << 8)) != 0) as i32,
        ((rflags & (1 << 9)) != 0) as i32,
        ((rflags & (1 << 10)) != 0) as i32,
        ((rflags & (1 << 11)) != 0) as i32,
    );
    crate::klog_info!(
        "Segment Registers:\n  CS: 0x{:04x}  DS: 0x{:04x}  ES: 0x{:04x}  FS: 0x{:04x}  GS: 0x{:04x}  SS: 0x{:04x}",
        cs,
        ds,
        es,
        fs,
        gs,
        ss
    );
    crate::klog_info!(
        "Control Registers:\n  CR0: 0x{:x}  CR2: 0x{:x}\n  CR3: 0x{:x}  CR4: 0x{:x}",
        cr0,
        cr2,
        cr3,
        cr4
    );
    crate::klog_info!("=== END CPU STATE DUMP ===");
}
pub fn kdiag_dump_interrupt_frame(frame: *const InterruptFrame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = &*frame;
        let exc_name = exception_name(f.vector as u8);
        crate::klog_info!("=== INTERRUPT FRAME DUMP ===");
        crate::klog_info!(
            "Vector: {} ({}) Error Code: 0x{:x}",
            f.vector,
            exc_name,
            f.error_code
        );
        crate::klog_info!(
            "RIP: 0x{:x}  CS: 0x{:x}  RFLAGS: 0x{:x}",
            f.rip,
            f.cs,
            f.rflags
        );
        crate::klog_info!("RSP: 0x{:x}  SS: 0x{:x}", f.rsp, f.ss);
        crate::klog_info!("RAX: 0x{:x}  RBX: 0x{:x}  RCX: 0x{:x}", f.rax, f.rbx, f.rcx);
        crate::klog_info!("RDX: 0x{:x}  RSI: 0x{:x}  RDI: 0x{:x}", f.rdx, f.rsi, f.rdi);
        crate::klog_info!("RBP: 0x{:x}  R8: 0x{:x}  R9: 0x{:x}", f.rbp, f.r8, f.r9);
        crate::klog_info!("R10: 0x{:x}  R11: 0x{:x}  R12: 0x{:x}", f.r10, f.r11, f.r12);
        crate::klog_info!("R13: 0x{:x}  R14: 0x{:x}  R15: 0x{:x}", f.r13, f.r14, f.r15);
        crate::klog_info!("=== END INTERRUPT FRAME DUMP ===");
    }
}
pub fn kdiag_dump_stack_trace() {
    let rbp = cpu::read_rbp();
    crate::klog_info!("=== STACK TRACE ===");
    kdiag_dump_stack_trace_from_rbp(rbp);
    crate::klog_info!("=== END STACK TRACE ===");
}
pub fn kdiag_dump_stack_trace_from_rbp(rbp: u64) {
    let mut entries: [StacktraceEntry; KDIAG_STACK_TRACE_DEPTH] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; KDIAG_STACK_TRACE_DEPTH];

    let frame_count = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        KDIAG_STACK_TRACE_DEPTH as c_int,
    );

    if frame_count == 0 {
        crate::klog_info!("No stack frames found");
        return;
    }

    for i in 0..frame_count as usize {
        let entry = &entries[i];
        crate::klog_info!(
            "Frame {}: RBP=0x{:x} RIP=0x{:x}",
            i,
            entry.frame_pointer,
            entry.return_address
        );
    }
}
pub fn kdiag_dump_stack_trace_from_frame(frame: *const InterruptFrame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = &*frame;
        crate::klog_info!("=== STACK TRACE FROM EXCEPTION ===");
        crate::klog_info!("Exception occurred at RIP: 0x{:x}", f.rip);
        kdiag_dump_stack_trace_from_rbp(f.rbp);
        crate::klog_info!("=== END STACK TRACE ===");
    }
}
/// Read a single stack word at `rsp + idx * 8`, validating that the
/// address falls within `[stack_lo, stack_hi)` first. Returns `None`
/// if the address would be out of bounds.
///
/// Used by the supervisor-fault stack-dump path in `boot/`. Centralises
/// the unaligned-read unsafe in this diagnostic crate so callers stay
/// in safe Rust.
pub fn kdiag_stack_word_at(rsp: u64, idx: isize, stack_lo: usize, stack_hi: usize) -> Option<u64> {
    let base = rsp as *const u64;
    // SAFETY: pointer arithmetic only — no deref yet.
    let addr = unsafe { base.offset(idx) };
    let addr_val = addr as usize;
    if addr_val < stack_lo || addr_val + 8 > stack_hi {
        return None;
    }
    // SAFETY: bounds-checked above against the caller's stack window.
    Some(unsafe { core::ptr::read_unaligned(addr) })
}

pub fn kdiag_hexdump(data: *const u8, length: usize, base_address: u64) {
    if data.is_null() || length == 0 {
        return;
    }

    let bytes = slopos_ostd::util::ptr_buf::borrow_buf(data, length);

    let mut i = 0usize;
    while i < length {
        crate::klog_info!("0x{:x}: ", base_address + i as u64);

        let mut j = 0usize;
        while j < 16 && i + j < length {
            if j == 8 {
                crate::klog_info!(" ");
            }
            crate::klog_info!("{:02x} ", bytes[i + j]);
            j += 1;
        }

        while j < 16 {
            if j == 8 {
                crate::klog_info!(" ");
            }
            crate::klog_info!("   ");
            j += 1;
        }

        crate::klog_info!(" |");
        let mut j = 0usize;
        while j < 16 && i + j < length {
            let c = bytes[i + j];
            let display = if (32..=126).contains(&c) {
                c as char
            } else {
                '.'
            };
            crate::klog_info!("{}", display);
            j += 1;
        }
        crate::klog_info!("|");

        i += 16;
    }
}
