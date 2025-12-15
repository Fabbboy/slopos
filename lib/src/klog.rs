use core::ffi::c_int;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::io;

const COM1_BASE: u16 = 0x3f8;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KlogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl KlogLevel {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => KlogLevel::Error,
            1 => KlogLevel::Warn,
            2 => KlogLevel::Info,
            3 => KlogLevel::Debug,
            _ => KlogLevel::Trace,
        }
    }
}

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(KlogLevel::Info as u8);
static SERIAL_READY: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn is_enabled(level: KlogLevel) -> bool {
    level as u8 <= CURRENT_LEVEL.load(Ordering::Relaxed)
}

#[inline(always)]
fn putc(byte: u8) {
    // We keep the serial-ready flag for parity with the old implementation,
    // but both paths emit directly to COM1 to avoid dependency cycles.
    let _ready = SERIAL_READY.load(Ordering::Relaxed);
    unsafe {
        io::outb(COM1_BASE, byte);
    }
}

fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        putc(b);
    }
}

pub(crate) fn log_line(level: KlogLevel, text: &str) {
    if !is_enabled(level) {
        return;
    }
    write_bytes(text.as_bytes());
    putc(b'\n');
}

pub fn is_enabled_level(level: KlogLevel) -> bool {
    is_enabled(level)
}

pub fn log_args(level: KlogLevel, args: fmt::Arguments<'_>) {
    if !is_enabled(level) {
        return;
    }
    struct KlogWriter;
    impl fmt::Write for KlogWriter {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            write_bytes(s.as_bytes());
            Ok(())
        }
    }
    let _ = fmt::write(&mut KlogWriter, args);
}

#[unsafe(no_mangle)]
pub fn klog_init() {
    CURRENT_LEVEL.store(KlogLevel::Info as u8, Ordering::Relaxed);
    SERIAL_READY.store(false, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub fn klog_attach_serial() {
    SERIAL_READY.store(true, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub fn klog_set_level(level: KlogLevel) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub fn klog_get_level() -> KlogLevel {
    KlogLevel::from_raw(CURRENT_LEVEL.load(Ordering::Relaxed))
}

#[unsafe(no_mangle)]
pub fn klog_is_enabled(level: KlogLevel) -> c_int {
    if is_enabled(level) { 1 } else { 0 }
}

#[unsafe(no_mangle)]
pub fn klog_newline() {
    putc(b'\n');
}

#[macro_export]
macro_rules! klog {
    ($level:expr, $($arg:tt)*) => {{
        $crate::klog::log_args($level, ::core::format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! klog_error {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Error, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Warn, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Info, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_debug {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Debug, ::core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_trace {
    ($($arg:tt)*) => {
        $crate::klog::log_args($crate::klog::KlogLevel::Trace, ::core::format_args!($($arg)*))
    };
}
