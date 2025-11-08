# Task 03 – Remove Legacy PIC Dependency

Goal: Once all runtime IRQs are serviced via IOAPIC → LAPIC, retire the 8259 PIC code path and clean up the remaining shims.

## Requirements
1. **PIC Shutdown**
   - After IOAPIC routing is verified, mask and disable the legacy PIC permanently (no ExtINT bridge, no EOIs).
   - Remove the idle-task serial polling fallback that was only needed when COM1 interrupts failed to fire.
2. **Code Pruning**
   - Delete unused PIC helpers (`pic_enable_irq`, `pic_enable_safe_irqs`, etc.) and stop compiling `drivers/pic.c` unless truly needed for diagnostics.
   - Update boot logs to reflect the pure APIC path (e.g., drop “legacy PIC kept active…” messages).
3. **Documentation**
   - Update `AGENTS.md` / relevant docs to mention that SlopOS now relies solely on LAPIC/IOAPIC for interrupts.
   - Note the minimum hardware/QEMU requirements (IOAPIC must be enabled in QEMU machine config — currently true for `q35`).
4. **Validation**
   - Run `make test` and `make boot-log` to confirm interrupts flow without the PIC present.
   - Verifiably panic/alert if hardware lacks an IOAPIC instead of silently trying to use the removed PIC path.

## Acceptance Criteria
- Building without `drivers/pic.*` succeeds, and no PIC-specific APIs are referenced anywhere.
- All interrupts (timer, keyboard, serial, future devices) are delivered via IOAPIC; the kernel emits a fatal error if IOAPIC setup fails.
- Shell input, scheduler ticks, and roulette continue to function under `make boot`/`boot-log`/`test`.
