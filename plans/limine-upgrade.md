# Plan: Upgrade Limine Bootloader to v8+

## Current State

- **Limine Version**: v5.20231207.1 (December 2023)
- **Base Revision**: 1 (hardcoded via `BaseRevision::with_revision(1)`)
- **Config Format**: `limine.cfg` (legacy INI-style)
- **HHDM Behavior**: Limine v5 maps both RAM and MMIO regions in HHDM

## Target State

- **Limine Version**: v8.7.0+ or v10.x
- **Base Revision**: 3 (default via `BaseRevision::new()`)
- **Config Format**: `limine.conf` (new hierarchical format)
- **HHDM Behavior**: Only RAM mapped; kernel handles MMIO mapping

## Why Upgrade?

1. Limine v5 is outdated (2+ years old)
2. The `limine` Rust crate (v0.5.0) defaults to revision 3
3. Better SMP, memory map, and paging mode support in v8+
4. Security and stability improvements
5. Active maintenance and bug fixes

## Blocking Issue

Limine v8+ changed the Higher Half Direct Map (HHDM) semantics:

| Aspect | Limine v5 | Limine v8+ |
|--------|-----------|------------|
| HHDM Contents | RAM + MMIO regions | RAM only |
| MMIO Access | Via `HHDM_BASE + phys_addr` | Must be explicitly mapped |
| LAPIC Access | Works via HHDM | Page fault (not mapped) |

**Observed Failure**:
```
Page fault at 0xffff8000fee000f0
Fault address = HHDM_BASE (0xffff800000000000) + LAPIC (0xfee00000) + offset (0xf0)
Error: Page not present (Read) (Supervisor)
```

## Implementation Plan

### Phase 1: Inventory MMIO Access Patterns

Identify all code paths that access MMIO via HHDM:

1. **LAPIC** (`0xfee00000`)
   - Location: `drivers/src/apic/` or similar
   - Used for: Timer, IPI, interrupt acknowledgment

2. **IOAPIC** (`0xfec00000` typically)
   - Location: `drivers/src/ioapic/` or similar
   - Used for: Interrupt routing

3. **Framebuffer** (variable, e.g., `0x80000000`)
   - Location: `video/src/`
   - Used for: Display output

4. **PCI MMIO BARs** (variable)
   - Location: `drivers/src/pci/`
   - Used for: Device communication

**Action**: Run grep to find all HHDM-based MMIO access:
```bash
grep -rn "hhdm\|HHDM\|0xfee0\|0xfec0" --include="*.rs" drivers/ mm/ video/
```

### Phase 2: Create MMIO Mapping Infrastructure

Create a dedicated MMIO mapping module in `mm/`:

```
mm/src/mmio.rs
```

**Required Functions**:

```rust
/// Map an MMIO region into kernel virtual address space
/// Returns the virtual address for the mapped region
pub fn mmio_map(phys_addr: u64, size: u64, flags: MmioFlags) -> Result<*mut u8, MmioError>;

/// Unmap a previously mapped MMIO region
pub fn mmio_unmap(virt_addr: *mut u8, size: u64) -> Result<(), MmioError>;

/// Map the Local APIC (convenience function)
pub fn mmio_map_lapic() -> Result<*mut u8, MmioError>;

/// Map an IOAPIC at the given physical address
pub fn mmio_map_ioapic(phys_addr: u64) -> Result<*mut u8, MmioError>;
```

**Implementation Details**:

1. Reserve a virtual address range for MMIO mappings (e.g., `0xFFFF_8100_0000_0000` to `0xFFFF_81FF_FFFF_FFFF`)
2. Use the existing paging infrastructure to create mappings
3. Set appropriate page flags: Present, Writable, No-Execute, Cache-Disable (for MMIO)
4. Track mappings to prevent double-mapping

### Phase 3: Update MMIO Consumers

Update all MMIO access sites to use the new mapping API:

#### 3.1 LAPIC Driver

**Before**:
```rust
let lapic_base = HHDM_BASE + 0xfee00000;
unsafe { (lapic_base as *mut u32).write_volatile(value); }
```

**After**:
```rust
static LAPIC_VIRT: OnceCell<*mut u8> = OnceCell::new();

pub fn init_lapic() {
    let virt = mmio_map_lapic().expect("Failed to map LAPIC");
    LAPIC_VIRT.set(virt).expect("LAPIC already initialized");
}

fn lapic_write(offset: u32, value: u32) {
    let base = LAPIC_VIRT.get().expect("LAPIC not initialized");
    unsafe { ((*base as u64 + offset as u64) as *mut u32).write_volatile(value); }
}
```

#### 3.2 IOAPIC Driver

Similar pattern - map during driver init, store virtual address.

#### 3.3 Framebuffer

The framebuffer is already passed by Limine with a virtual address in the response, so this may not need changes. Verify by checking `FramebufferResponse`.

#### 3.4 PCI MMIO

PCI BAR MMIO access needs to map each BAR region before use.

### Phase 4: Update Boot Sequence

Ensure MMIO mapping happens early enough:

1. Memory system init (existing)
2. **NEW**: MMIO mapping subsystem init
3. **UPDATED**: LAPIC/IOAPIC init (now uses mmio_map)
4. Rest of driver init

### Phase 5: Update Limine Configuration

#### 5.1 Rename Config File

```bash
mv limine.cfg limine.conf
```

#### 5.2 Convert Config Format

**Old (`limine.cfg`)**:
```ini
TIMEOUT=0
SERIAL=yes
VERBOSE=yes

:SlopOS Kernel
PROTOCOL=limine
KERNEL_PATH=boot:///boot/kernel.elf
RESOLUTION=1920x1080
```

**New (`limine.conf`)**:
```
timeout: 0
serial: yes
verbose: yes

/SlopOS Kernel
    protocol: limine
    path: boot():/boot/kernel.elf
    resolution: 1920x1080
```

#### 5.3 Update Makefile

Change references from `limine.cfg` to `limine.conf` and update cmdline format.

### Phase 6: Update Limine Submodule

```bash
cd third_party/limine
git fetch --tags
git checkout v8.7.0-binary  # or latest stable
cd ../..
```

### Phase 7: Update Boot Code

#### 7.1 Base Revision

```rust
// Change from:
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(1);

// To:
static BASE_REVISION: BaseRevision = BaseRevision::new();  // revision 3
```

#### 7.2 Verify API Compatibility

Check for any deprecated or changed APIs in the `limine` crate for v8+ support.

### Phase 8: Testing

1. **Unit Tests**: Test MMIO mapping functions in isolation
2. **Boot Test**: `make boot-log` - verify kernel boots
3. **Full Test**: `make test` - run test harness
4. **Manual Verification**: Check LAPIC timer, keyboard interrupts, display output

## Files to Modify

| File | Changes |
|------|---------|
| `mm/src/lib.rs` | Add `pub mod mmio;` |
| `mm/src/mmio.rs` | **NEW** - MMIO mapping infrastructure |
| `mm/src/mm_constants.rs` | Add MMIO virtual address range constants |
| `drivers/src/apic/lapic.rs` | Use mmio_map for LAPIC |
| `drivers/src/apic/ioapic.rs` | Use mmio_map for IOAPIC |
| `boot/src/limine_protocol.rs` | Change to `BaseRevision::new()` |
| `limine.cfg` → `limine.conf` | Rename and convert format |
| `Makefile` | Update config file references |
| `third_party/limine` | Update submodule to v8+ |

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| MMIO mapping bugs | Kernel crash | Extensive testing, defensive checks |
| Missed MMIO access sites | Runtime page faults | Thorough grep audit, boot testing |
| Limine API changes | Build failures | Check limine crate changelog |
| Performance regression | Slower MMIO access | Unlikely - same number of page table lookups |

## Estimated Effort

| Phase | Effort |
|-------|--------|
| Phase 1: Inventory | 1-2 hours |
| Phase 2: MMIO Infrastructure | 4-6 hours |
| Phase 3: Update Consumers | 2-4 hours |
| Phase 4: Boot Sequence | 1 hour |
| Phase 5-7: Config/Limine | 1 hour |
| Phase 8: Testing | 2-3 hours |
| **Total** | **11-17 hours** |

## Success Criteria

- [ ] Kernel boots with Limine v8.7.0+
- [ ] `BaseRevision::new()` (revision 3) works
- [ ] No page faults accessing LAPIC/IOAPIC
- [ ] Interrupts work (timer, keyboard)
- [ ] Framebuffer display works
- [ ] `make test` passes
- [ ] No regressions in existing functionality

## References

- [Limine Protocol Specification](https://github.com/limine-bootloader/limine/blob/trunk/PROTOCOL.md)
- [Limine v8 CONFIG.md](https://github.com/limine-bootloader/limine/blob/v8.x/CONFIG.md)
- [limine-rs crate](https://github.com/limine-bootloader/limine-rs)
- [OSDev Wiki - APIC](https://wiki.osdev.org/APIC)
