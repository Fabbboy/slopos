//! Well-known x86 I/O port constants.
//!
//! These describe stable hardware port addresses (UART, PIT, PS/2, QEMU,
//! ACPI PM1A) and the bit layouts of their associated registers. The
//! [`Port<T>`](super::raw_port::Port) handles are non-registry-gated so
//! they remain reachable in early-boot contexts before
//! [`register_io_port_registry`](super::port::register_io_port_registry)
//! has run.

use super::raw_port::Port;

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
