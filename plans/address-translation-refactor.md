# Address Translation API Refactoring Plan

**Status:** Complete (Phases 1-4 complete)
**Author:** Claude (with kernel-architect guidance)
**Date:** 2026-01-09

---

## Executive Summary

SlopOS currently has fragmented address translation APIs with three separate HHDM offset storages, one of which is never initialized. This plan proposes a clean Rust-native redesign that leverages the type system for compile-time safety, following patterns from Linux and the Rust OS ecosystem.

## Status Update

- Typed address API, unified HHDM storage, and `MmioRegion` are implemented.
- Core MM and driver migrations are complete; legacy `phys_virt` wrappers removed.
- Phase 4 cleanup complete (no legacy wrappers or untyped HHDM helpers remain).
- Remaining gaps (planned follow-ups):
  - `mm/src/user_copy.rs` uses `VirtAddr::new()` on user-provided pointers; non-canonical input should fail gracefully instead of panicking (`VirtAddr::try_new()` or explicit canonical check).
  - `mm/src/hhdm.rs` initializes the `HHDM_INITIALIZED` flag before storing the offset; publish offset first, then set the flag to avoid observing `initialized` with offset=0.
  - `mm/src/mmio.rs` lacks overflow checking for `phys + hhdm::offset()` when building `virt_base`.
  - Some drivers still use raw `u64` physical addresses (type-safety gap), e.g. `drivers/src/virtio_gpu.rs` queue/command phys fields and `drivers/src/ioapic.rs` `phys_addr`/`map_ioapic_mmio`/`acpi_map_table`.
  - `MmioAddr` type is defined but not used in any driver or MMIO API surface.
  - For a long-term Rust-native fix to user pointer validation and kernel/user type boundaries, see `plans/user-pointer-type-safety.md`.

---

## Problem Statement

### Current Architecture (The Mess)

```
                           ┌─────────────────────────────┐
                           │    Limine Bootloader        │
                           │    (source of truth)        │
                           └──────────────┬──────────────┘
                                          │
        ┌─────────────────────────────────┼─────────────────────────────────┐
        │                                 │                                 │
        ▼                                 ▼                                 ▼
┌───────────────────┐           ┌───────────────────┐           ┌───────────────────┐
│ boot/limine_proto │           │ mm/memory_init.rs │           │ mm/lib.rs         │
│                   │           │                   │           │                   │
│ get_hhdm_offset() │──────────▶│ HHDM_OFFSET:u64   │           │ HHDM_OFFSET:      │
│ (live query)      │  writes   │ (static mut)      │           │ AtomicU64         │
└────────┬──────────┘           └────────┬──────────┘           │ (NEVER INIT!)     │
         │                               │                      └────────┬──────────┘
         │                               │                               │
         ▼                               ▼                               ▼
┌───────────────────┐           ┌───────────────────┐           ┌───────────────────┐
│ sched_bridge      │           │ mm/phys_virt.rs   │           │ mm::hhdm_*()      │
│                   │           │                   │           │                   │
│ get_hhdm_offset() │           │ mm_phys_to_virt() │           │ hhdm_phys_to_virt │
│ (trait delegate)  │           │ (validated)       │           │ (broken - uses 0) │
└────────┬──────────┘           └───────────────────┘           └────────┬──────────┘
         │                                                               │
         ▼                                                               ▼
┌───────────────────┐                                           ┌───────────────────┐
│ apic.rs/ioapic.rs │                                           │ video_bridge.rs   │
│ (inline HHDM math)│                                           │ (uses broken API!)│
└───────────────────┘                                           └───────────────────┘
```

### Critical Issues

| Issue | Severity | Impact |
|-------|----------|--------|
| `mm::lib::HHDM_OFFSET` never initialized | **CRITICAL** | `hhdm_phys_to_virt()` returns wrong values |
| Three separate HHDM storages | HIGH | Maintenance burden, confusion |
| Raw `u64` for all addresses | HIGH | Easy to confuse physical/virtual |
| No MMIO abstraction | MEDIUM | Drivers do unsafe pointer arithmetic |
| `sched_bridge` workaround | MEDIUM | Adds complexity for circular dep avoidance |

### Affected Files

**Broken/Dead Code:**
- `mm/src/lib.rs:25` - `HHDM_OFFSET: AtomicU64` (never initialized)
- `mm/src/lib.rs:40` - `mm::init()` (never called)
- `mm/src/lib.rs:51` - `hhdm_phys_to_virt()` (uses uninitialized offset)
- `drivers/src/video_bridge.rs:104` - Uses broken `hhdm_phys_to_virt()`

**Duplicate HHDM Storage:**
- `mm/src/memory_init.rs:89` - `HHDM_OFFSET: u64` (static mut)
- `mm/src/memory_init.rs:97` - `hhdm_offset_value()`
- `boot/src/limine_protocol.rs` - Live query to bootloader

**Inline HHDM Math (DRY violation):**
- `drivers/src/apic.rs:17-27` - `hhdm_virt_for()` helper
- `drivers/src/ioapic.rs:154-164` - `phys_to_virt_ptr()` helper

---

## Research: How Others Solve This

### Linux Kernel

Linux separates three distinct concerns:

| Concern | API | Use Case |
|---------|-----|----------|
| Direct-mapped kernel RAM | `__pa()` / `__va()` | Kernel heap, stack, kmalloc |
| MMIO device registers | `ioremap()` → `readl()`/`writel()` | PCI BARs, APIC, IOAPIC |
| Arbitrary virtual addresses | Page table walk | User memory, verify mappings |

Key design decisions:
- `__pa`/`__va` are raw macros - just `addr ± PAGE_OFFSET`
- `ioremap` returns `__iomem *` - a sparse annotation catching misuse
- `__iomem` pointers cannot be dereferenced directly - must use accessors

### Rust OS Ecosystem

**x86_64 crate** (already a SlopOS dependency):
```rust
pub struct PhysAddr(u64);  // Validates 52-bit physical address
pub struct VirtAddr(u64);  // Validates canonical 48-bit virtual address
```

**Redox OS:**
- `PhysicalAddress` and `VirtualAddress` as distinct types
- Architecture-specific `phys_to_virt()` translation
- Type safety eliminates undefined behavior at compile time

### Key Insight

> What C (Linux) achieves with sparse annotations and coding conventions,
> Rust achieves with the type system at **compile time, zero cost**.

---

## Proposed Architecture

### Design Goals

1. **Single source of truth** - One HHDM storage, initialized once
2. **Type safety** - Cannot confuse PhysAddr with VirtAddr
3. **MMIO safety** - Separate type enforcing volatile access
4. **Zero runtime cost** - Newtypes compile away
5. **No circular dependencies** - Types live in `abi` crate

### New Architecture

```
                           ┌─────────────────────────────┐
                           │    Limine Bootloader        │
                           │    (source of truth)        │
                           └──────────────┬──────────────┘
                                          │
                                          ▼
                           ┌─────────────────────────────┐
                           │     abi/src/addr.rs         │
                           │ ─────────────────────────── │
                           │ pub struct PhysAddr(u64)    │◄── Zero-cost newtypes
                           │ pub struct VirtAddr(u64)    │
                           │ pub struct MmioAddr(u64)    │◄── MMIO distinct type
                           └──────────────┬──────────────┘
                                          │
                           ┌──────────────┴──────────────┐
                           │                             │
                           ▼                             ▼
              ┌─────────────────────────┐   ┌─────────────────────────┐
              │      mm/src/hhdm.rs     │   │     mm/src/mmio.rs      │
              │ ─────────────────────── │   │ ─────────────────────── │
              │ static HHDM: AtomicU64  │   │ pub struct MmioRegion   │
              │ (SINGLE source)         │   │ - read<T>() -> T        │
              │                         │   │ - write<T>(val)         │
              │ trait PhysAddrHhdm {    │   │ (volatile, type-safe)   │
              │   fn to_virt() -> VirtA │   └─────────────────────────┘
              │   fn to_virt_checked()  │
              │ }                       │
              └─────────────────────────┘
```

---

## Detailed Design

### 1. Address Types (`abi/src/addr.rs`)

```rust
//! Physical and Virtual address types for type-safe memory operations.

/// A physical memory address. Cannot be dereferenced directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysAddr(u64);

/// A virtual memory address in kernel space.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VirtAddr(u64);

/// An MMIO address. Must be accessed via volatile operations only.
/// Equivalent to Linux's `__iomem *` annotation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct MmioAddr(u64);

impl PhysAddr {
    pub const NULL: Self = Self(0);

    #[inline]
    pub const fn new(addr: u64) -> Self { Self(addr) }

    #[inline]
    pub const fn as_u64(self) -> u64 { self.0 }

    #[inline]
    pub const fn is_null(self) -> bool { self.0 == 0 }

    #[inline]
    pub const fn offset(self, off: u64) -> Self {
        Self(self.0.wrapping_add(off))
    }

    #[inline]
    pub const fn align_down(self, align: u64) -> Self {
        Self(self.0 & !(align - 1))
    }

    #[inline]
    pub const fn align_up(self, align: u64) -> Self {
        Self((self.0 + align - 1) & !(align - 1))
    }
}

impl VirtAddr {
    pub const NULL: Self = Self(0);

    #[inline]
    pub const fn new(addr: u64) -> Self { Self(addr) }

    #[inline]
    pub const fn as_u64(self) -> u64 { self.0 }

    #[inline]
    pub const fn is_null(self) -> bool { self.0 == 0 }

    #[inline]
    pub const fn as_ptr<T>(self) -> *const T { self.0 as *const T }

    #[inline]
    pub const fn as_mut_ptr<T>(self) -> *mut T { self.0 as *mut T }

    #[inline]
    pub const fn offset(self, off: u64) -> Self {
        Self(self.0.wrapping_add(off))
    }
}

impl MmioAddr {
    pub const NULL: Self = Self(0);

    #[inline]
    pub const fn new(addr: u64) -> Self { Self(addr) }

    #[inline]
    pub const fn as_u64(self) -> u64 { self.0 }

    #[inline]
    pub const fn is_null(self) -> bool { self.0 == 0 }
}

// Convenience: From raw u64 (explicit conversion required)
impl From<u64> for PhysAddr {
    fn from(addr: u64) -> Self { Self(addr) }
}

impl From<u64> for VirtAddr {
    fn from(addr: u64) -> Self { Self(addr) }
}

impl From<PhysAddr> for u64 {
    fn from(addr: PhysAddr) -> Self { addr.0 }
}

impl From<VirtAddr> for u64 {
    fn from(addr: VirtAddr) -> Self { addr.0 }
}
```

### 2. HHDM Module (`mm/src/hhdm.rs`)

```rust
//! Higher Half Direct Map (HHDM) translation.
//!
//! This is the ONLY place where HHDM offset is stored and accessed.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use slopos_abi::addr::{PhysAddr, VirtAddr};

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
static HHDM_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize HHDM with offset from bootloader. Call once during boot.
///
/// # Panics
/// Panics if called more than once (catches double-init bugs).
pub fn init(offset: u64) {
    if HHDM_INITIALIZED.swap(true, Ordering::SeqCst) {
        panic!("HHDM already initialized");
    }
    HHDM_OFFSET.store(offset, Ordering::Release);
}

/// Check if HHDM is initialized.
#[inline]
pub fn is_available() -> bool {
    HHDM_INITIALIZED.load(Ordering::Acquire)
}

/// Get raw HHDM offset. For internal use only.
///
/// # Panics
/// Debug-panics if HHDM not initialized.
#[inline]
pub fn offset() -> u64 {
    debug_assert!(is_available(), "HHDM not initialized");
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// Extension trait adding HHDM translation methods to PhysAddr.
pub trait PhysAddrHhdm {
    /// Convert to virtual address via HHDM.
    /// Returns NULL for null addresses.
    ///
    /// # Panics
    /// Panics if HHDM not initialized.
    fn to_virt(self) -> VirtAddr;

    /// Try to convert. Returns None if HHDM unavailable or address is null.
    fn try_to_virt(self) -> Option<VirtAddr>;

    /// Convert with full validation (reservation checks, overflow, etc.)
    fn to_virt_checked(self) -> Option<VirtAddr>;
}

impl PhysAddrHhdm for PhysAddr {
    #[inline]
    fn to_virt(self) -> VirtAddr {
        if self.is_null() {
            return VirtAddr::NULL;
        }
        assert!(is_available(), "HHDM not initialized");
        VirtAddr::new(self.as_u64() + offset())
    }

    #[inline]
    fn try_to_virt(self) -> Option<VirtAddr> {
        if self.is_null() || !is_available() {
            return None;
        }
        Some(VirtAddr::new(self.as_u64() + offset()))
    }

    fn to_virt_checked(self) -> Option<VirtAddr> {
        use crate::memory_reservations as reservations;

        if self.is_null() {
            return None;
        }

        if !is_available() {
            return None;
        }

        // Check reservation database
        if let Some(region) = reservations::find(self.as_u64()) {
            if !region.allows_mm_phys_to_virt() {
                return None;
            }
        }

        let hhdm = offset();

        // Check if already in higher-half (idempotent)
        if self.as_u64() >= hhdm {
            return Some(VirtAddr::new(self.as_u64()));
        }

        // Check overflow
        if self.as_u64() > u64::MAX - hhdm {
            return None;
        }

        Some(VirtAddr::new(self.as_u64() + hhdm))
    }
}

/// Extension trait for VirtAddr reverse translation.
pub trait VirtAddrHhdm {
    /// Convert back to physical assuming HHDM mapping.
    fn to_phys_hhdm(self) -> PhysAddr;

    /// Convert via page table walk (works for any mapping).
    fn to_phys_walk(self) -> Option<PhysAddr>;
}

impl VirtAddrHhdm for VirtAddr {
    #[inline]
    fn to_phys_hhdm(self) -> PhysAddr {
        if self.is_null() {
            return PhysAddr::NULL;
        }
        PhysAddr::new(self.as_u64().wrapping_sub(offset()))
    }

    fn to_phys_walk(self) -> Option<PhysAddr> {
        if self.is_null() {
            return None;
        }
        crate::paging::translate(self)
    }
}
```

### 3. MMIO Module (`mm/src/mmio.rs`)

```rust
//! MMIO region abstraction - type-safe device register access.
//!
//! Equivalent to Linux's ioremap() + __iomem pointer pattern.

use core::ptr::{read_volatile, write_volatile};
use slopos_abi::addr::PhysAddr;
use crate::hhdm;

/// A mapped MMIO region providing safe volatile access to device registers.
///
/// Like Linux's `__iomem *`, this type cannot be dereferenced directly.
/// Use `read()` and `write()` methods for proper volatile access.
pub struct MmioRegion {
    virt_base: u64,
    size: usize,
}

impl MmioRegion {
    /// Map a physical MMIO region via HHDM.
    ///
    /// Returns None if:
    /// - Physical address is null
    /// - Size is zero
    /// - Address + size would overflow
    /// - HHDM is not available
    pub fn map(phys: PhysAddr, size: usize) -> Option<Self> {
        if phys.is_null() || size == 0 {
            return None;
        }

        // Check for overflow
        phys.as_u64().checked_add(size as u64)?;

        if !hhdm::is_available() {
            return None;
        }

        Some(Self {
            virt_base: phys.as_u64() + hhdm::offset(),
            size,
        })
    }

    /// Map a single 4KB page at physical address.
    pub fn map_page(phys: PhysAddr) -> Option<Self> {
        Self::map(phys, 4096)
    }

    /// Read a value at byte offset.
    ///
    /// # Panics
    /// Panics if offset + sizeof(T) exceeds region size.
    #[inline]
    pub fn read<T: Copy>(&self, offset: usize) -> T {
        let end = offset.checked_add(core::mem::size_of::<T>())
            .expect("offset overflow");
        assert!(end <= self.size, "MMIO read out of bounds");

        let ptr = (self.virt_base + offset as u64) as *const T;
        // SAFETY: MmioRegion guarantees valid mapping, bounds checked above
        unsafe { read_volatile(ptr) }
    }

    /// Write a value at byte offset.
    ///
    /// # Panics
    /// Panics if offset + sizeof(T) exceeds region size.
    #[inline]
    pub fn write<T: Copy>(&self, offset: usize, value: T) {
        let end = offset.checked_add(core::mem::size_of::<T>())
            .expect("offset overflow");
        assert!(end <= self.size, "MMIO write out of bounds");

        let ptr = (self.virt_base + offset as u64) as *mut T;
        // SAFETY: MmioRegion guarantees valid mapping, bounds checked above
        unsafe { write_volatile(ptr, value) }
    }

    /// Get virtual base address (for debugging only).
    pub fn virt_base(&self) -> u64 {
        self.virt_base
    }

    /// Get region size.
    pub fn size(&self) -> usize {
        self.size
    }
}

// MmioRegion is !Send and !Sync by default, which is correct for most MMIO
// regions that are CPU-local or require explicit synchronization.

/// Marker trait for MMIO regions that are safe to share between CPUs.
/// Implement this only for MMIO regions with hardware synchronization.
pub unsafe trait SharedMmio {}

// If a region is marked SharedMmio, it can be Sync
unsafe impl<T: SharedMmio> Sync for T {}
```

### 4. Example: Clean APIC Driver

**Before:**
```rust
fn hhdm_virt_for(phys: u64) -> Option<u64> {
    if phys == 0 { return None; }
    if sched_bridge::is_hhdm_available() != 0 {
        Some(phys + sched_bridge::get_hhdm_offset())
    } else {
        None
    }
}

static APIC_BASE_ADDRESS: AtomicU64 = AtomicU64::new(0);

fn read_register(reg: u32) -> u32 {
    let base = APIC_BASE_ADDRESS.load(Ordering::Relaxed);
    if !is_available() || base == 0 { return 0; }
    let reg_ptr = (base + reg as u64) as *const u32;
    unsafe { read_volatile(reg_ptr) }
}
```

**After:**
```rust
use slopos_abi::addr::PhysAddr;
use slopos_mm::{MmioRegion, hhdm::PhysAddrHhdm};
use spin::Once;

static APIC_REGS: Once<MmioRegion> = Once::new();

pub fn init() -> Result<(), ApicError> {
    let base_msr = cpu::read_msr(MSR_APIC_BASE);
    let phys = PhysAddr::new(base_msr & APIC_BASE_ADDR_MASK);

    let region = MmioRegion::map(phys, 0x1000)
        .ok_or(ApicError::MappingFailed)?;

    APIC_REGS.call_once(|| region);
    Ok(())
}

#[inline]
fn read_register(reg: u32) -> u32 {
    APIC_REGS.get()
        .map(|r| r.read(reg as usize))
        .unwrap_or(0)
}

#[inline]
fn write_register(reg: u32, value: u32) {
    if let Some(r) = APIC_REGS.get() {
        r.write(reg as usize, value);
    }
}
```

---

## Migration Plan

### Phase 1: Add Types (Non-Breaking)

**Goal:** Add new types alongside existing code. No breaking changes.

**Tasks:**
1. Create `abi/src/addr.rs` with `PhysAddr`, `VirtAddr`, `MmioAddr`
2. Export from `abi/src/lib.rs`
3. Create `mm/src/hhdm.rs` with single HHDM storage
4. Create `mm/src/mmio.rs` with `MmioRegion`
5. Add `#[deprecated]` attributes to old functions
6. Update `mm/src/lib.rs` to export new modules

**Notes:** Address constructors now validate 52-bit physical and canonical virtual addresses.

**Files to create:**
- `abi/src/addr.rs`
- `mm/src/hhdm.rs`
- `mm/src/mmio.rs`

**Files to modify:**
- `abi/src/lib.rs` - add `pub mod addr`
- `mm/src/lib.rs` - add modules, deprecate old functions

### Phase 2: Migrate Core MM

**Goal:** Convert memory subsystem to use typed addresses.

**Status:** Completed

**Tasks (completed):**
1. Update `mm/src/paging.rs` to use `PhysAddr`/`VirtAddr`
2. Update `mm/src/page_alloc.rs`
3. Update `mm/src/process_vm.rs`
4. Call `hhdm::init()` from `init_memory_system()`
5. Remove `memory_init::HHDM_OFFSET` static
7. Update core MM callers (`kernel_heap`, `shared_memory`, `user_copy`)

**Files modified:**
- `mm/src/memory_init.rs`
- `mm/src/paging.rs`
- `mm/src/page_alloc.rs`
- `mm/src/process_vm.rs`
- `mm/src/kernel_heap.rs`
- `mm/src/shared_memory.rs`
- `mm/src/user_copy.rs`

### Phase 3: Migrate Drivers

**Goal:** Convert all drivers to use typed addresses and MmioRegion.

**Status:** Completed

**Tasks (completed):**
1. Convert `drivers/src/apic.rs` to use `MmioRegion`
2. Convert `drivers/src/ioapic.rs` to use `MmioRegion`
3. Convert `drivers/src/virtio_gpu.rs` to use typed addresses + `MmioRegion`
4. Fix `drivers/src/video_bridge.rs` to use correct API
5. Remove inline HHDM helpers from drivers
6. Convert PCI GPU candidate mapping to `MmioRegion` (`drivers/src/pci.rs`)

**Files to modify:**
- `drivers/src/apic.rs`
- `drivers/src/ioapic.rs`
- `drivers/src/virtio_gpu.rs`
- `drivers/src/video_bridge.rs`
- `drivers/src/pci.rs`

### Phase 4: Remove Legacy

**Goal:** Remove all deprecated code and workarounds.

**Tasks:**
1. Done: Remove `sched_bridge` HHDM functions
2. Done: Remove `BootServices::get_hhdm_offset()` from trait
3. Done: Replace `mm_init_phys_virt_helpers()` usage in `mm/src/memory_init.rs`
4. Done: Remove `mm/src/phys_virt.rs` after callers are migrated
5. Done: Remove any remaining legacy wrappers and untyped HHDM helpers
6. Done: Update any remaining callers

**Files to modify/delete:**
- `mm/src/lib.rs` - remove deprecated exports/functions (if any remain)

---

## Testing Strategy

### Unit Tests (Future)

```rust
#[test]
fn test_phys_addr_null() {
    assert!(PhysAddr::NULL.is_null());
    assert_eq!(PhysAddr::NULL.to_virt(), VirtAddr::NULL);
}

#[test]
fn test_hhdm_translation() {
    hhdm::init(0xFFFF_8000_0000_0000);
    let phys = PhysAddr::new(0x1000);
    let virt = phys.to_virt();
    assert_eq!(virt.as_u64(), 0xFFFF_8000_0000_1000);
}

#[test]
#[should_panic]
fn test_double_init_panics() {
    hhdm::init(0x1000);
    hhdm::init(0x2000); // Should panic
}
```

### Integration Tests

1. Boot test - verify kernel boots with new address types
2. Memory allocation - verify page allocator works
3. APIC/IOAPIC - verify interrupt handling still works
4. VirtIO - verify GPU driver works
5. Full system test via `make test`

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Breaking boot sequence | Medium | Critical | Phase 1 is non-breaking; extensive boot testing |
| Performance regression | Low | Medium | Newtypes are zero-cost; benchmark critical paths |
| Incomplete migration | Medium | Medium | Deprecation warnings catch missed conversions |
| Circular dependency issues | Low | High | Types in `abi` crate have no dependencies |

---

## Success Criteria

1. **Single HHDM storage** - Only `mm/src/hhdm.rs` stores offset
2. **Type safety** - All address parameters use `PhysAddr`/`VirtAddr`
3. **MMIO safety** - All device registers accessed via `MmioRegion`
4. **No deprecated calls** - All old APIs removed
5. **All tests pass** - `make test` succeeds
6. **Boot verified** - Kernel boots and runs normally

---

## References

- [Linux Device I/O Documentation](https://docs.kernel.org/driver-api/device-io.html)
- [Linux ioremap implementation](https://github.com/torvalds/linux/blob/master/arch/x86/mm/ioremap.c)
- [x86_64 crate documentation](https://docs.rs/x86_64)
- [x86_64 addr.rs source](https://github.com/rust-osdev/x86_64/blob/master/src/addr.rs)
- [Redox OS kernel](https://github.com/redox-os/kernel)
- [Writing an OS in Rust - Advanced Paging](https://os.phil-opp.com/advanced-paging/)

---

## Appendix: Comparison Table

| Aspect | Current SlopOS | Proposed Design | Linux |
|--------|---------------|-----------------|-------|
| Type Safety | Raw u64 everywhere | PhysAddr/VirtAddr distinct | Sparse annotations |
| MMIO Safety | Raw pointer math | MmioRegion enforces volatile | __iomem + accessors |
| Single Source | 3 HHDM storages | 1 atomic in hhdm.rs | 1 PAGE_OFFSET |
| Compile-time checks | None | Full Rust type system | Sparse (optional) |
| Runtime cost | Zero | Zero (newtypes) | Zero |
| Circular deps | sched_bridge workaround | Types in abi | N/A (monolithic) |
