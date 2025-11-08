# Task 02 – Route Core IRQs Through IOAPIC

Goal: Use the IOAPIC driver to deliver timer, keyboard, and serial interrupts directly to the LAPIC, eliminating the PIC→ExtINT bridge for those lines.

## Requirements
1. **IRQ Dispatcher Integration**
   - Extend `drivers/irq.c` so each IRQ line (timer=IRQ0, keyboard=IRQ1, serial COM1=IRQ4) is mapped to its corresponding GSI and configured via the IOAPIC helper.
   - Choose stable vectors in the existing IRQ vector space (e.g., 0x20+irq) and ensure the LAPIC receives them.
2. **Device Bring-up**
   - Update the PIT, keyboard, and serial init paths so they no longer depend on `pic_enable_irq`. Mask/unmask through IOAPIC instead.
   - Confirm EOI flow uses LAPIC only; PIC-specific EOIs should become no-ops once these lines are migrated.
3. **Fallback / Debug**
   - Keep the PIC initialized but leave these IRQ lines masked there so we can revert quickly if needed.
   - Add boot logs showing each line’s GSI, vector, polarity, and trigger mode when routed.
4. **Validation**
   - Boot via `make boot-log` and ensure:
     - Timer ticks still drive the scheduler.
     - Keyboard and serial input reach the shell without the idle-task poller firing.

## Acceptance Criteria
- `pic_enable_irq()` is no longer called for IRQ0/1/4; IOAPIC helpers handle masking/unmasking.
- No legacy PIC EOIs are sent for those IRQs in steady state.
- Logs demonstrate successful IOAPIC routing and shell interaction works under QEMU’s `-serial stdio`.
