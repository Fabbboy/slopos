use core::fmt::{self, Write};
use spin::Mutex;
use slopos_lib::io;

// Standard COM port base addresses
pub const COM1_BASE: u16 = 0x3f8;
pub const COM2_BASE: u16 = 0x2f8;
pub const COM3_BASE: u16 = 0x3e8;
pub const COM4_BASE: u16 = 0x2e8;

// UART register offsets (standard across 8250/16450/16550 family)
const REG_RBR: u16 = 0; // Receiver Buffer Register (read) / Transmitter Holding Register (write)
const REG_IER: u16 = 1; // Interrupt Enable Register
const REG_IIR: u16 = 2; // Interrupt Identification Register (read) / FIFO Control Register (write)
const REG_LCR: u16 = 3; // Line Control Register
const REG_MCR: u16 = 4; // Modem Control Register
const REG_LSR: u16 = 5; // Line Status Register
#[allow(dead_code)]
const REG_MSR: u16 = 6; // Modem Status Register
const REG_SCR: u16 = 7; // Scratch Register

// LCR bits
const LCR_DLAB: u8 = 0x80; // Divisor Latch Access Bit

// IIR bits (for UART type detection)
const IIR_FIFO_MASK: u8 = 0xC0;
const IIR_FIFO_ENABLED: u8 = 0xC0;

// FCR bits (FIFO Control Register)
const FCR_ENABLE_FIFO: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;
const FCR_CLEAR_TX: u8 = 0x04;
const FCR_14_BYTE_THRESHOLD: u8 = 0xC0;

// LSR bits
const LSR_DATA_READY: u8 = 0x01;
const LSR_TX_EMPTY: u8 = 0x20;

// MCR bits
const MCR_DTR: u8 = 0x01;
const MCR_RTS: u8 = 0x02;
const MCR_AUX2: u8 = 0x08;

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

/// Initialize the default serial port (COM1)
pub fn init() {
    let mut port = SERIAL.lock();
    unsafe { port.init(); }
}

/// Initialize a specific serial port by base address
pub fn init_port(base: u16) -> Result<UartCapabilities, ()> {
    // For now, we only support the default port
    // Future enhancement: support multiple ports
    if base == COM1_BASE {
        let mut port = SERIAL.lock();
        unsafe { port.init(); }
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

pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = SERIAL.lock().write_fmt(args);
}

#[unsafe(no_mangle)]
pub fn serial_poll_receive(port: u16) {
    // Poll the UART Line Status Register for data ready.
    // This works with all UART types (8250/16450/16550 family)
    while unsafe { io::inb(port + REG_LSR) } & LSR_DATA_READY != 0 {
        let byte = unsafe { io::inb(port + REG_RBR) };
        let mut buf = INPUT_BUFFER.lock();
        buf.push(byte);
    }
}

#[unsafe(no_mangle)]
pub fn serial_buffer_pending(port: u16) -> i32 {
    // Ensure we service the port before reporting availability.
    serial_poll_receive(port);
    let buf = INPUT_BUFFER.lock();
    (!buf.is_empty()) as i32
}

#[unsafe(no_mangle)]
pub fn serial_buffer_read(port: u16, out: *mut u8) -> i32 {
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
        io::outb(self.base + REG_IIR, FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);
        
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
                io::outb(self.base + REG_IIR, 
                    FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_14_BYTE_THRESHOLD);
            } else {
                // 16550 - enable FIFO but be cautious
                io::outb(self.base + REG_IIR, 
                    FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);
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
    
    /// Read a byte if available (non-blocking)
    #[allow(dead_code)]
    fn read_byte(&self) -> Option<u8> {
        unsafe {
            // Check if data is ready
            if (io::inb(self.base + REG_LSR) & LSR_DATA_READY) != 0 {
                Some(io::inb(self.base + REG_RBR))
            } else {
                None
            }
        }
    }
    
    /// Get detected UART capabilities
    pub fn capabilities(&self) -> UartCapabilities {
        self.caps
    }
    
    /// Get UART base address
    #[allow(dead_code)]
    pub fn base(&self) -> u16 {
        self.base
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

