use core::fmt::{self, Write};
use slopos_lib::RingBuffer;
use slopos_lib::io::Port;
use slopos_lib::ports::{
    COM1, UART_FCR_14_BYTE_THRESHOLD as FCR_14_BYTE_THRESHOLD, UART_FCR_CLEAR_RX as FCR_CLEAR_RX,
    UART_FCR_CLEAR_TX as FCR_CLEAR_TX, UART_FCR_ENABLE_FIFO as FCR_ENABLE_FIFO,
    UART_IIR_FIFO_ENABLED as IIR_FIFO_ENABLED, UART_IIR_FIFO_MASK as IIR_FIFO_MASK,
    UART_LCR_DLAB as LCR_DLAB, UART_LSR_DATA_READY as LSR_DATA_READY,
    UART_LSR_TX_EMPTY as LSR_TX_EMPTY, UART_MCR_AUX2 as MCR_AUX2, UART_MCR_DTR as MCR_DTR,
    UART_MCR_RTS as MCR_RTS, UART_REG_IER as REG_IER, UART_REG_IIR as REG_IIR,
    UART_REG_LCR as REG_LCR, UART_REG_LSR as REG_LSR, UART_REG_MCR as REG_MCR,
    UART_REG_RBR as REG_RBR, UART_REG_SCR as REG_SCR,
};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartType {
    Uart8250,
    Uart16450,
    Uart16550,
    Uart16550A,
    Uart16650,
    Uart16750,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct UartCapabilities {
    pub uart_type: UartType,
    pub has_fifo: bool,
    pub fifo_working: bool,
    pub fifo_size: usize,
}

static SERIAL: Mutex<SerialPort> = Mutex::new(SerialPort::new(COM1));
const BUF_SIZE: usize = 256;

type SerialBuffer = RingBuffer<u8, BUF_SIZE>;

static INPUT_BUFFER: Mutex<SerialBuffer> = Mutex::new(SerialBuffer::new_with(0));

pub fn init() {
    let mut port = SERIAL.lock();
    unsafe { port.init() }
}

pub fn init_port(base: u16) -> Result<UartCapabilities, ()> {
    if base == COM1.address() {
        let mut port = SERIAL.lock();
        unsafe { port.init() }
        Ok(port.capabilities())
    } else {
        Err(())
    }
}

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

pub fn serial_putc_com1(ch: u8) {
    SERIAL.lock().write_byte(ch);
}

pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = SERIAL.lock().write_fmt(args);
}

pub fn serial_poll_receive(base: u16) {
    let port = Port::<u8>::new(base);
    let lsr = port.offset(REG_LSR);
    let rbr = port.offset(REG_RBR);
    while unsafe { lsr.read() } & LSR_DATA_READY != 0 {
        let byte = unsafe { rbr.read() };
        let mut buf = INPUT_BUFFER.lock();
        let _ = buf.try_push(byte);
    }
}

pub fn serial_buffer_pending(port: u16) -> i32 {
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
    base: Port<u8>,
    caps: UartCapabilities,
}

impl SerialPort {
    const fn new(base: Port<u8>) -> Self {
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

    #[inline]
    fn reg(&self, offset: u16) -> Port<u8> {
        self.base.offset(offset)
    }

    unsafe fn detect_uart(&mut self) -> UartCapabilities {
        self.reg(REG_IIR)
            .write(FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);

        for _ in 0..10 {
            core::hint::spin_loop();
        }

        let iir_after = self.reg(REG_IIR).read();
        let has_fifo = (iir_after & IIR_FIFO_MASK) == IIR_FIFO_ENABLED;

        self.reg(REG_IIR).write(0);

        if !has_fifo {
            return UartCapabilities {
                uart_type: UartType::Uart16450,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            };
        }

        let test_value = 0xAA;
        self.reg(REG_SCR).write(test_value);
        let scratch_read = self.reg(REG_SCR).read();
        let fifo_working = scratch_read == test_value;

        let fifo_size = 16;
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

    unsafe fn init(&mut self) {
        self.caps = self.detect_uart();

        self.reg(REG_IER).write(0x00);
        self.reg(REG_LCR).write(LCR_DLAB);
        self.reg(REG_RBR).write(0x01);
        self.reg(REG_IER).write(0x00);
        self.reg(REG_LCR).write(0x03);

        if self.caps.has_fifo {
            if self.caps.fifo_working {
                self.reg(REG_IIR)
                    .write(FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_14_BYTE_THRESHOLD);
            } else {
                self.reg(REG_IIR)
                    .write(FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);
            }
        }

        self.reg(REG_MCR).write(MCR_DTR | MCR_RTS | MCR_AUX2);
    }

    fn write_byte(&mut self, byte: u8) {
        unsafe {
            while (self.reg(REG_LSR).read() & LSR_TX_EMPTY) == 0 {
                core::hint::spin_loop();
            }
            self.reg(REG_RBR).write(byte);
        }
    }

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
