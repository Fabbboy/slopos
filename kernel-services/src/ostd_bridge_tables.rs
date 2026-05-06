//! Static platform tables that OSTD ingests at boot via its
//! `register_*` hooks: reserved IRQ vectors, legacy I/O port ranges,
//! and architecturally-fixed MMIO ranges.
//!
//! Runtime-discovered MMIO regions (HPET, IOAPIC, PCI ECAM, device
//! BARs, framebuffer) are intentionally absent here. They are added
//! to OSTD's heap-free dynamic-range secondary registry at the moment
//! a driver tries to map them: `slopos_mm::mmio::MmioRegionExt::map`
//! calls `slopos_ostd::mm::io_mem::register_io_mem_range` before
//! `IoMemRegistry::reserve`, so the static slice below only needs to
//! cover ranges whose phys addresses are already known at boot
//! (currently just the LAPIC).

use slopos_abi::addr::PhysAddr;
use slopos_arch::arch::idt::{
    LAPIC_TIMER_VECTOR, LUF_DRAIN_IPI_VECTOR, RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR,
    SYSCALL_VECTOR, TLB_SHOOTDOWN_VECTOR,
};
use slopos_ostd::io::port::PortRange;
use slopos_ostd::mm::io_mem::PhysRange;

const SHUTDOWN_VECTOR: u8 = 0xFE;
const SPURIOUS_VECTOR: u8 = 0xFF;

pub static MMIO_RANGES: &[PhysRange] = &[PhysRange {
    base: PhysAddr(0xFEE0_0000),
    len: 0x1000,
}];

pub static PORT_RANGES: &[PortRange] = &[
    PortRange {
        start: 0x20,
        end: 0x21,
    },
    PortRange {
        start: 0x40,
        end: 0x43,
    },
    PortRange {
        start: 0x70,
        end: 0x71,
    },
    PortRange {
        start: 0xA0,
        end: 0xA1,
    },
    PortRange {
        start: 0x501,
        end: 0x501,
    },
    PortRange {
        start: 0x3F8,
        end: 0x3FF,
    },
];

pub static RESERVED_VECTORS: &[u8] = &[
    SYSCALL_VECTOR,
    LUF_DRAIN_IPI_VECTOR,
    RCU_QS_IPI_VECTOR,
    RESCHEDULE_IPI_VECTOR,
    TLB_SHOOTDOWN_VECTOR,
    LAPIC_TIMER_VECTOR,
    SHUTDOWN_VECTOR,
    SPURIOUS_VECTOR,
];
