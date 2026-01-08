//! Hardware constants and definitions for SlopOS drivers.
//!
//! This module re-exports hardware definitions from `slopos_abi::arch::x86_64`
//! for backward compatibility. New code should import directly from abi.

// Re-export from abi for backward compatibility
pub use slopos_abi::arch::x86_64::ioapic as ioapic_defs;
pub use slopos_abi::arch::x86_64::ports as ports_defs;

// apic_defs includes both apic and cpuid constants (legacy layout)
pub mod apic_defs {
    //! Local APIC definitions - re-exported from abi.
    pub use slopos_abi::arch::x86_64::apic::*;
    pub use slopos_abi::arch::x86_64::cpuid::{CPUID_FEAT_EDX_APIC, CPUID_FEAT_ECX_X2APIC};
}

// pci_defs includes both pci and config port addresses (legacy layout)
pub mod pci_defs {
    //! PCI definitions - re-exported from abi.
    pub use slopos_abi::arch::x86_64::pci::*;
    pub use slopos_abi::arch::x86_64::ports::{PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA};
}

// Legacy module aliases for existing imports
pub mod pic_defs {
    //! Legacy PIC definitions - re-exported from ports.
    pub use slopos_abi::arch::x86_64::ports::{
        PIC1_COMMAND, PIC1_DATA, PIC2_COMMAND, PIC2_DATA, PIC_EOI,
    };
}

pub mod pit_defs {
    //! PIT definitions - re-exported from ports.
    pub use slopos_abi::arch::x86_64::ports::{
        PIT_BASE_FREQUENCY_HZ, PIT_CHANNEL0_PORT, PIT_COMMAND_ACCESS_LOHI,
        PIT_COMMAND_BINARY, PIT_COMMAND_CHANNEL0, PIT_COMMAND_MODE_SQUARE,
        PIT_COMMAND_PORT, PIT_DEFAULT_FREQUENCY_HZ, PIT_IRQ_LINE,
    };
}

pub mod ps2_defs {
    //! PS/2 definitions - re-exported from ports.
    pub use slopos_abi::arch::x86_64::ports::{
        PS2_COMMAND_PORT, PS2_DATA_PORT, PS2_STATUS_PORT,
    };
}

pub mod serial_defs {
    //! Serial port definitions - re-exported from ports.
    pub use slopos_abi::arch::x86_64::ports::{
        COM1_BASE, COM2_BASE, COM3_BASE, COM4_BASE,
        UART_FCR_14_BYTE_THRESHOLD, UART_FCR_CLEAR_RX, UART_FCR_CLEAR_TX,
        UART_FCR_ENABLE_FIFO, UART_IIR_FIFO_ENABLED, UART_IIR_FIFO_MASK,
        UART_LCR_DLAB, UART_LSR_DATA_READY, UART_LSR_TX_EMPTY,
        UART_MCR_AUX2, UART_MCR_DTR, UART_MCR_RTS,
        UART_REG_IER, UART_REG_IIR, UART_REG_LCR, UART_REG_LSR,
        UART_REG_MCR, UART_REG_RBR, UART_REG_SCR,
    };
    // Backward compat aliases for old names
    pub use slopos_abi::arch::x86_64::ports::UART_REG_RBR as REG_RBR;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_IER as REG_IER;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_IIR as REG_IIR;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_LCR as REG_LCR;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_MCR as REG_MCR;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_LSR as REG_LSR;
    pub use slopos_abi::arch::x86_64::ports::UART_REG_SCR as REG_SCR;
    pub use slopos_abi::arch::x86_64::ports::UART_LCR_DLAB as LCR_DLAB;
    pub use slopos_abi::arch::x86_64::ports::UART_IIR_FIFO_MASK as IIR_FIFO_MASK;
    pub use slopos_abi::arch::x86_64::ports::UART_IIR_FIFO_ENABLED as IIR_FIFO_ENABLED;
    pub use slopos_abi::arch::x86_64::ports::UART_FCR_ENABLE_FIFO as FCR_ENABLE_FIFO;
    pub use slopos_abi::arch::x86_64::ports::UART_FCR_CLEAR_RX as FCR_CLEAR_RX;
    pub use slopos_abi::arch::x86_64::ports::UART_FCR_CLEAR_TX as FCR_CLEAR_TX;
    pub use slopos_abi::arch::x86_64::ports::UART_FCR_14_BYTE_THRESHOLD as FCR_14_BYTE_THRESHOLD;
    pub use slopos_abi::arch::x86_64::ports::UART_LSR_DATA_READY as LSR_DATA_READY;
    pub use slopos_abi::arch::x86_64::ports::UART_LSR_TX_EMPTY as LSR_TX_EMPTY;
    pub use slopos_abi::arch::x86_64::ports::UART_MCR_DTR as MCR_DTR;
    pub use slopos_abi::arch::x86_64::ports::UART_MCR_RTS as MCR_RTS;
    pub use slopos_abi::arch::x86_64::ports::UART_MCR_AUX2 as MCR_AUX2;
}
