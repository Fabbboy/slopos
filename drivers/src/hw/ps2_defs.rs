//! PS/2 controller port definitions.
//!
//! Shared by keyboard, mouse, and IRQ handlers.

// ============================================================================
// PS/2 Controller Ports
// ============================================================================

/// PS/2 data port - read keyboard/mouse data, write commands to device
pub(crate) const PS2_DATA_PORT: u16 = 0x60;
/// PS/2 status port (read) / command port (write)
pub(crate) const PS2_STATUS_PORT: u16 = 0x64;
/// PS/2 command port (alias for writes to status port)
pub(crate) const PS2_COMMAND_PORT: u16 = 0x64;
