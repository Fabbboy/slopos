//! Static platform tables OSTD ingests at boot: reserved IRQ vectors, legacy
//! I/O port ranges, and MMIO ranges whose phys addresses are known that early
//! (currently just the LAPIC).
//!
//! Runtime-discovered MMIO (HPET, IOAPIC, PCI ECAM, device BARs, framebuffer)
//! is absent by design: `slopos_mm::mmio::MmioRegionExt::map` registers those
//! with OSTD's dynamic-range registry at the moment a driver maps them.

use slopos_abi::addr::PhysAddr;
use slopos_arch::arch::idt::{
    LAPIC_TIMER_VECTOR, RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR, SHUTDOWN_VECTOR, SYSCALL_VECTOR,
    TLB_SHOOTDOWN_VECTOR,
};
use slopos_ostd::io::port::PortRange;
use slopos_ostd::mm::io_mem::PhysRange;

const SPURIOUS_VECTOR: u8 = 0xFF;

pub static MMIO_RANGES: &[PhysRange] = &[PhysRange {
    base: PhysAddr(0xFEE0_0000),
    len: 0x1000,
}];

pub static PORT_RANGES: &[PortRange] = &[
    PortRange {
        start: 0x40,
        end: 0x44,
    },
    PortRange {
        start: 0x60,
        end: 0x65,
    },
    PortRange {
        start: 0x70,
        end: 0x72,
    },
    PortRange {
        start: 0x501,
        end: 0x502,
    },
    PortRange {
        start: 0x3F8,
        end: 0x400,
    },
    // ACPI PM1A_CNT (16-bit): standard 0x604, Bochs/Qemu fallback 0xB004,
    // VirtualBox quirk 0x4004.
    PortRange {
        start: 0x604,
        end: 0x606,
    },
    PortRange {
        start: 0xB004,
        end: 0xB006,
    },
    PortRange {
        start: 0x4004,
        end: 0x4006,
    },
    // PCH reset-control register (RST_CNT) at 0xCF9: the architecturally fixed
    // x86 reset port, wired to the platform RESET#. Written only by the reboot
    // path, alongside the firmware-described FADT RESET_REG.
    PortRange {
        start: 0xCF9,
        end: 0xCFA,
    },
];

pub static RESERVED_VECTORS: &[u8] = &[
    SYSCALL_VECTOR,
    RCU_QS_IPI_VECTOR,
    RESCHEDULE_IPI_VECTOR,
    TLB_SHOOTDOWN_VECTOR,
    LAPIC_TIMER_VECTOR,
    SHUTDOWN_VECTOR,
    SPURIOUS_VECTOR,
];
