use core::fmt::{self, Write};
use spin::Mutex;
use slopos_lib::io;

static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(0x3f8));

pub fn init() {
    let mut port = SERIAL.lock();
    unsafe { port.init(); }
}

pub fn write_str(s: &str) {
    let _ = SERIAL.lock().write_str(s);
}

pub fn write_line(s: &str) {
    let mut guard = SERIAL.lock();
    let _ = guard.write_str(s);
    let _ = guard.write_str("\r\n");
}

pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = SERIAL.lock().write_fmt(args);
}

struct SerialPort {
    base: u16,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    unsafe fn init(&mut self) {
        unsafe {
            io::outb(self.base + 1, 0x00);
            io::outb(self.base + 3, 0x80);
            io::outb(self.base + 0, 0x03);
            io::outb(self.base + 1, 0x00);
            io::outb(self.base + 3, 0x03);
            io::outb(self.base + 2, 0xC7);
            io::outb(self.base + 4, 0x0B);
        }
    }

    fn write_byte(&mut self, byte: u8) {
        unsafe {
            while (io::inb(self.base + 5) & 0x20) == 0 {
                core::hint::spin_loop();
            }
            io::outb(self.base, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            match b {
                b'\n' => {
                    self.write_byte(b'\r');
                    self.write_byte(b'\n');
                }
                _ => self.write_byte(b),
            }
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {{
        $crate::serial::print_args(core::format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! serial_println {
    () => {
        $crate::serial::write_line("");
    };
    ($fmt:expr) => {
        $crate::serial::write_line($fmt);
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::serial::print_args(core::format_args!(concat!($fmt, "\n"), $($arg)*));
    };
}

