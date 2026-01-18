//! Unified kernel panic infrastructure.
//!
//! All panics flow through `panic_handler_impl()`. Exception handlers can
//! enrich diagnostics by calling `set_panic_cpu_state()` before panicking.

use core::ffi::c_int;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_drivers::keyboard::poll_wait_enter;
use slopos_drivers::serial;
use slopos_lib::cpu;
use slopos_lib::stacktrace::{self, StacktraceEntry};
use slopos_mm::memory_init::is_memory_system_initialized;
use slopos_video::panic_screen;

use crate::shutdown::{execute_kernel, kernel_shutdown};

static PANIC_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static PANIC_RIP: AtomicU64 = AtomicU64::new(0);
static PANIC_RSP: AtomicU64 = AtomicU64::new(0);
static PANIC_HAS_CPU_STATE: AtomicBool = AtomicBool::new(false);
const PANIC_BACKTRACE_MAX: usize = 16;

/// Set CPU state from an interrupt frame to be included in panic diagnostics.
#[inline]
pub fn set_panic_cpu_state(rip: u64, rsp: u64) {
    PANIC_RIP.store(rip, Ordering::SeqCst);
    PANIC_RSP.store(rsp, Ordering::SeqCst);
    PANIC_HAS_CPU_STATE.store(true, Ordering::SeqCst);
}

fn take_panic_cpu_state() -> (Option<u64>, Option<u64>) {
    if PANIC_HAS_CPU_STATE.swap(false, Ordering::SeqCst) {
        (
            Some(PANIC_RIP.load(Ordering::SeqCst)),
            Some(PANIC_RSP.load(Ordering::SeqCst)),
        )
    } else {
        (None, None)
    }
}

fn panic_serial_write(s: &str) {
    serial::write_line(s);
}

fn panic_dump_backtrace() {
    let rbp = cpu::read_rbp();
    let mut entries: [StacktraceEntry; PANIC_BACKTRACE_MAX] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PANIC_BACKTRACE_MAX];

    let captured = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        PANIC_BACKTRACE_MAX as c_int,
    );
    if captured <= 0 {
        panic_serial_write("Backtrace: <empty>");
        return;
    }

    panic_serial_write("Backtrace (most recent call first):");
    for i in 0..captured as usize {
        let entry = &entries[i];
        let mut line = MessageBuffer::new();
        let _ = write!(
            line,
            "  #{} rbp=0x{:016x} rip=0x{:016x}",
            i, entry.frame_pointer, entry.return_address
        );
        panic_serial_write(line.as_str());
    }
}

/// Core panic implementation. Called by the kernel's `#[panic_handler]`.
pub fn panic_handler_impl(info: &PanicInfo) -> ! {
    cpu::disable_interrupts();

    if PANIC_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        panic_serial_write("\n!!! RECURSIVE PANIC DETECTED - HALTING !!!\n");
        cpu::halt_loop();
    }

    let (extra_rip, extra_rsp) = take_panic_cpu_state();

    let current_rsp = cpu::read_rsp();
    let cr0 = cpu::read_cr0();
    let cr3 = cpu::read_cr3();
    let cr4 = cpu::read_cr4();

    let display_rip = extra_rip;
    let display_rsp = extra_rsp.unwrap_or(current_rsp);

    panic_serial_write("\n\n=== KERNEL PANIC ===");

    let mut message_buf = MessageBuffer::new();

    if let Some(location) = info.location() {
        let _ = write!(
            message_buf,
            "{}:{}:{}: ",
            location.file(),
            location.line(),
            location.column()
        );
    }

    if let Some(msg) = info.message().as_str() {
        let _ = write!(message_buf, "{}", msg);
    } else {
        let _ = write!(message_buf, "{}", info.message());
    }

    let message_str = message_buf.as_str();
    panic_serial_write(message_str);

    panic_serial_write("Register snapshot:");
    if let Some(rip) = display_rip {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("RIP", rip));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("RSP", display_rsp));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR0", cr0));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR3", cr3));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR4", cr4));
    }

    panic_dump_backtrace();

    panic_serial_write("===================");
    panic_serial_write("Kernel panic: unrecoverable error");

    if panic_screen::display_panic_screen(
        Some(message_str),
        display_rip,
        Some(display_rsp),
        cr0,
        cr3,
        cr4,
    ) {
        panic_serial_write("Press ENTER to shutdown...");
        poll_wait_enter();
    } else {
        panic_serial_write("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_serial_write("Memory system unavailable; skipping paint ritual");
    }

    kernel_shutdown(b"panic\0".as_ptr() as *const core::ffi::c_char);
}

struct MessageBuffer {
    buf: [u8; 512],
    len: usize,
}

impl MessageBuffer {
    const fn new() -> Self {
        Self {
            buf: [0u8; 512],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // SAFETY: we only write valid UTF-8 through the Write impl
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl Write for MessageBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let available = self.buf.len() - self.len;
        let to_copy = bytes.len().min(available);
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

struct HexBuffer {
    buf: [u8; 32],
}

impl HexBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; 32] }
    }

    fn format_labeled(&mut self, label: &str, value: u64) -> &str {
        const HEX_CHARS: &[u8] = b"0123456789ABCDEF";

        let mut pos = 0;

        for &b in label.as_bytes() {
            if pos < self.buf.len() {
                self.buf[pos] = b;
                pos += 1;
            }
        }

        if pos + 4 <= self.buf.len() {
            self.buf[pos] = b':';
            self.buf[pos + 1] = b' ';
            self.buf[pos + 2] = b'0';
            self.buf[pos + 3] = b'x';
            pos += 4;
        }

        for i in 0..16 {
            if pos < self.buf.len() {
                let nibble = ((value >> (60 - i * 4)) & 0xF) as usize;
                self.buf[pos] = HEX_CHARS[nibble];
                pos += 1;
            }
        }

        // SAFETY: we only write ASCII bytes
        unsafe { core::str::from_utf8_unchecked(&self.buf[..pos]) }
    }
}
