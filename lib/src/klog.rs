#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void, CStr, VaList};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::{io, numfmt, string};

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

fn write_padded(bytes: &[u8], width: i32, zero_pad: bool) {
    let len = bytes.len() as i32;
    let padding = if width > len { width - len } else { 0 };
    let pad_char = if zero_pad { b'0' } else { b' ' };

    for _ in 0..padding {
        putc(pad_char);
    }
    write_bytes(bytes);
}

pub(crate) fn log_line(level: KlogLevel, text: &str) {
    if !is_enabled(level) {
        return;
    }
    write_bytes(text.as_bytes());
    putc(b'\n');
}

#[no_mangle]
pub extern "C" fn klog_init() {
    CURRENT_LEVEL.store(KlogLevel::Info as u8, Ordering::Relaxed);
    SERIAL_READY.store(false, Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn klog_attach_serial() {
    SERIAL_READY.store(true, Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn klog_set_level(level: KlogLevel) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn klog_get_level() -> KlogLevel {
    KlogLevel::from_raw(CURRENT_LEVEL.load(Ordering::Relaxed))
}

#[no_mangle]
pub extern "C" fn klog_is_enabled(level: KlogLevel) -> c_int {
    if is_enabled(level) {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn klog_newline() {
    putc(b'\n');
}

#[no_mangle]
pub unsafe extern "C" fn klog_info(msg: *const u8, _: ...) {
    unsafe {
        klog_printf(KlogLevel::Info, msg as *const c_char);
    }
}

#[no_mangle]
pub unsafe extern "C" fn klog_debug(msg: *const u8, _: ...) {
    unsafe {
        klog_printf(KlogLevel::Debug, msg as *const c_char);
    }
}

#[derive(Clone, Copy)]
enum Length {
    Default,
    Long,
    LongLong,
    Size,
}

unsafe fn format_string(args: &mut VaList<'_>, width: i32, zero_pad: bool) {
    let str_ptr: *const c_char = unsafe { args.arg() };
    let s = if str_ptr.is_null() {
        b"(null)\0".as_ptr() as *const c_char
    } else {
        str_ptr
    };
    let len = unsafe { string::strlen_internal(s) };
    let bytes = unsafe { core::slice::from_raw_parts(s as *const u8, len) };
    write_padded(bytes, width, zero_pad);
}

unsafe fn format_char(args: &mut VaList<'_>, width: i32, zero_pad: bool) {
    let c: i32 = unsafe { args.arg() };
    let tmp = [c as u8];
    write_padded(&tmp, width, zero_pad);
}

unsafe fn format_signed(
    args: &mut VaList<'_>,
    length: Length,
    width: i32,
    zero_pad: bool,
) {
    let mut value: i64 = unsafe {
        match length {
            Length::LongLong => args.arg::<i64>(),
            Length::Long => args.arg::<i64>(),
            Length::Size => args.arg::<isize>() as i64,
            Length::Default => args.arg::<i32>() as i64,
        }
    };

    let mut negative = value < 0;
    let magnitude = if negative {
        value = value.wrapping_neg();
        value as u64
    } else {
        value as u64
    };

    let mut buffer = [0u8; 48];
    let digits = unsafe {
        numfmt::u64_to_decimal_internal(
            magnitude,
            buffer.as_mut_ptr() as *mut c_char,
            buffer.len(),
        )
    };
    if digits == 0 {
        buffer[0] = b'0';
    }

    let total = digits + if negative { 1 } else { 0 };
    let pad_char = if zero_pad { b'0' } else { b' ' };
    let padding = if width > total as i32 {
        (width as usize).saturating_sub(total)
    } else {
        0
    };

    if negative && pad_char == b'0' {
        putc(b'-');
        negative = false;
    }

    for _ in 0..padding {
        putc(pad_char);
    }

    if negative {
        putc(b'-');
    }

    write_bytes(&buffer[..digits.max(1)]);
}

unsafe fn format_unsigned(
    args: &mut VaList<'_>,
    length: Length,
    width: i32,
    zero_pad: bool,
    spec: u8,
) {
    let value: u64 = unsafe {
        match length {
            Length::LongLong => args.arg::<u64>(),
            Length::Long => args.arg::<u64>(),
            Length::Size => args.arg::<usize>() as u64,
            Length::Default => args.arg::<u32>() as u64,
        }
    };

    let mut buffer = [0u8; 48];
    let len = if spec == b'u' {
        unsafe {
            numfmt::u64_to_decimal_internal(
                value,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.len(),
            )
        }
    } else {
        unsafe {
            numfmt::u64_to_hex_internal(
                value,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.len(),
                false,
            )
        }
    };

    let mut bytes = &buffer[..len.max(1)];
    if spec == b'x' {
        for b in buffer.iter_mut().take(len) {
            if (b'A'..=b'F').contains(b) {
                *b = *b - b'A' + b'a';
            }
        }
        bytes = &buffer[..len.max(1)];
    }

    write_padded(bytes, width, zero_pad);
}

unsafe fn format_pointer(args: &mut VaList<'_>, width: i32, zero_pad: bool) {
    let ptr_val: *mut c_void = unsafe { args.arg() };
    let mut buffer = [0u8; 48];
    let len = unsafe {
        numfmt::u64_to_hex_internal(
            ptr_val as u64,
            buffer.as_mut_ptr() as *mut c_char,
            buffer.len(),
            true,
        )
    };
    let bytes = &buffer[..len.max(1)];
    write_padded(bytes, width, zero_pad);
}

#[no_mangle]
pub unsafe extern "C" fn klog_printf(
    level: KlogLevel,
    fmt: *const c_char,
    mut args: ...
) {
    if !is_enabled(level) || fmt.is_null() {
        return;
    }

    let fmt_bytes = unsafe { CStr::from_ptr(fmt).to_bytes() };
    let mut idx = 0usize;

    while idx < fmt_bytes.len() {
        let ch = fmt_bytes[idx];
        if ch != b'%' {
            putc(ch);
            idx += 1;
            continue;
        }

        idx += 1;
        if idx >= fmt_bytes.len() {
            putc(b'%');
            break;
        }

        if fmt_bytes[idx] == b'%' {
            putc(b'%');
            idx += 1;
            continue;
        }

        let mut zero_pad = false;
        let mut width: i32 = 0;

        if fmt_bytes[idx] == b'0' {
            zero_pad = true;
            idx += 1;
        }

        while idx < fmt_bytes.len() && (fmt_bytes[idx] as char).is_ascii_digit() {
            width = width.saturating_mul(10) + (fmt_bytes[idx] - b'0') as i32;
            idx += 1;
        }

        let mut length = Length::Default;
        if idx < fmt_bytes.len() && fmt_bytes[idx] == b'l' {
            idx += 1;
            if idx < fmt_bytes.len() && fmt_bytes[idx] == b'l' {
                length = Length::LongLong;
                idx += 1;
            } else {
                length = Length::Long;
            }
        } else if idx < fmt_bytes.len() && fmt_bytes[idx] == b'z' {
            length = Length::Size;
            idx += 1;
        }

        if idx >= fmt_bytes.len() {
            break;
        }

        let spec = fmt_bytes[idx];
        idx += 1;

        match spec {
            b's' => unsafe { format_string(&mut args, width, zero_pad) },
            b'c' => unsafe { format_char(&mut args, width, zero_pad) },
            b'd' | b'i' => unsafe { format_signed(&mut args, length, width, zero_pad) },
            b'u' | b'x' | b'X' => unsafe { format_unsigned(&mut args, length, width, zero_pad, spec) },
            b'p' => unsafe { format_pointer(&mut args, width, zero_pad) },
            _ => {
                putc(b'%');
                if spec != 0 {
                    putc(spec);
                } else {
                    idx = idx.saturating_sub(1);
                }
            }
        }
    }
}


