mod pic;
pub mod pit;
pub mod port;
pub mod port_consts;
pub mod power;
pub mod ps2;
pub mod raw_port;
pub mod uart;

pub use pic::init_and_disable_legacy_8259;
pub use pit::Pit;
pub use port::{
    IoPort, IoPortError, IoPortRegistry, PortAccessible, PortRange, io_wait,
    register_io_port_registry,
};
pub use ps2::Ps2Regs;
pub use uart::UartRegs;

/// Terminate the VM through QEMU's `isa-debug-exit` device.
///
/// Safe: the port has exactly one effect, and it is to end the machine.
/// There is no memory it can corrupt and no state left to observe, so the
/// write carries no obligation for a caller to discharge. Does nothing on
/// hardware, where the port is unclaimed.
///
/// QEMU reports `(value << 1) | 1`, so 0 surfaces as exit code 1 and 1 as 3.
pub fn qemu_debug_exit(value: u8) {
    // SAFETY: writing 0xF4 is the documented `isa-debug-exit` protocol; on
    // real hardware the port is unclaimed and the write is discarded.
    unsafe { port_consts::QEMU_DEBUG_EXIT.write(value) };
}
