//! 16550 UART serial port definitions.

// ============================================================================
// Standard COM Port Base Addresses
// ============================================================================

/// COM1 base address
pub const COM1_BASE: u16 = 0x3f8;
/// COM2 base address
pub const COM2_BASE: u16 = 0x2f8;
/// COM3 base address
pub const COM3_BASE: u16 = 0x3e8;
/// COM4 base address
pub const COM4_BASE: u16 = 0x2e8;

// ============================================================================
// UART Register Offsets (8250/16450/16550 family)
// ============================================================================

/// Receiver Buffer / Transmitter Holding Register
pub(crate) const REG_RBR: u16 = 0;
/// Interrupt Enable Register
pub(crate) const REG_IER: u16 = 1;
/// Interrupt Identification / FIFO Control Register
pub(crate) const REG_IIR: u16 = 2;
/// Line Control Register
pub(crate) const REG_LCR: u16 = 3;
/// Modem Control Register
pub(crate) const REG_MCR: u16 = 4;
/// Line Status Register
pub(crate) const REG_LSR: u16 = 5;
/// Scratch Register
pub(crate) const REG_SCR: u16 = 7;

// ============================================================================
// LCR (Line Control Register) Bits
// ============================================================================

/// Divisor Latch Access Bit
pub(crate) const LCR_DLAB: u8 = 0x80;

// ============================================================================
// IIR (Interrupt Identification Register) Bits
// ============================================================================

/// FIFO presence mask
pub(crate) const IIR_FIFO_MASK: u8 = 0xC0;
/// FIFO enabled indicator
pub(crate) const IIR_FIFO_ENABLED: u8 = 0xC0;

// ============================================================================
// FCR (FIFO Control Register) Bits
// ============================================================================

/// Enable FIFO
pub(crate) const FCR_ENABLE_FIFO: u8 = 0x01;
/// Clear receive FIFO
pub(crate) const FCR_CLEAR_RX: u8 = 0x02;
/// Clear transmit FIFO
pub(crate) const FCR_CLEAR_TX: u8 = 0x04;
/// 14-byte trigger threshold
pub(crate) const FCR_14_BYTE_THRESHOLD: u8 = 0xC0;

// ============================================================================
// LSR (Line Status Register) Bits
// ============================================================================

/// Data ready to read
pub(crate) const LSR_DATA_READY: u8 = 0x01;
/// Transmitter holding register empty
pub(crate) const LSR_TX_EMPTY: u8 = 0x20;

// ============================================================================
// MCR (Modem Control Register) Bits
// ============================================================================

/// Data Terminal Ready
pub(crate) const MCR_DTR: u8 = 0x01;
/// Request To Send
pub(crate) const MCR_RTS: u8 = 0x02;
/// Auxiliary output 2 (enables interrupts on some systems)
pub(crate) const MCR_AUX2: u8 = 0x08;
