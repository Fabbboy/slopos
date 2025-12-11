use core::fmt::{self, Write};
use spin::Mutex;
use slopos_lib::io;

static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(0x3f8));
const BUF_SIZE: usize = 256;

struct SerialBuffer {
    buf: [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}

impl SerialBuffer {
    const fn new() -> Self {
        Self {
            buf: [0; BUF_SIZE],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % BUF_SIZE;
        if next != self.tail {
            self.buf[self.head] = byte;
            self.head = next;
        }
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % BUF_SIZE;
        Some(byte)
    }

    fn is_empty(&self) -> bool {
        self.head == self.tail
    }
}

static INPUT_BUFFER: Mutex<SerialBuffer> = Mutex::new(SerialBuffer::new());

pub fn init() {
    let mut port = SERIAL.lock();
    unsafe { port.init(); }
}

#[no_mangle]
pub extern "C" fn serial_enable_interrupts(_port: u16, _irq: u8) -> i32 {
    0
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

#[no_mangle]
pub extern "C" fn serial_poll_receive(port: u16) {
    // Poll the UART Line Status Register (offset +5) for data ready.
    while unsafe { io::inb(port + 5) } & 0x01 != 0 {
        let byte = unsafe { io::inb(port) };
        let mut buf = INPUT_BUFFER.lock();
        buf.push(byte);
    }
}

#[no_mangle]
pub extern "C" fn serial_buffer_pending(port: u16) -> i32 {
    // Ensure we service the port before reporting availability.
    serial_poll_receive(port);
    let buf = INPUT_BUFFER.lock();
    (!buf.is_empty()) as i32
}

#[no_mangle]
pub extern "C" fn serial_buffer_read(port: u16, out: *mut u8) -> i32 {
    // Refresh buffer then attempt to pop one byte.
    serial_poll_receive(port);
    let mut buf = INPUT_BUFFER.lock();
    match buf.pop() {
        Some(b) => {
            if !out.is_null() {
                unsafe { *out = b };
            }
            0
        }
        None => -1,
    }
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

