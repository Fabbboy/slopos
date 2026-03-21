use crate::io::Port;

pub const COM1: Port<u8> = Port::new(0x3F8);

pub const PIT_CHANNEL0: Port<u8> = Port::new(0x40);
pub const PIT_COMMAND: Port<u8> = Port::new(0x43);

pub const PS2_DATA: Port<u8> = Port::new(0x60);
pub const PS2_STATUS: Port<u8> = Port::new(0x64);
pub const PS2_COMMAND: Port<u8> = Port::new(0x64);

pub const QEMU_DEBUG_EXIT: Port<u8> = Port::new(0xF4);

pub const IO_DELAY: Port<u8> = Port::new(0x80);

pub const ACPI_PM1A_CNT: Port<u16> = Port::new(0x604);
pub const ACPI_PM1A_CNT_BOCHS: Port<u16> = Port::new(0xB004);
pub const ACPI_PM1A_CNT_VBOX: Port<u16> = Port::new(0x4004);

pub const UART_REG_RBR: u16 = 0;
pub const UART_REG_THR: u16 = 0;
pub const UART_REG_IER: u16 = 1;
pub const UART_REG_IIR: u16 = 2;
pub const UART_REG_FCR: u16 = 2;
pub const UART_REG_LCR: u16 = 3;
pub const UART_REG_MCR: u16 = 4;
pub const UART_REG_LSR: u16 = 5;
pub const UART_REG_MSR: u16 = 6;
pub const UART_REG_SCR: u16 = 7;

pub const UART_LCR_DLAB: u8 = 0x80;
pub const UART_IIR_FIFO_MASK: u8 = 0xC0;
pub const UART_IIR_FIFO_ENABLED: u8 = 0xC0;
pub const UART_FCR_ENABLE_FIFO: u8 = 0x01;
pub const UART_FCR_CLEAR_RX: u8 = 0x02;
pub const UART_FCR_CLEAR_TX: u8 = 0x04;
pub const UART_FCR_14_BYTE_THRESHOLD: u8 = 0xC0;
pub const UART_LSR_DATA_READY: u8 = 0x01;
pub const UART_LSR_TX_EMPTY: u8 = 0x20;
pub const UART_MCR_DTR: u8 = 0x01;
pub const UART_MCR_RTS: u8 = 0x02;
pub const UART_MCR_AUX2: u8 = 0x08;

pub const PIT_BASE_FREQUENCY_HZ: u32 = 1_193_182;

// ---------------------------------------------------------------------------
// Low-level serial I/O primitives
// ---------------------------------------------------------------------------
//
// These are the **single source of truth** for putting bytes on a UART.
// Every path that writes to a serial port — early-boot klog, the runtime
// klog backend, and the `SerialPort` driver — must funnel through here.
//
// The functions are intentionally lock-free: callers are responsible for
// serialisation (cli/sti, spinlock, IrqMutex, … whatever suits the context).

/// Write one byte to a UART, polling the Line Status Register until the
/// transmit holding register is empty.
///
/// # Safety
///
/// Port I/O.  Caller must ensure `base` refers to a valid, initialised
/// 8250/16550-compatible UART and that concurrent access is serialised.
#[inline(always)]
pub unsafe fn serial_putc(base: Port<u8>, byte: u8) {
    let lsr = base.offset(UART_REG_LSR);
    let thr = base.offset(UART_REG_THR);
    unsafe {
        while (lsr.read() & UART_LSR_TX_EMPTY) == 0 {
            core::hint::spin_loop();
        }
        thr.write(byte);
    }
}

/// Write a byte slice to a UART, converting lone `\n` into `\r\n`.
///
/// # Safety
///
/// Same requirements as [`serial_putc`].
#[inline]
pub unsafe fn serial_write_bytes(base: Port<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' {
            unsafe { serial_putc(base, b'\r') };
        }
        unsafe { serial_putc(base, b) };
    }
}
