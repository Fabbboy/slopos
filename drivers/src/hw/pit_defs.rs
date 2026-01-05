//! 8254 PIT (Programmable Interval Timer) definitions.

// ============================================================================
// Frequency Constants
// ============================================================================

/// PIT base oscillator frequency (Hz)
pub const PIT_BASE_FREQUENCY_HZ: u32 = 1_193_182;
/// Default timer frequency (Hz)
pub const PIT_DEFAULT_FREQUENCY_HZ: u32 = 100;

// ============================================================================
// PIT I/O Ports
// ============================================================================

/// Channel 0 data port
pub(crate) const PIT_CHANNEL0_PORT: u16 = 0x40;
/// Command/mode register port
pub(crate) const PIT_COMMAND_PORT: u16 = 0x43;

// ============================================================================
// Command Register Bits
// ============================================================================

/// Select channel 0
pub(crate) const PIT_COMMAND_CHANNEL0: u8 = 0x00;
/// Access mode: low byte then high byte
pub(crate) const PIT_COMMAND_ACCESS_LOHI: u8 = 0x30;
/// Operating mode: square wave generator
pub(crate) const PIT_COMMAND_MODE_SQUARE: u8 = 0x06;
/// Binary counting mode
pub(crate) const PIT_COMMAND_BINARY: u8 = 0x00;

// ============================================================================
// IRQ Assignment
// ============================================================================

/// PIT is connected to legacy IRQ 0
pub(crate) const PIT_IRQ_LINE: u8 = 0;
