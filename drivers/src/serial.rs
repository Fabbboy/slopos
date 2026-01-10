use core::fmt::{self, Write};
use slopos_lib::{RingBuffer, io};
use spin::Mutex;

use slopos_abi::arch::x86_64::ports::{
    COM1_BASE, UART_FCR_14_BYTE_THRESHOLD as FCR_14_BYTE_THRESHOLD,
    UART_FCR_CLEAR_RX as FCR_CLEAR_RX, UART_FCR_CLEAR_TX as FCR_CLEAR_TX,
    UART_FCR_ENABLE_FIFO as FCR_ENABLE_FIFO, UART_IIR_FIFO_ENABLED as IIR_FIFO_ENABLED,
    UART_IIR_FIFO_MASK as IIR_FIFO_MASK, UART_LCR_DLAB as LCR_DLAB,
    UART_LSR_DATA_READY as LSR_DATA_READY, UART_LSR_TX_EMPTY as LSR_TX_EMPTY,
    UART_MCR_AUX2 as MCR_AUX2, UART_MCR_DTR as MCR_DTR, UART_MCR_RTS as MCR_RTS,
    UART_REG_IER as REG_IER, UART_REG_IIR as REG_IIR, UART_REG_LCR as REG_LCR,
    UART_REG_LSR as REG_LSR, UART_REG_MCR as REG_MCR, UART_REG_RBR as REG_RBR,
    UART_REG_SCR as REG_SCR,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartType {
    Uart8250,   // Original, no FIFO
    Uart16450,  // No FIFO, but register compatible
    Uart16550,  // Has FIFO (may be buggy)
    Uart16550A, // Has working FIFO
    Uart16650,  // Enhanced FIFO (32 bytes)
    Uart16750,  // Enhanced FIFO (64 bytes)
    Unknown,    // Could not determine
}

#[derive(Debug, Clone, Copy)]
pub struct UartCapabilities {
    pub uart_type: UartType,
    pub has_fifo: bool,
    pub fifo_working: bool,
    pub fifo_size: usize,
}

static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1_BASE));
const BUF_SIZE: usize = 256;

type SerialBuffer = RingBuffer<u8, BUF_SIZE>;

static INPUT_BUFFER: Mutex<SerialBuffer> = Mutex::new(SerialBuffer::new_with(0));

/// Initialize the default serial port (COM1)
pub fn init() {
    let mut port = SERIAL.lock();
    unsafe {
        port.init();
    }
}

/// Initialize a specific serial port by base address
pub fn init_port(base: u16) -> Result<UartCapabilities, ()> {
    // For now, we only support the default port
    // Future enhancement: support multiple ports
    if base == COM1_BASE {
        let mut port = SERIAL.lock();
        unsafe {
            port.init();
        }
        Ok(port.capabilities())
    } else {
        Err(())
    }
}

/// Get capabilities of the current serial port
pub fn get_capabilities() -> UartCapabilities {
    SERIAL.lock().capabilities()
}

pub fn write_str(s: &str) {
    let _ = SERIAL.lock().write_str(s);
}

pub fn write_line(s: &str) {
    let mut guard = SERIAL.lock();
    let _ = guard.write_str(s);
    let _ = guard.write_str("\r\n");
}

/// Write a single byte to COM1 (for compatibility with legacy code)
pub fn serial_putc_com1(ch: u8) {
    let mut guard = SERIAL.lock();
    guard.write_byte(ch);
}

pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = SERIAL.lock().write_fmt(args);
}
pub fn serial_poll_receive(port: u16) {
    while unsafe { io::inb(port + REG_LSR) } & LSR_DATA_READY != 0 {
        let byte = unsafe { io::inb(port + REG_RBR) };
        let mut buf = INPUT_BUFFER.lock();
        let _ = buf.try_push(byte);
    }
}
pub fn serial_buffer_pending(port: u16) -> i32 {
    // Ensure we service the port before reporting availability.
    serial_poll_receive(port);
    let buf = INPUT_BUFFER.lock();
    (!buf.is_empty()) as i32
}
pub fn serial_buffer_read(port: u16, out: *mut u8) -> i32 {
    serial_poll_receive(port);
    let mut buf = INPUT_BUFFER.lock();
    match buf.try_pop() {
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
    caps: UartCapabilities,
}

impl SerialPort {
    const fn new(base: u16) -> Self {
        Self {
            base,
            caps: UartCapabilities {
                uart_type: UartType::Unknown,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            },
        }
    }

    /// Detect UART type and capabilities
    unsafe fn detect_uart(&mut self) -> UartCapabilities {
        // Test for FIFO presence by writing to FCR (IIR when writing)
        // If FIFO exists, IIR bits 6-7 will be set when reading back
        io::outb(
            self.base + REG_IIR,
            FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX,
        );

        // Small delay to allow FIFO to initialize
        for _ in 0..10 {
            core::hint::spin_loop();
        }

        let iir_after = io::inb(self.base + REG_IIR);
        let has_fifo = (iir_after & IIR_FIFO_MASK) == IIR_FIFO_ENABLED;

        // Clear FIFO for clean state (will be reconfigured in init)
        io::outb(self.base + REG_IIR, 0);

        if !has_fifo {
            // No FIFO - could be 8250 or 16450
            // 16450 is register-compatible with 16550 but lacks FIFO
            // We can't easily distinguish them, so default to 16450
            return UartCapabilities {
                uart_type: UartType::Uart16450,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            };
        }

        // Has FIFO - test if it's working (16550A vs buggy 16550)
        // Write test pattern to scratch register and read back
        let test_value = 0xAA;
        io::outb(self.base + REG_SCR, test_value);
        let scratch_read = io::inb(self.base + REG_SCR);

        // Check if scratch register works (indicates 16550A or better)
        let fifo_working = scratch_read == test_value;

        // Try to determine FIFO size by checking IIR bits
        // This is a heuristic - actual detection varies by chip
        let fifo_size = if fifo_working {
            // Enhanced UARTs (16650/16750) have larger FIFOs
            // For now, assume standard 16-byte FIFO for 16550A
            // Enhanced detection would require more sophisticated tests
            16
        } else {
            16 // 16550 has FIFO but it may be buggy
        };

        let uart_type = if fifo_working {
            UartType::Uart16550A
        } else {
            UartType::Uart16550
        };

        UartCapabilities {
            uart_type,
            has_fifo: true,
            fifo_working,
            fifo_size,
        }
    }

    /// Initialize UART with cross-compatible settings
    unsafe fn init(&mut self) {
        // Detect UART capabilities first
        self.caps = self.detect_uart();

        // Disable interrupts
        io::outb(self.base + REG_IER, 0x00);

        // Enable DLAB to access baud rate divisor
        io::outb(self.base + REG_LCR, LCR_DLAB);

        // Set baud rate divisor to 1 (115200 baud with 1.8432 MHz clock)
        // Low byte
        io::outb(self.base + REG_RBR, 0x01);
        // High byte
        io::outb(self.base + REG_IER, 0x00);

        // Configure line: 8 bits, no parity, 1 stop bit (8N1)
        io::outb(self.base + REG_LCR, 0x03);

        // Configure FIFO if available
        if self.caps.has_fifo {
            if self.caps.fifo_working {
                // Enable FIFO with 14-byte threshold (16550A and better)
                io::outb(
                    self.base + REG_IIR,
                    FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_14_BYTE_THRESHOLD,
                );
            } else {
                // 16550 - enable FIFO but be cautious
                io::outb(
                    self.base + REG_IIR,
                    FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX,
                );
            }
        }
        // If no FIFO (8250/16450), FCR write is ignored, so it's safe

        // Set modem control: DTR, RTS, and AUX2 (for loopback on some systems)
        io::outb(self.base + REG_MCR, MCR_DTR | MCR_RTS | MCR_AUX2);
    }

    /// Write a byte, compatible with all UART types
    fn write_byte(&mut self, byte: u8) {
        unsafe {
            // Wait for transmitter to be ready
            // LSR bit 5 (THRE - Transmit Holding Register Empty) must be set
            while (io::inb(self.base + REG_LSR) & LSR_TX_EMPTY) == 0 {
                core::hint::spin_loop();
            }
            io::outb(self.base + REG_RBR, byte);
        }
    }

    /// Get detected UART capabilities
    pub fn capabilities(&self) -> UartCapabilities {
        self.caps
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
