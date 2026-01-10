//! x86 I/O port addresses.
//!
//! This module provides a type-safe `Port` newtype that consolidates all
//! known I/O port addresses used by SlopOS, preventing accidentally using
//! other u16 values as port numbers.

/// x86 I/O port address.
///
/// Ports are accessed via IN/OUT instructions. This newtype groups all
/// known port addresses and prevents accidentally using other u16 values.
///
/// # Example
///
/// ```ignore
/// use slopos_abi::arch::x86_64::ports::Port;
///
/// unsafe { outb(Port::COM1.number(), b'A'); }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Port(pub u16);

impl Port {
    // =========================================================================
    // Serial (8250/16550 UART)
    // =========================================================================

    /// COM1 serial port base address.
    pub const COM1: Self = Self(0x3F8);

    /// COM2 serial port base address.
    pub const COM2: Self = Self(0x2F8);

    /// COM3 serial port base address.
    pub const COM3: Self = Self(0x3E8);

    /// COM4 serial port base address.
    pub const COM4: Self = Self(0x2E8);

    // =========================================================================
    // Programmable Interval Timer (8254 PIT)
    // =========================================================================

    /// PIT Channel 0 data port.
    pub const PIT_CHANNEL0: Self = Self(0x40);

    /// PIT Channel 1 data port.
    pub const PIT_CHANNEL1: Self = Self(0x41);

    /// PIT Channel 2 data port.
    pub const PIT_CHANNEL2: Self = Self(0x42);

    /// PIT Command/mode register port.
    pub const PIT_COMMAND: Self = Self(0x43);

    // =========================================================================
    // PS/2 Controller (8042)
    // =========================================================================

    /// PS/2 data port - read keyboard/mouse data, write commands to device.
    pub const PS2_DATA: Self = Self(0x60);

    /// PS/2 status port (read) / command port (write).
    pub const PS2_STATUS: Self = Self(0x64);

    /// PS/2 command port (alias for writes to status port).
    pub const PS2_COMMAND: Self = Self(0x64);

    // =========================================================================
    // Legacy PIC (8259)
    // =========================================================================

    /// Master PIC command port.
    pub const PIC1_COMMAND: Self = Self(0x20);

    /// Master PIC data port.
    pub const PIC1_DATA: Self = Self(0x21);

    /// Slave PIC command port.
    pub const PIC2_COMMAND: Self = Self(0xA0);

    /// Slave PIC data port.
    pub const PIC2_DATA: Self = Self(0xA1);

    // =========================================================================
    // PCI Configuration (Type 1)
    // =========================================================================

    /// PCI configuration address port.
    pub const PCI_CONFIG_ADDRESS: Self = Self(0xCF8);

    /// PCI configuration data port.
    pub const PCI_CONFIG_DATA: Self = Self(0xCFC);

    // =========================================================================
    // CMOS/RTC
    // =========================================================================

    /// CMOS address port.
    pub const CMOS_ADDRESS: Self = Self(0x70);

    /// CMOS data port.
    pub const CMOS_DATA: Self = Self(0x71);

    // =========================================================================
    // Debug Ports
    // =========================================================================

    /// QEMU debug exit port.
    pub const QEMU_DEBUG_EXIT: Self = Self(0xF4);

    /// Bochs debug port.
    pub const BOCHS_DEBUG: Self = Self(0xE9);

    // =========================================================================
    // Methods
    // =========================================================================

    /// Get the raw port number for IN/OUT instructions.
    #[inline]
    pub const fn number(self) -> u16 {
        self.0
    }

    /// Create an offset port (e.g., COM1 + register offset).
    #[inline]
    pub const fn offset(self, off: u16) -> Self {
        Self(self.0 + off)
    }

    /// Create a new port from a raw address.
    #[inline]
    pub const fn new(addr: u16) -> Self {
        Self(addr)
    }
}

// =============================================================================
// UART Register Offsets (relative to COMx base)
// =============================================================================

/// Receiver Buffer Register (read) / Transmitter Holding Register (write).
pub const UART_REG_RBR: u16 = 0;
/// Transmitter Holding Register (same offset as RBR, write only).
pub const UART_REG_THR: u16 = 0;
/// Interrupt Enable Register.
pub const UART_REG_IER: u16 = 1;
/// Interrupt Identification Register (read).
pub const UART_REG_IIR: u16 = 2;
/// FIFO Control Register (write).
pub const UART_REG_FCR: u16 = 2;
/// Line Control Register.
pub const UART_REG_LCR: u16 = 3;
/// Modem Control Register.
pub const UART_REG_MCR: u16 = 4;
/// Line Status Register.
pub const UART_REG_LSR: u16 = 5;
/// Modem Status Register.
pub const UART_REG_MSR: u16 = 6;
/// Scratch Register.
pub const UART_REG_SCR: u16 = 7;

// =============================================================================
// UART Control Bits
// =============================================================================

/// Divisor Latch Access Bit (LCR).
pub const UART_LCR_DLAB: u8 = 0x80;

/// FIFO presence mask (IIR).
pub const UART_IIR_FIFO_MASK: u8 = 0xC0;
/// FIFO enabled indicator (IIR).
pub const UART_IIR_FIFO_ENABLED: u8 = 0xC0;

/// Enable FIFO (FCR).
pub const UART_FCR_ENABLE_FIFO: u8 = 0x01;
/// Clear receive FIFO (FCR).
pub const UART_FCR_CLEAR_RX: u8 = 0x02;
/// Clear transmit FIFO (FCR).
pub const UART_FCR_CLEAR_TX: u8 = 0x04;
/// 14-byte trigger threshold (FCR).
pub const UART_FCR_14_BYTE_THRESHOLD: u8 = 0xC0;

/// Data ready to read (LSR).
pub const UART_LSR_DATA_READY: u8 = 0x01;
/// Transmitter holding register empty (LSR).
pub const UART_LSR_TX_EMPTY: u8 = 0x20;

/// Data Terminal Ready (MCR).
pub const UART_MCR_DTR: u8 = 0x01;
/// Request To Send (MCR).
pub const UART_MCR_RTS: u8 = 0x02;
/// Auxiliary output 2 - enables interrupts on some systems (MCR).
pub const UART_MCR_AUX2: u8 = 0x08;

// =============================================================================
// PIT Constants
// =============================================================================

/// PIT base oscillator frequency (Hz).
pub const PIT_BASE_FREQUENCY_HZ: u32 = 1_193_182;

/// Default timer frequency (Hz).
pub const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

/// Select channel 0 (PIT command).
pub const PIT_COMMAND_CHANNEL0: u8 = 0x00;

/// Access mode: low byte then high byte (PIT command).
pub const PIT_COMMAND_ACCESS_LOHI: u8 = 0x30;

/// Operating mode: square wave generator (PIT command).
pub const PIT_COMMAND_MODE_SQUARE: u8 = 0x06;

/// Binary counting mode (PIT command).
pub const PIT_COMMAND_BINARY: u8 = 0x00;

/// PIT is connected to legacy IRQ 0.
pub const PIT_IRQ_LINE: u8 = 0;

// =============================================================================
// PIC Constants
// =============================================================================

/// End of Interrupt command.
pub const PIC_EOI: u8 = 0x20;

// =============================================================================
// Raw Port Address Constants
// =============================================================================

pub const COM1_BASE: u16 = Port::COM1.0;
pub const PIT_CHANNEL0_PORT: u16 = Port::PIT_CHANNEL0.0;
pub const PIT_COMMAND_PORT: u16 = Port::PIT_COMMAND.0;
pub const PS2_DATA_PORT: u16 = Port::PS2_DATA.0;
pub const PS2_STATUS_PORT: u16 = Port::PS2_STATUS.0;
pub const PS2_COMMAND_PORT: u16 = Port::PS2_COMMAND.0;
pub const PIC1_COMMAND: u16 = Port::PIC1_COMMAND.0;
pub const PIC1_DATA: u16 = Port::PIC1_DATA.0;
pub const PIC2_COMMAND: u16 = Port::PIC2_COMMAND.0;
pub const PIC2_DATA: u16 = Port::PIC2_DATA.0;
pub const PCI_CONFIG_ADDRESS: u16 = Port::PCI_CONFIG_ADDRESS.0;
pub const PCI_CONFIG_DATA: u16 = Port::PCI_CONFIG_DATA.0;
