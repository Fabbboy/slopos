# Architecture Constants Consolidation

> **Status**: Proposed
> **Author**: Claude (AI-assisted design)
> **Date**: 2026-01-07
> **Scope**: Cross-crate refactoring of x86_64 architecture constants

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Problem Statement](#problem-statement)
3. [Current State Analysis](#current-state-analysis)
4. [Design Goals](#design-goals)
5. [Proposed Architecture](#proposed-architecture)
6. [Rust Type-Safety Patterns](#rust-type-safety-patterns)
7. [Detailed Module Specifications](#detailed-module-specifications)
8. [Migration Plan](#migration-plan)
9. [File Inventory](#file-inventory)
10. [Backward Compatibility](#backward-compatibility)
11. [Testing Strategy](#testing-strategy)
12. [Future Considerations](#future-considerations)

---

## Executive Summary

This document proposes consolidating all x86_64 architecture-specific constants into a unified `abi/src/arch/x86_64/` module hierarchy. The refactoring eliminates duplicate definitions, resolves circular dependency issues, and leverages Rust's type system to provide compile-time safety that C kernels cannot achieve.

**Key outcomes:**
- Single source of truth for ~500 architecture constants
- Type-safe newtypes preventing misuse of MSR addresses, port numbers, selectors
- Bitflags for page table entries, APIC flags, IOAPIC redirection entries
- Clean dependency graph: `abi` → all other crates (no cycles)
- Foundation for future multi-architecture support (aarch64, riscv64)

---

## Problem Statement

### Current Issues

1. **Duplication**: Constants like `APIC_BASE_ADDR_MASK` are defined in multiple places:
   - `drivers/src/hw/apic_defs.rs:30` → `0xFFFF_F000`
   - `mm/src/memory_init.rs:27` → `0xFFFFF000`

2. **Circular Dependency Risk**: Memory management (`mm`) needs APIC constants for early initialization, but `drivers` already depends on `mm`. Moving APIC constants to drivers would create a cycle.

3. **No Type Safety**: Raw `u32`/`u64` constants can be accidentally misused:
   ```rust
   // Current: nothing prevents this bug
   let port = MSR_APIC_BASE;  // Oops, used MSR address as port number
   outb(port as u16, value);  // Silent corruption
   ```

4. **Scattered Definitions**: Architecture constants spread across 7+ files in 3+ crates:
   - `drivers/src/hw/*.rs` (7 files)
   - `mm/src/mm_constants.rs`
   - `boot/src/gdt.rs`, `boot/src/idt.rs`
   - `abi/src/arch.rs` (only 4 constants)

5. **Unused Tooling**: `bitflags` crate is in workspace dependencies but never used.

### How Linux Solves This

The Linux kernel has a clear layered architecture:

```
arch/x86/include/asm/           ← Architecture-specific definitions
├── apic.h                      ← APIC registers, MSRs
├── pgtable_types.h             ← Page table flags
├── segment.h                   ← GDT selectors
├── irq_vectors.h               ← IDT vectors
└── msr-index.h                 ← MSR addresses

include/linux/                  ← Cross-subsystem generic types
├── types.h
└── ...

drivers/, mm/, kernel/          ← Subsystems consume arch headers
```

**Key insight**: Architecture definitions live in their own layer, not scattered across subsystems. Both `mm/` and `drivers/` include from `arch/x86/include/asm/`.

---

## Current State Analysis

### Crate Dependency Graph

```
                    ┌─────────┐
                    │   abi   │  ← Foundation (no internal deps)
                    └────┬────┘
                         │
         ┌───────────────┼───────────────┐
         │               │               │
         ▼               ▼               ▼
    ┌────────┐      ┌────────┐      ┌────────┐
    │  lib   │      │   mm   │      │   fs   │
    └────┬───┘      └────┬───┘      └────┬───┘
         │               │               │
         └───────┬───────┴───────┬───────┘
                 │               │
                 ▼               ▼
            ┌─────────┐    ┌─────────┐
            │ drivers │    │  sched  │
            └────┬────┘    └────┬────┘
                 │               │
                 └───────┬───────┘
                         │
                    ┌────┴────┐
                    │  boot   │
                    │  video  │
                    │  tests  │
                    └─────────┘
```

**Critical observation**: `abi` is the foundation crate with no internal dependencies. All other crates depend on it (except `fs`, `userland` which are minimal). This makes `abi` the ideal location for architecture constants.

### Constants Inventory by Location

| Location | Category | Count | Examples |
|----------|----------|-------|----------|
| `drivers/src/hw/apic_defs.rs` | Local APIC | ~35 | `LAPIC_ID`, `LAPIC_EOI`, `MSR_APIC_BASE` |
| `drivers/src/hw/ioapic_defs.rs` | I/O APIC | ~30 | `IOAPIC_REG_VER`, delivery modes |
| `drivers/src/hw/pci_defs.rs` | PCI bus | ~40 | `PCI_CONFIG_ADDRESS`, BAR flags |
| `drivers/src/hw/pit_defs.rs` | Timer | ~15 | `PIT_CHANNEL0_PORT`, frequencies |
| `drivers/src/hw/ps2_defs.rs` | PS/2 | ~5 | `PS2_DATA_PORT`, `PS2_STATUS_PORT` |
| `drivers/src/hw/pic_defs.rs` | Legacy PIC | ~10 | `PIC1_COMMAND`, `PIC_EOI` |
| `drivers/src/hw/serial_defs.rs` | UART | ~25 | `COM1_BASE`, UART registers |
| `mm/src/mm_constants.rs` | Paging | ~40 | `PAGE_PRESENT`, `KERNEL_VIRTUAL_BASE` |
| `boot/src/gdt.rs` | GDT | ~15 | `GDT_CODE_SELECTOR`, descriptors |
| `boot/src/idt.rs` | IDT | ~25 | `EXCEPTION_*`, `IDT_GATE_*` |
| `abi/src/arch.rs` | Mixed | 4 | `GDT_USER_CODE_SELECTOR`, `SYSCALL_VECTOR` |

**Total**: ~250 unique constants, with ~20 duplicates across files.

### Identified Duplicates

| Constant | Value | Locations |
|----------|-------|-----------|
| `APIC_BASE_ADDR_MASK` | `0xFFFF_F000` | `drivers/hw/apic_defs.rs`, `mm/memory_init.rs` |
| `COM1_BASE` | `0x3F8` | `drivers/hw/serial_defs.rs`, `lib/klog.rs` |
| `PAGE_SIZE_4KB` | `0x1000` | `mm/mm_constants.rs`, `mm/memory_reservations.rs`, `mm/phys_virt.rs`, `drivers/virtio_gpu.rs` |
| `KERNEL_VIRTUAL_BASE` | `0xFFFF_FFFF_8000_0000` | `mm/mm_constants.rs`, `mm/memory_reservations.rs`, `boot/boot_memory.rs` |
| `IRQ_BASE_VECTOR` | `32` | `boot/idt.rs`, `drivers/irq.rs` |
| `SYSCALL_VECTOR` | `0x80` | `boot/idt.rs`, `sched/test_tasks.rs` |
| `GDT_USER_*_SELECTOR` | `0x23`/`0x1B` | `abi/arch.rs`, `drivers/syscall.rs`, `sched/test_tasks.rs` |
| `MAX_PROCESSES` | `256` | `mm/mm_constants.rs`, `fs/fileio.rs` |
| `PROCESS_CODE_START_VA` | `0x40_0000` | `mm/mm_constants.rs`, `sched/task.rs` (as `USER_CODE_BASE`) |

---

## Design Goals

### Primary Goals

1. **Single Source of Truth**: Every architecture constant defined exactly once
2. **Type Safety**: Newtypes and bitflags prevent misuse at compile time
3. **Clean Dependencies**: No circular dependency risks
4. **Idiomatic Rust**: Leverage language features C kernels can't use
5. **Self-Documenting**: Types and associated constants explain themselves

### Secondary Goals

1. **IDE Support**: Better autocomplete, go-to-definition
2. **Future-Proof**: Easy to add `aarch64/`, `riscv64/` modules
3. **Backward Compatible**: Gradual migration via re-exports
4. **Minimal Runtime Cost**: `#[repr(transparent)]` newtypes are zero-cost

### Non-Goals

1. Runtime architecture detection (compile-time target only)
2. Complete x86 ISA coverage (only what SlopOS uses)
3. Abstracting away architecture differences (explicit is better)

---

## Proposed Architecture

### Module Hierarchy

```
abi/src/
├── lib.rs                      # Crate root, pub mod declarations
├── arch.rs                     # Top-level: #[cfg] arch detection, re-exports
│
└── arch/
    ├── mod.rs                  # Module root, cfg-based arch selection
    │
    └── x86_64/
        ├── mod.rs              # Re-exports all submodules
        │
        ├── msr.rs              # Model-Specific Registers
        │   └── Msr newtype, MSR addresses
        │
        ├── apic.rs             # Local APIC
        │   ├── ApicBaseMsr newtype
        │   ├── Register offsets (LAPIC_*)
        │   └── ApicFlags bitflags
        │
        ├── ioapic.rs           # I/O APIC
        │   ├── Register offsets
        │   ├── IoApicFlags bitflags
        │   └── MADT entry types
        │
        ├── paging.rs           # Page Tables
        │   ├── PageFlags bitflags
        │   ├── Page sizes (4KB, 2MB, 1GB)
        │   └── PTE address mask
        │
        ├── memory.rs           # Memory Layout
        │   ├── Kernel virtual base, HHDM
        │   ├── User space ranges
        │   └── Process memory layout
        │
        ├── gdt.rs              # Global Descriptor Table
        │   ├── SegmentSelector newtype
        │   ├── Descriptor flags
        │   └── GDT layout constants
        │
        ├── idt.rs              # Interrupt Descriptor Table
        │   ├── Exception vectors (0-31)
        │   ├── IRQ base vector
        │   ├── Syscall vector
        │   └── Gate type flags
        │
        ├── ports.rs            # I/O Ports
        │   ├── Port newtype
        │   └── All port addresses (serial, PIT, PS/2, PIC, PCI)
        │
        ├── pci.rs              # PCI Configuration
        │   ├── Config space registers
        │   ├── Header types
        │   └── Known vendor/device IDs
        │
        └── cpuid.rs            # CPU Feature Detection
            └── Feature flag constants
```

### Import Patterns

**For consumers:**
```rust
// Specific imports (preferred)
use slopos_abi::arch::x86_64::{Msr, PageFlags, SegmentSelector};
use slopos_abi::arch::x86_64::apic::ApicBaseMsr;
use slopos_abi::arch::x86_64::ports::Port;

// Glob import for heavy usage
use slopos_abi::arch::x86_64::paging::*;
```

**Top-level re-export in abi/src/arch.rs:**
```rust
// Automatic architecture selection
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

// Future: #[cfg(target_arch = "aarch64")] pub mod aarch64;
```

---

## Rust Type-Safety Patterns

### Pattern 1: Newtype for Addresses/Indices

**Problem**: MSR addresses, port numbers, and selector values are all `u16`/`u32` but semantically different.

**Solution**: Newtypes with `#[repr(transparent)]` for zero-cost abstraction.

```rust
/// Model-Specific Register address.
///
/// MSRs are accessed via RDMSR/WRMSR instructions using a 32-bit address.
/// This newtype prevents accidentally using an MSR address where a port
/// number or other value is expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Msr(pub u32);

impl Msr {
    // IA32 MSRs (0x00 - 0x1FF)
    pub const APIC_BASE: Self = Self(0x1B);
    pub const MTRR_CAP: Self = Self(0xFE);
    pub const SYSENTER_CS: Self = Self(0x174);
    pub const SYSENTER_ESP: Self = Self(0x175);
    pub const SYSENTER_EIP: Self = Self(0x176);
    pub const PAT: Self = Self(0x277);

    // AMD64/Intel 64 MSRs (0xC000_0000+)
    pub const EFER: Self = Self(0xC000_0080);
    pub const STAR: Self = Self(0xC000_0081);
    pub const LSTAR: Self = Self(0xC000_0082);
    pub const CSTAR: Self = Self(0xC000_0083);
    pub const SFMASK: Self = Self(0xC000_0084);
    pub const FS_BASE: Self = Self(0xC000_0100);
    pub const GS_BASE: Self = Self(0xC000_0101);
    pub const KERNEL_GS_BASE: Self = Self(0xC000_0102);

    /// Returns the raw MSR address for use with RDMSR/WRMSR.
    #[inline]
    pub const fn address(self) -> u32 {
        self.0
    }
}
```

**Usage:**
```rust
// Type-safe MSR access
fn read_msr(msr: Msr) -> u64 {
    unsafe { /* rdmsr(msr.address()) */ }
}

let apic_base = read_msr(Msr::APIC_BASE);
// read_msr(0x1B);  // Compile error: expected Msr, found integer
```

### Pattern 2: Bitflags for Hardware Flags

**Problem**: Page table flags, APIC settings are bitmasks. Easy to accidentally OR incompatible flags.

**Solution**: `bitflags!` macro provides type-safe flag combinations.

```rust
bitflags::bitflags! {
    /// x86_64 page table entry flags.
    ///
    /// These flags control page permissions, caching behavior, and
    /// hardware-maintained access/dirty bits. Combine with `|` operator.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PageFlags: u64 {
        /// Page is present in memory.
        const PRESENT       = 1 << 0;
        /// Page is writable (otherwise read-only).
        const WRITABLE      = 1 << 1;
        /// Page is accessible from user mode (ring 3).
        const USER          = 1 << 2;
        /// Write-through caching (vs write-back).
        const WRITE_THROUGH = 1 << 3;
        /// Disable caching for this page.
        const CACHE_DISABLE = 1 << 4;
        /// Set by hardware when page is accessed.
        const ACCESSED      = 1 << 5;
        /// Set by hardware when page is written.
        const DIRTY         = 1 << 6;
        /// Page is 2MB (PDE) or 1GB (PDPTE) huge page.
        const HUGE          = 1 << 7;
        /// Page is global (not flushed on CR3 change).
        const GLOBAL        = 1 << 8;
        /// Disable instruction fetch from this page (requires NX bit in EFER).
        const NO_EXECUTE    = 1 << 63;

        // Convenience combinations
        const KERNEL_RW = Self::PRESENT.bits() | Self::WRITABLE.bits();
        const KERNEL_RO = Self::PRESENT.bits();
        const USER_RW = Self::PRESENT.bits() | Self::WRITABLE.bits() | Self::USER.bits();
        const USER_RO = Self::PRESENT.bits() | Self::USER.bits();
    }
}

impl PageFlags {
    /// Address mask for extracting physical frame address from PTE.
    /// Bits 12-51 contain the 4KB-aligned physical address.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    /// Extract physical address from a page table entry.
    #[inline]
    pub const fn extract_address(pte: u64) -> u64 {
        pte & Self::ADDRESS_MASK
    }
}
```

**Usage:**
```rust
// Type-safe flag combinations
let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
let pte = phys_addr | flags.bits();

// Checking flags
if flags.contains(PageFlags::USER) {
    // User-accessible page
}

// Invalid combinations caught at compile time (if using strict bitflags)
// let bad = PageFlags::PRESENT | 0x100;  // Error: cannot OR with raw integer
```

### Pattern 3: Newtype with Extraction Methods

**Problem**: APIC base MSR contains both an address and flags packed into 64 bits.

**Solution**: Newtype with const methods for field extraction.

```rust
/// IA32_APIC_BASE MSR value (MSR 0x1B).
///
/// Layout:
/// - Bits 0-7: Reserved
/// - Bit 8: BSP flag (1 = bootstrap processor)
/// - Bit 9: Reserved
/// - Bit 10: x2APIC enable
/// - Bit 11: APIC global enable
/// - Bits 12-51: APIC base physical address (4KB aligned)
/// - Bits 52-63: Reserved
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ApicBaseMsr(pub u64);

impl ApicBaseMsr {
    /// Mask for extracting the APIC physical base address.
    pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    /// Bootstrap processor flag.
    pub const BSP: u64 = 1 << 8;

    /// x2APIC mode enable.
    pub const X2APIC_ENABLE: u64 = 1 << 10;

    /// APIC global enable.
    pub const GLOBAL_ENABLE: u64 = 1 << 11;

    /// Extract the physical base address of the APIC registers.
    #[inline]
    pub const fn address(self) -> u64 {
        self.0 & Self::ADDR_MASK
    }

    /// Check if this is the bootstrap processor.
    #[inline]
    pub const fn is_bsp(self) -> bool {
        self.0 & Self::BSP != 0
    }

    /// Check if x2APIC mode is enabled.
    #[inline]
    pub const fn is_x2apic(self) -> bool {
        self.0 & Self::X2APIC_ENABLE != 0
    }

    /// Check if the APIC is globally enabled.
    #[inline]
    pub const fn is_enabled(self) -> bool {
        self.0 & Self::GLOBAL_ENABLE != 0
    }

    /// Create a new MSR value with the given base address and flags.
    #[inline]
    pub const fn new(base: u64, bsp: bool, x2apic: bool, enable: bool) -> Self {
        let mut val = base & Self::ADDR_MASK;
        if bsp { val |= Self::BSP; }
        if x2apic { val |= Self::X2APIC_ENABLE; }
        if enable { val |= Self::GLOBAL_ENABLE; }
        Self(val)
    }
}
```

**Usage:**
```rust
let msr_val = read_msr(Msr::APIC_BASE);
let apic_base = ApicBaseMsr(msr_val);

let phys_addr = apic_base.address();  // Clean extraction
if apic_base.is_bsp() {
    // Bootstrap processor initialization
}
```

### Pattern 4: Structured Selectors

**Problem**: GDT selectors encode index, TI bit, and RPL in 16 bits.

**Solution**: Newtype with constructor and accessor methods.

```rust
/// x86_64 segment selector.
///
/// Layout (16 bits):
/// - Bits 0-1: Requested Privilege Level (RPL)
/// - Bit 2: Table Indicator (0 = GDT, 1 = LDT)
/// - Bits 3-15: Descriptor index
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SegmentSelector(pub u16);

impl SegmentSelector {
    /// Null selector (index 0, GDT, RPL 0).
    pub const NULL: Self = Self(0);

    /// Kernel code segment (GDT index 1, RPL 0).
    pub const KERNEL_CODE: Self = Self::new(1, false, 0);  // 0x08

    /// Kernel data segment (GDT index 2, RPL 0).
    pub const KERNEL_DATA: Self = Self::new(2, false, 0);  // 0x10

    /// User data segment (GDT index 3, RPL 3).
    /// Note: Must come before user code for SYSRET compatibility.
    pub const USER_DATA: Self = Self::new(3, false, 3);    // 0x1B

    /// User code segment (GDT index 4, RPL 3).
    pub const USER_CODE: Self = Self::new(4, false, 3);    // 0x23

    /// TSS segment (GDT index 5, RPL 0).
    pub const TSS: Self = Self::new(5, false, 0);          // 0x28

    /// Create a new segment selector.
    ///
    /// # Arguments
    /// * `index` - Descriptor table index (0-8191)
    /// * `ldt` - Use LDT instead of GDT
    /// * `rpl` - Requested privilege level (0-3)
    #[inline]
    pub const fn new(index: u16, ldt: bool, rpl: u8) -> Self {
        let ti = if ldt { 1 << 2 } else { 0 };
        Self((index << 3) | ti | (rpl as u16 & 0x3))
    }

    /// Get the descriptor table index.
    #[inline]
    pub const fn index(self) -> u16 {
        self.0 >> 3
    }

    /// Check if this selector references the LDT.
    #[inline]
    pub const fn is_ldt(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Get the requested privilege level.
    #[inline]
    pub const fn rpl(self) -> u8 {
        (self.0 & 0x3) as u8
    }

    /// Get the raw selector value for loading into segment register.
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}
```

### Pattern 5: Port Newtype

**Problem**: I/O port numbers are `u16` but shouldn't be mixed with other values.

**Solution**: Port newtype with all known ports as associated constants.

```rust
/// x86 I/O port address.
///
/// Ports are accessed via IN/OUT instructions. This newtype groups all
/// known port addresses and prevents accidentally using other u16 values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Port(pub u16);

impl Port {
    // ==================== Serial (8250 UART) ====================
    pub const COM1: Self = Self(0x3F8);
    pub const COM2: Self = Self(0x2F8);
    pub const COM3: Self = Self(0x3E8);
    pub const COM4: Self = Self(0x2E8);

    // ==================== Programmable Interval Timer (8254) ====================
    pub const PIT_CHANNEL0: Self = Self(0x40);
    pub const PIT_CHANNEL1: Self = Self(0x41);
    pub const PIT_CHANNEL2: Self = Self(0x42);
    pub const PIT_COMMAND: Self = Self(0x43);

    // ==================== PS/2 Controller (8042) ====================
    pub const PS2_DATA: Self = Self(0x60);
    pub const PS2_STATUS: Self = Self(0x64);
    pub const PS2_COMMAND: Self = Self(0x64);

    // ==================== Legacy PIC (8259) ====================
    pub const PIC1_COMMAND: Self = Self(0x20);
    pub const PIC1_DATA: Self = Self(0x21);
    pub const PIC2_COMMAND: Self = Self(0xA0);
    pub const PIC2_DATA: Self = Self(0xA1);

    // ==================== PCI Configuration ====================
    pub const PCI_CONFIG_ADDRESS: Self = Self(0xCF8);
    pub const PCI_CONFIG_DATA: Self = Self(0xCFC);

    // ==================== CMOS/RTC ====================
    pub const CMOS_ADDRESS: Self = Self(0x70);
    pub const CMOS_DATA: Self = Self(0x71);

    // ==================== QEMU/Bochs Debug ====================
    pub const QEMU_DEBUG_EXIT: Self = Self(0xF4);
    pub const BOCHS_DEBUG: Self = Self(0xE9);

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
}

// UART register offsets (relative to COMx base)
impl Port {
    pub const UART_RBR: u16 = 0;  // Receive Buffer (read)
    pub const UART_THR: u16 = 0;  // Transmit Holding (write)
    pub const UART_IER: u16 = 1;  // Interrupt Enable
    pub const UART_IIR: u16 = 2;  // Interrupt Identification (read)
    pub const UART_FCR: u16 = 2;  // FIFO Control (write)
    pub const UART_LCR: u16 = 3;  // Line Control
    pub const UART_MCR: u16 = 4;  // Modem Control
    pub const UART_LSR: u16 = 5;  // Line Status
    pub const UART_MSR: u16 = 6;  // Modem Status
    pub const UART_SCR: u16 = 7;  // Scratch
}
```

---

## Detailed Module Specifications

### `abi/src/arch/x86_64/msr.rs`

**Purpose**: Model-Specific Register addresses.

**Contents**:
- `Msr` newtype
- All MSR addresses used by SlopOS
- EFER flags (separate bitflags if needed)

**Constants to include**:
```
MSR_APIC_BASE (0x1B)
MSR_MTRR_CAP (0xFE)
MSR_SYSENTER_* (0x174-0x176)
MSR_PAT (0x277)
MSR_EFER (0xC000_0080)
MSR_STAR (0xC000_0081)
MSR_LSTAR (0xC000_0082)
MSR_CSTAR (0xC000_0083)
MSR_SFMASK (0xC000_0084)
MSR_FS_BASE (0xC000_0100)
MSR_GS_BASE (0xC000_0101)
MSR_KERNEL_GS_BASE (0xC000_0102)
```

### `abi/src/arch/x86_64/apic.rs`

**Purpose**: Local APIC definitions.

**Contents**:
- `ApicBaseMsr` newtype (from MSR 0x1B value)
- LAPIC register offsets
- LAPIC flags (spurious, LVT, timer, etc.)
- CPUID feature flags for APIC detection

**Source**: Migrate from `drivers/src/hw/apic_defs.rs`

### `abi/src/arch/x86_64/ioapic.rs`

**Purpose**: I/O APIC definitions.

**Contents**:
- Register offsets (ID, VER, REDIR)
- `IoApicFlags` bitflags for redirection entries
- Delivery modes, destination modes
- Polarity, trigger modes
- MADT entry types for ACPI parsing

**Source**: Migrate from `drivers/src/hw/ioapic_defs.rs`

### `abi/src/arch/x86_64/paging.rs`

**Purpose**: Page table structures and flags.

**Contents**:
- `PageFlags` bitflags
- Page sizes (4KB, 2MB, 1GB)
- PTE address mask
- Entries per table (512)

**Source**: Extract from `mm/src/mm_constants.rs`

### `abi/src/arch/x86_64/memory.rs`

**Purpose**: Memory layout constants.

**Contents**:
- Kernel virtual base
- HHDM (Higher Half Direct Map) base
- Kernel heap base/size
- User space ranges
- Process memory layout (code, data, heap, stack VAs)
- Exception stack layout

**Source**: Extract from `mm/src/mm_constants.rs`

### `abi/src/arch/x86_64/gdt.rs`

**Purpose**: Global Descriptor Table.

**Contents**:
- `SegmentSelector` newtype
- Standard selectors (kernel code/data, user code/data, TSS)
- Descriptor access byte flags
- Descriptor flags (granularity, size, long mode)

**Source**: Extract from `boot/src/gdt.rs`, merge with `abi/src/arch.rs`

### `abi/src/arch/x86_64/idt.rs`

**Purpose**: Interrupt Descriptor Table.

**Contents**:
- Exception vector numbers (0-31)
- IRQ base vector (32)
- Syscall vector (0x80)
- Gate type flags (interrupt gate, trap gate)

**Source**: Extract from `boot/src/idt.rs`, merge with `abi/src/arch.rs`

### `abi/src/arch/x86_64/ports.rs`

**Purpose**: I/O port addresses.

**Contents**:
- `Port` newtype
- All port addresses (serial, PIT, PS/2, PIC, PCI, debug)
- UART register offsets

**Source**: Consolidate from `drivers/src/hw/*_defs.rs`

### `abi/src/arch/x86_64/pci.rs`

**Purpose**: PCI bus constants.

**Contents**:
- Configuration space registers
- Header type constants
- Command register bits
- BAR flags
- Known vendor/device IDs (VirtIO, etc.)

**Source**: Migrate from `drivers/src/hw/pci_defs.rs`

### `abi/src/arch/x86_64/cpuid.rs`

**Purpose**: CPUID feature detection.

**Contents**:
- Feature flag constants for EDX, ECX of leaf 1
- Extended feature flags
- Leaf numbers

**Source**: Extract from `drivers/src/hw/apic_defs.rs`, expand

---

## Migration Plan

### Phase 1: Foundation (abi changes only)

**Goal**: Create module structure and add bitflags dependency.

**Steps**:
1. Add `bitflags = "2.4"` to `abi/Cargo.toml`
2. Create `abi/src/arch/mod.rs` with arch detection
3. Create `abi/src/arch/x86_64/mod.rs` as re-exporter
4. Create initial modules with Msr, PageFlags, SegmentSelector newtypes
5. Update `abi/src/lib.rs` to use new arch module structure
6. Verify build

**Files created**:
- `abi/src/arch/mod.rs`
- `abi/src/arch/x86_64/mod.rs`
- `abi/src/arch/x86_64/msr.rs`
- `abi/src/arch/x86_64/paging.rs`
- `abi/src/arch/x86_64/gdt.rs`

**Verification**: `make build` succeeds, no functional changes yet.

### Phase 2: Hardware Definitions Migration

**Goal**: Move `drivers/src/hw/*_defs.rs` to abi.

**Steps**:
1. Create `abi/src/arch/x86_64/apic.rs` from `drivers/hw/apic_defs.rs`
2. Create `abi/src/arch/x86_64/ioapic.rs` from `drivers/hw/ioapic_defs.rs`
3. Create `abi/src/arch/x86_64/ports.rs` consolidating all port constants
4. Create `abi/src/arch/x86_64/pci.rs` from `drivers/hw/pci_defs.rs`
5. Update `drivers/src/hw/mod.rs` to re-export from abi (backward compat)
6. Update direct consumers in drivers/ to use new imports

**Files modified**:
- `drivers/src/hw/mod.rs` (becomes thin re-export)
- `drivers/src/apic.rs`, `drivers/src/ioapic.rs`, etc.

**Verification**: `make build && make test`

### Phase 3: Memory Constants Migration

**Goal**: Move `mm/src/mm_constants.rs` to abi.

**Steps**:
1. Create `abi/src/arch/x86_64/memory.rs` with layout constants
2. Move page flags to `abi/src/arch/x86_64/paging.rs` as bitflags
3. Update `mm/src/mm_constants.rs` to re-export from abi
4. Update mm/ consumers to use new imports

**Files modified**:
- `mm/src/mm_constants.rs` (becomes thin re-export)
- `mm/src/paging.rs`, `mm/src/memory_init.rs`, etc.

**Verification**: `make build && make test`

### Phase 4: Boot/Sched Integration

**Goal**: Update remaining consumers.

**Steps**:
1. Update `boot/src/gdt.rs` to use `SegmentSelector`
2. Update `boot/src/idt.rs` to use abi vectors
3. Update `sched/src/task.rs`, `sched/src/test_tasks.rs`
4. Update `video/src/lib.rs`
5. Remove remaining duplicate constants

**Verification**: `make build && make test`

### Phase 5: Cleanup

**Goal**: Remove old files, final verification.

**Steps**:
1. Delete `drivers/src/hw/*_defs.rs` files
2. Simplify `drivers/src/hw/mod.rs`
3. Remove backward-compat re-exports if no longer needed
4. Final code review for any remaining duplicates
5. Update documentation

**Verification**:
- `make build && make test`
- `grep -r "const.*=" --include="*.rs" | grep -E "(PAGE_|APIC_|MSR_|GDT_|IDT_)" | wc -l` should show reduction

---

## File Inventory

### Files to Create

| File | Lines (est.) | Description |
|------|--------------|-------------|
| `abi/src/arch/mod.rs` | ~20 | Arch detection, re-exports |
| `abi/src/arch/x86_64/mod.rs` | ~50 | Module re-exports |
| `abi/src/arch/x86_64/msr.rs` | ~80 | MSR newtype and addresses |
| `abi/src/arch/x86_64/apic.rs` | ~150 | Local APIC definitions |
| `abi/src/arch/x86_64/ioapic.rs` | ~120 | I/O APIC definitions |
| `abi/src/arch/x86_64/paging.rs` | ~100 | PageFlags bitflags |
| `abi/src/arch/x86_64/memory.rs` | ~80 | Memory layout constants |
| `abi/src/arch/x86_64/gdt.rs` | ~100 | SegmentSelector newtype |
| `abi/src/arch/x86_64/idt.rs` | ~60 | IDT vectors and gates |
| `abi/src/arch/x86_64/ports.rs` | ~120 | Port newtype, all ports |
| `abi/src/arch/x86_64/pci.rs` | ~150 | PCI constants |
| `abi/src/arch/x86_64/cpuid.rs` | ~60 | CPU feature flags |

**Total new**: ~12 files, ~1090 lines

### Files to Modify

| File | Changes |
|------|---------|
| `abi/Cargo.toml` | Add bitflags dependency |
| `abi/src/lib.rs` | Update arch module export |
| `abi/src/arch.rs` | Transform or remove (replaced by arch/mod.rs) |
| `drivers/src/hw/mod.rs` | Become re-export layer |
| `drivers/src/apic.rs` | Update imports |
| `drivers/src/ioapic.rs` | Update imports |
| `drivers/src/irq.rs` | Update imports |
| `drivers/src/pci.rs` | Update imports |
| `drivers/src/pit.rs` | Update imports |
| `drivers/src/serial.rs` | Update imports |
| `drivers/src/keyboard.rs` | Update imports |
| `drivers/src/mouse.rs` | Update imports |
| `mm/src/mm_constants.rs` | Become re-export layer |
| `mm/src/paging.rs` | Update imports |
| `mm/src/memory_init.rs` | Update imports |
| `boot/src/gdt.rs` | Use SegmentSelector |
| `boot/src/idt.rs` | Update imports |
| `sched/src/task.rs` | Update imports |
| `sched/src/test_tasks.rs` | Update imports |

### Files to Delete (after migration)

| File | Lines | Reason |
|------|-------|--------|
| `drivers/src/hw/apic_defs.rs` | 103 | Moved to abi |
| `drivers/src/hw/ioapic_defs.rs` | 101 | Moved to abi |
| `drivers/src/hw/pci_defs.rs` | 109 | Moved to abi |
| `drivers/src/hw/pit_defs.rs` | 40 | Consolidated into ports.rs |
| `drivers/src/hw/ps2_defs.rs` | 15 | Consolidated into ports.rs |
| `drivers/src/hw/pic_defs.rs` | 24 | Consolidated into ports.rs |
| `drivers/src/hw/serial_defs.rs` | 84 | Consolidated into ports.rs |

**Total deleted**: ~476 lines

### Net Change

- **Added**: ~1090 lines (type-safe, documented)
- **Removed**: ~476 lines (raw constants)
- **Net**: +614 lines

The increase is justified by:
- Comprehensive documentation
- Type-safe newtypes with methods
- Bitflags implementations
- Module organization boilerplate

---

## Backward Compatibility

### Re-export Strategy

During migration, old import paths continue to work via re-exports:

```rust
// drivers/src/hw/mod.rs (after migration)
//! Hardware definitions - re-exported from abi for backward compatibility.

pub use slopos_abi::arch::x86_64::apic::*;
pub use slopos_abi::arch::x86_64::ioapic::*;
// etc.

// Deprecation warning for direct imports
#[deprecated(since = "0.2.0", note = "Use slopos_abi::arch::x86_64::apic instead")]
pub use slopos_abi::arch::x86_64::apic as apic_defs;
```

### Type Compatibility

Newtypes use `#[repr(transparent)]` ensuring:
- `Msr(0x1B)` has same memory layout as `0x1Bu32`
- `SegmentSelector(0x23)` has same layout as `0x23u16`
- Zero runtime overhead
- Safe transmutation if needed

### Gradual Adoption

Consumers can adopt new types incrementally:
```rust
// Old code continues to work
let selector: u16 = 0x23;

// New code is more explicit
let selector = SegmentSelector::USER_CODE;
let raw: u16 = selector.bits();  // When raw value needed
```

---

## Testing Strategy

### Unit Tests

Each newtype/bitflags should have basic tests in abi:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_selector_layout() {
        assert_eq!(SegmentSelector::KERNEL_CODE.bits(), 0x08);
        assert_eq!(SegmentSelector::KERNEL_DATA.bits(), 0x10);
        assert_eq!(SegmentSelector::USER_DATA.bits(), 0x1B);
        assert_eq!(SegmentSelector::USER_CODE.bits(), 0x23);
    }

    #[test]
    fn segment_selector_decomposition() {
        let sel = SegmentSelector::USER_CODE;
        assert_eq!(sel.index(), 4);
        assert_eq!(sel.rpl(), 3);
        assert!(!sel.is_ldt());
    }

    #[test]
    fn page_flags_combinations() {
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
        assert!(flags.contains(PageFlags::PRESENT));
        assert!(flags.contains(PageFlags::USER));
        assert!(!flags.contains(PageFlags::HUGE));
    }
}
```

### Integration Tests

The existing `make test` target runs the interrupt test harness, which exercises:
- IDT vector handling
- GDT selector usage
- APIC configuration

After migration, these tests verify the new types work correctly in context.

### Regression Checks

Before merging each phase:
1. `make build` - Compilation succeeds
2. `make test` - Interrupt harness passes
3. `make boot-log` - Kernel boots normally
4. Manual inspection of serial output for any warnings

---

## Future Considerations

### Multi-Architecture Support

The module structure supports future architectures:

```
abi/src/arch/
├── mod.rs          # #[cfg] selects target arch
├── x86_64/         # Current
├── aarch64/        # Future ARM64 port
│   ├── mod.rs
│   ├── gic.rs      # Generic Interrupt Controller
│   ├── paging.rs   # ARM64 page tables
│   └── ...
└── riscv64/        # Future RISC-V port
    ├── mod.rs
    ├── plic.rs     # Platform-Level Interrupt Controller
    └── ...
```

### Additional Type Safety

Future enhancements could include:
- `PhysAddr` / `VirtAddr` newtypes (common in Rust OS projects)
- Typed register access (`lapic.read::<SpuriousVector>()`)
- Builder patterns for complex structures (GDT entries, IDT gates)

### Macro-Based Register Definitions

For very regular structures (LAPIC registers), consider macros:

```rust
define_mmio_registers! {
    LapicRegs {
        0x020 => id: u32,
        0x030 => version: u32,
        0x0B0 => eoi: u32 [write_only],
        0x0F0 => spurious: SpuriousVector,
        // ...
    }
}
```

---

## Appendix: Current vs Proposed Import Comparison

### Current (scattered)

```rust
// In drivers/src/apic.rs
use crate::hw::apic_defs::{
    CPUID_FEAT_EDX_APIC, MSR_APIC_BASE, APIC_BASE_ADDR_MASK,
    LAPIC_ID, LAPIC_VERSION, LAPIC_EOI, LAPIC_SPURIOUS,
    LAPIC_SPURIOUS_ENABLE, // ... 20+ more
};

// In mm/src/memory_init.rs
const APIC_BASE_ADDR_MASK: u64 = 0xFFFFF000;  // Duplicate!

// In boot/src/idt.rs
pub const IRQ_BASE_VECTOR: u8 = 32;
pub const SYSCALL_VECTOR: u8 = 0x80;

// In mm/src/mm_constants.rs
pub const PAGE_PRESENT: u64 = 0x001;
pub const PAGE_WRITABLE: u64 = 0x002;
// ... 15+ more raw constants
```

### Proposed (consolidated)

```rust
// In drivers/src/apic.rs
use slopos_abi::arch::x86_64::{Msr, ApicBaseMsr};
use slopos_abi::arch::x86_64::apic::{LapicReg, ApicFlags};
use slopos_abi::arch::x86_64::cpuid::CpuidFeature;

// In mm/src/memory_init.rs
use slopos_abi::arch::x86_64::{Msr, ApicBaseMsr};
// No more duplicate constants!

// In boot/src/idt.rs
use slopos_abi::arch::x86_64::idt::{Exception, IRQ_BASE_VECTOR, SYSCALL_VECTOR};

// In mm/src/paging.rs
use slopos_abi::arch::x86_64::paging::PageFlags;
let flags = PageFlags::PRESENT | PageFlags::WRITABLE;  // Type-safe!
```

---

## Conclusion

This refactoring addresses fundamental architectural issues in how SlopOS organizes hardware constants. By following Linux's proven pattern of a dedicated architecture layer, and leveraging Rust's type system beyond what C can offer, we achieve:

1. **Correctness**: Single source of truth eliminates subtle bugs from mismatched constants
2. **Safety**: Compile-time prevention of misused values
3. **Maintainability**: Clear ownership and documentation
4. **Extensibility**: Foundation for multi-architecture support

The migration can be done incrementally with full backward compatibility at each phase.
