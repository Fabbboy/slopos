//! Internal legacy 8259 PIC lifecycle. SlopOS never routes IRQs through the
//! 8259; ACPI mandates this init-and-mask sequence when MADT `PCAT_COMPAT`
//! reports a dual 8259 and the OS switches to APIC/IOAPIC operation.

use crate::io::raw_port::{Port, io_wait};
use crate::sync::{BspToken, InitFlag};

const MASTER_CMD: Port<u8> = Port::new(0x20);
const MASTER_DATA: Port<u8> = Port::new(0x21);
const SLAVE_CMD: Port<u8> = Port::new(0xA0);
const SLAVE_DATA: Port<u8> = Port::new(0xA1);

const ICW1_INIT: u8 = 0x10;
const ICW1_EXPECT_ICW4: u8 = 0x01;
const ICW4_8086_MODE: u8 = 0x01;

const MASTER_VECTOR_OFFSET: u8 = 0x20;
const SLAVE_VECTOR_OFFSET: u8 = 0x28;
const MASTER_HAS_SLAVE_ON_IRQ2: u8 = 1 << 2;
const SLAVE_CASCADE_ID: u8 = 2;
const MASK_ALL_IRQS: u8 = 0xFF;

static LEGACY_8259_DISABLED: InitFlag = InitFlag::new();

/// Initialize the legacy dual 8259 pair into a known state and mask all IRQs.
///
/// The `BspToken` pins this to the BSP boot-init phase; the sequence is
/// idempotent across repeated init paths and tests.
pub fn init_and_disable_legacy_8259<'brand>(_token: &BspToken<'brand>) {
    if !LEGACY_8259_DISABLED.claim() {
        return;
    }

    // SAFETY: These are the architected 8259 command/data ports.  The
    // sequence fully reinitializes the master/slave pair, remaps them
    // away from CPU exception vectors, and masks every IRQ line.  No
    // SlopOS interrupt path depends on PIC delivery after this point.
    unsafe {
        MASTER_CMD.write(ICW1_INIT | ICW1_EXPECT_ICW4);
        io_wait();
        SLAVE_CMD.write(ICW1_INIT | ICW1_EXPECT_ICW4);
        io_wait();

        MASTER_DATA.write(MASTER_VECTOR_OFFSET);
        io_wait();
        SLAVE_DATA.write(SLAVE_VECTOR_OFFSET);
        io_wait();

        MASTER_DATA.write(MASTER_HAS_SLAVE_ON_IRQ2);
        io_wait();
        SLAVE_DATA.write(SLAVE_CASCADE_ID);
        io_wait();

        MASTER_DATA.write(ICW4_8086_MODE);
        io_wait();
        SLAVE_DATA.write(ICW4_8086_MODE);
        io_wait();

        MASTER_DATA.write(MASK_ALL_IRQS);
        io_wait();
        SLAVE_DATA.write(MASK_ALL_IRQS);
        io_wait();
    }
}
