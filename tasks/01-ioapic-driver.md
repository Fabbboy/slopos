# Task 01 – Implement IOAPIC Driver

Goal: Detect and program the system I/O APIC so external IRQ lines no longer depend on the legacy 8259 PIC bridge.

## Requirements
1. **Discovery**
   - Parse the ACPI MADT (via Limine-provided tables) to locate IOAPIC entries and interrupt source overrides.
   - Map the IOAPIC MMIO registers into the HHDM so the kernel can read/write them.
   - Record the global system interrupt (GSI) ranges each IOAPIC services.
2. **Driver API**
   - Create `drivers/ioapic.[ch]` that exposes helpers to:
     - Initialize all discovered IOAPICs
     - Configure a redirection entry (vector, delivery mode, polarity, trigger mode, destination LAPIC ID, mask state)
     - Mask/unmask a given GSI
   - Reuse existing LAPIC IDs; assume single-CPU routing for now.
3. **Testing hooks**
   - Add debug logging so we can confirm IOAPIC base addresses, GSIs, and redirection entries during boot.
   - Provide a temporary helper that can be called from early init to map IRQ1 (keyboard) through the IOAPIC but keep the PIC path active for fallback (will be removed in later tasks).

## Acceptance Criteria
- Boot log clearly shows IOAPIC detection and the GSI ranges.
- A developer can call `ioapic_config_irq(gsi, vector, lapic_id, flags)` and see the redirection entry change in logs.
- Legacy PIC code remains untouched in this task (migration happens later), but the new driver compiles and is ready for integration.
