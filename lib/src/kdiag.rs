use core::arch::asm;
use core::ffi::{c_char, c_int, CStr};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::cpu;
use crate::stacktrace::{self, StacktraceEntry};
use crate::tsc;

pub const KDIAG_STACK_TRACE_DEPTH: usize = 16;

#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

fn exception_name(vector: u8) -> &'static [u8] {
    match vector {
        0 => b"Divide Error\0",
        1 => b"Debug\0",
        2 => b"NMI\0",
        3 => b"Breakpoint\0",
        4 => b"Overflow\0",
        5 => b"Bound Range\0",
        6 => b"Invalid Opcode\0",
        7 => b"Device Not Available\0",
        8 => b"Double Fault\0",
        10 => b"Invalid TSS\0",
        11 => b"Segment Not Present\0",
        12 => b"Stack Fault\0",
        13 => b"General Protection\0",
        14 => b"Page Fault\0",
        16 => b"FPU Error\0",
        17 => b"Alignment Check\0",
        18 => b"Machine Check\0",
        19 => b"SIMD FP Exception\0",
        _ => b"Unknown\0",
    }
}

static MONOTONIC_TIME: AtomicU64 = AtomicU64::new(0);
static LAST_TSC: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
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

#[unsafe(no_mangle)]
pub fn kdiag_dump_cpu_state() {
    let (rsp, rbp, rax, rbx, rcx, rdx, rsi, rdi): (u64, u64, u64, u64, u64, u64, u64, u64);
    let (r8, r9, r10, r11, r12, r13, r14, r15): (u64, u64, u64, u64, u64, u64, u64, u64);
    let (rflags, cr0, cr2, cr3, cr4): (u64, u64, u64, u64, u64);
    let (cs, ds, es, fs, gs, ss): (u16, u16, u16, u16, u16, u16);

    unsafe {
        asm!("mov {}, rsp", out(reg) rsp);
        asm!("mov {}, rbp", out(reg) rbp);
        asm!("mov {}, rax", out(reg) rax);
        asm!("mov {}, rbx", out(reg) rbx);
        asm!("mov {}, rcx", out(reg) rcx);
        asm!("mov {}, rdx", out(reg) rdx);
        asm!("mov {}, rsi", out(reg) rsi);
        asm!("mov {}, rdi", out(reg) rdi);
        asm!("mov {}, r8", out(reg) r8);
        asm!("mov {}, r9", out(reg) r9);
        asm!("mov {}, r10", out(reg) r10);
        asm!("mov {}, r11", out(reg) r11);
        asm!("mov {}, r12", out(reg) r12);
        asm!("mov {}, r13", out(reg) r13);
        asm!("mov {}, r14", out(reg) r14);
        asm!("mov {}, r15", out(reg) r15);
        asm!("pushfq; pop {}", out(reg) rflags);

        asm!("mov {0:x}, cs", out(reg) cs);
        asm!("mov {0:x}, ds", out(reg) ds);
        asm!("mov {0:x}, es", out(reg) es);
        asm!("mov {0:x}, fs", out(reg) fs);
        asm!("mov {0:x}, gs", out(reg) gs);
        asm!("mov {0:x}, ss", out(reg) ss);

        asm!("mov {}, cr0", out(reg) cr0);
        asm!("mov {}, cr2", out(reg) cr2);
        asm!("mov {}, cr3", out(reg) cr3);
        asm!("mov {}, cr4", out(reg) cr4);
    }

    crate::klog_info!("=== CPU STATE DUMP ===");
    crate::klog_info!(
        "General Purpose Registers:\n  RAX: 0x{:x}  RBX: 0x{:x}  RCX: 0x{:x}  RDX: 0x{:x}\n  RSI: 0x{:x}  RDI: 0x{:x}  RBP: 0x{:x}  RSP: 0x{:x}\n  R8 : 0x{:x}  R9 : 0x{:x}  R10: 0x{:x}  R11: 0x{:x}\n  R12: 0x{:x}  R13: 0x{:x}  R14: 0x{:x}  R15: 0x{:x}",
        rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp, r8, r9, r10, r11, r12, r13, r14, r15
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
        cs, ds, es, fs, gs, ss
    );
    crate::klog_info!(
        "Control Registers:\n  CR0: 0x{:x}  CR2: 0x{:x}\n  CR3: 0x{:x}  CR4: 0x{:x}",
        cr0, cr2, cr3, cr4
    );
    crate::klog_info!("=== END CPU STATE DUMP ===");
}

#[unsafe(no_mangle)]
pub fn kdiag_dump_interrupt_frame(frame: *const InterruptFrame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = &*frame;
        let exc_name = CStr::from_ptr(exception_name(f.vector as u8).as_ptr() as *const c_char)
            .to_str()
            .unwrap_or("<invalid utf-8>");
        crate::klog_info!("=== INTERRUPT FRAME DUMP ===");
        crate::klog_info!(
            "Vector: {} ({}) Error Code: 0x{:x}",
            f.vector,
            exc_name,
            f.error_code
        );
        crate::klog_info!("RIP: 0x{:x}  CS: 0x{:x}  RFLAGS: 0x{:x}", f.rip, f.cs, f.rflags);
        crate::klog_info!("RSP: 0x{:x}  SS: 0x{:x}", f.rsp, f.ss);
        crate::klog_info!("RAX: 0x{:x}  RBX: 0x{:x}  RCX: 0x{:x}", f.rax, f.rbx, f.rcx);
        crate::klog_info!("RDX: 0x{:x}  RSI: 0x{:x}  RDI: 0x{:x}", f.rdx, f.rsi, f.rdi);
        crate::klog_info!("RBP: 0x{:x}  R8: 0x{:x}  R9: 0x{:x}", f.rbp, f.r8, f.r9);
        crate::klog_info!("R10: 0x{:x}  R11: 0x{:x}  R12: 0x{:x}", f.r10, f.r11, f.r12);
        crate::klog_info!("R13: 0x{:x}  R14: 0x{:x}  R15: 0x{:x}", f.r13, f.r14, f.r15);
        crate::klog_info!("=== END INTERRUPT FRAME DUMP ===");
    }
}

#[unsafe(no_mangle)]
pub fn kdiag_dump_stack_trace() {
    let rbp = cpu::read_rbp();
    crate::klog_info!("=== STACK TRACE ===");
    kdiag_dump_stack_trace_from_rbp(rbp);
    crate::klog_info!("=== END STACK TRACE ===");
}

#[unsafe(no_mangle)]
pub fn kdiag_dump_stack_trace_from_rbp(rbp: u64) {
    let mut entries: [StacktraceEntry; KDIAG_STACK_TRACE_DEPTH] =
        [StacktraceEntry { frame_pointer: 0, return_address: 0 }; KDIAG_STACK_TRACE_DEPTH];

    let frame_count =
        stacktrace::stacktrace_capture_from(rbp, entries.as_mut_ptr(), KDIAG_STACK_TRACE_DEPTH as c_int);

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

#[unsafe(no_mangle)]
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

#[unsafe(no_mangle)]
pub fn kdiag_hexdump(data: *const u8, length: usize, base_address: u64) {
    if data.is_null() || length == 0 {
        return;
    }

    let bytes = unsafe { core::slice::from_raw_parts(data, length) };

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
            let display = if (32..=126).contains(&c) { c as char } else { '.' };
            crate::klog_info!("{}", display);
            j += 1;
        }
        crate::klog_info!("|");

        i += 16;
    }
}
