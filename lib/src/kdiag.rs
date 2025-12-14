use core::arch::asm;
use core::ffi::{c_char, c_int};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::cpu;
use crate::klog::{self, KlogLevel};
use crate::stacktrace::{self, stacktrace_entry};
use crate::tsc;

pub const KDIAG_STACK_TRACE_DEPTH: usize = 16;

const GPR_FMT: &[u8] = b"General Purpose Registers:\n\
  RAX: 0x%lx  RBX: 0x%lx  RCX: 0x%lx  RDX: 0x%lx\n\
  RSI: 0x%lx  RDI: 0x%lx  RBP: 0x%lx  RSP: 0x%lx\n\
  R8 : 0x%lx  R9 : 0x%lx  R10: 0x%lx  R11: 0x%lx\n\
  R12: 0x%lx  R13: 0x%lx  R14: 0x%lx  R15: 0x%lx\n\0";

const FLAGS_FMT: &[u8] =
    b"Flags Register:\n  RFLAGS: 0x%lx [CF:%d PF:%d AF:%d ZF:%d SF:%d TF:%d IF:%d DF:%d OF:%d]\n\0";

const SEGMENT_FMT: &[u8] =
    b"Segment Registers:\n  CS: 0x%04x  DS: 0x%04x  ES: 0x%04x  FS: 0x%04x  GS: 0x%04x  SS: 0x%04x\n\0";

const CONTROL_FMT: &[u8] =
    b"Control Registers:\n  CR0: 0x%lx  CR2: 0x%lx\n  CR3: 0x%lx  CR4: 0x%lx\n\0";

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct interrupt_frame {
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
pub extern "C" fn kdiag_timestamp() -> u64 {
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
pub extern "C" fn kdiag_dump_cpu_state() {
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

    unsafe {
        klog::klog_printf(
            KlogLevel::Info,
            b"=== CPU STATE DUMP ===\n\0".as_ptr() as *const c_char,
        );
        klog::klog_printf(
            KlogLevel::Info,
            GPR_FMT.as_ptr() as *const c_char,
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
        );
        klog::klog_printf(
            KlogLevel::Info,
            FLAGS_FMT.as_ptr() as *const c_char,
            rflags,
            ((rflags & (1 << 0)) != 0) as c_int,
            ((rflags & (1 << 2)) != 0) as c_int,
            ((rflags & (1 << 4)) != 0) as c_int,
            ((rflags & (1 << 6)) != 0) as c_int,
            ((rflags & (1 << 7)) != 0) as c_int,
            ((rflags & (1 << 8)) != 0) as c_int,
            ((rflags & (1 << 9)) != 0) as c_int,
            ((rflags & (1 << 10)) != 0) as c_int,
            ((rflags & (1 << 11)) != 0) as c_int,
        );
        klog::klog_printf(
            KlogLevel::Info,
            SEGMENT_FMT.as_ptr() as *const c_char,
            cs as u32,
            ds as u32,
            es as u32,
            fs as u32,
            gs as u32,
            ss as u32,
        );
        klog::klog_printf(
            KlogLevel::Info,
            CONTROL_FMT.as_ptr() as *const c_char,
            cr0,
            cr2,
            cr3,
            cr4,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"=== END CPU STATE DUMP ===\n\0".as_ptr() as *const c_char,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kdiag_dump_interrupt_frame(frame: *const interrupt_frame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = &*frame;
        klog::klog_printf(
            KlogLevel::Info,
            b"=== INTERRUPT FRAME DUMP ===\n\0".as_ptr() as *const _,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"Vector: %u (%s) Error Code: 0x%lx\n\0".as_ptr() as *const _,
            f.vector as u32,
            exception_name(f.vector as u8).as_ptr() as *const c_char,
            f.error_code,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"RIP: 0x%lx  CS: 0x%lx  RFLAGS: 0x%lx\n\0".as_ptr() as *const _,
            f.rip,
            f.cs,
            f.rflags,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"RSP: 0x%lx  SS: 0x%lx\n\0".as_ptr() as *const _,
            f.rsp,
            f.ss,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"RAX: 0x%lx  RBX: 0x%lx  RCX: 0x%lx\n\0".as_ptr() as *const _,
            f.rax,
            f.rbx,
            f.rcx,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"RDX: 0x%lx  RSI: 0x%lx  RDI: 0x%lx\n\0".as_ptr() as *const _,
            f.rdx,
            f.rsi,
            f.rdi,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"RBP: 0x%lx  R8: 0x%lx  R9: 0x%lx\n\0".as_ptr() as *const _,
            f.rbp,
            f.r8,
            f.r9,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"R10: 0x%lx  R11: 0x%lx  R12: 0x%lx\n\0".as_ptr() as *const _,
            f.r10,
            f.r11,
            f.r12,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"R13: 0x%lx  R14: 0x%lx  R15: 0x%lx\n\0".as_ptr() as *const _,
            f.r13,
            f.r14,
            f.r15,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"=== END INTERRUPT FRAME DUMP ===\n\0".as_ptr() as *const _,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kdiag_dump_stack_trace() {
    let rbp = cpu::read_rbp();
    unsafe {
        klog::klog_printf(
            KlogLevel::Info,
            b"=== STACK TRACE ===\n\0".as_ptr() as *const _,
        );
    }
    kdiag_dump_stack_trace_from_rbp(rbp);
    unsafe {
        klog::klog_printf(
            KlogLevel::Info,
            b"=== END STACK TRACE ===\n\0".as_ptr() as *const _,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kdiag_dump_stack_trace_from_rbp(rbp: u64) {
    let mut entries: [stacktrace_entry; KDIAG_STACK_TRACE_DEPTH] =
        [stacktrace_entry { frame_pointer: 0, return_address: 0 }; KDIAG_STACK_TRACE_DEPTH];

    let frame_count =
        stacktrace::stacktrace_capture_from(rbp, entries.as_mut_ptr(), KDIAG_STACK_TRACE_DEPTH as c_int);

    if frame_count == 0 {
        unsafe {
            klog::klog_printf(
                KlogLevel::Info,
                b"No stack frames found\n\0".as_ptr() as *const _,
            );
        }
        return;
    }

    for i in 0..frame_count as usize {
        let entry = &entries[i];
        unsafe {
            klog::klog_printf(
                KlogLevel::Info,
                b"Frame %d: RBP=0x%lx RIP=0x%lx\n\0".as_ptr() as *const _,
                i as i32,
                entry.frame_pointer,
                entry.return_address,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kdiag_dump_stack_trace_from_frame(frame: *const interrupt_frame) {
    if frame.is_null() {
        return;
    }
    unsafe {
        let f = &*frame;
        klog::klog_printf(
            KlogLevel::Info,
            b"=== STACK TRACE FROM EXCEPTION ===\n\0".as_ptr() as *const _,
        );
        klog::klog_printf(
            KlogLevel::Info,
            b"Exception occurred at RIP: 0x%lx\n\0".as_ptr() as *const _,
            f.rip,
        );
        kdiag_dump_stack_trace_from_rbp(f.rbp);
        klog::klog_printf(
            KlogLevel::Info,
            b"=== END STACK TRACE ===\n\0".as_ptr() as *const _,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kdiag_hexdump(data: *const u8, length: usize, base_address: u64) {
    if data.is_null() || length == 0 {
        return;
    }

    let bytes = unsafe { core::slice::from_raw_parts(data, length) };

    let mut i = 0usize;
    while i < length {
        unsafe {
            klog::klog_printf(
                KlogLevel::Info,
                b"0x%lx: \0".as_ptr() as *const _,
                base_address + i as u64,
            );
        }

        let mut j = 0usize;
        while j < 16 && i + j < length {
            if j == 8 {
                unsafe { klog::klog_printf(KlogLevel::Info, b" \0".as_ptr() as *const _) };
            }
            unsafe {
                klog::klog_printf(
                    KlogLevel::Info,
                    b"%02x \0".as_ptr() as *const _,
                    bytes[i + j] as c_int,
                );
            }
            j += 1;
        }

        while j < 16 {
            if j == 8 {
                unsafe { klog::klog_printf(KlogLevel::Info, b" \0".as_ptr() as *const _) };
            }
            unsafe { klog::klog_printf(KlogLevel::Info, b"   \0".as_ptr() as *const _) };
            j += 1;
        }

        unsafe { klog::klog_printf(KlogLevel::Info, b" |\0".as_ptr() as *const _) };
        let mut j = 0usize;
        while j < 16 && i + j < length {
            let c = bytes[i + j];
            let display = if (32..=126).contains(&c) { c } else { b'.' };
            unsafe {
                klog::klog_printf(
                    KlogLevel::Info,
                    b"%c\0".as_ptr() as *const _,
                    display as c_int,
                );
            }
            j += 1;
        }
        unsafe { klog::klog_printf(KlogLevel::Info, b"|\n\0".as_ptr() as *const _) };

        i += 16;
    }
}
