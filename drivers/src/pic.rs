use slopos_lib::io;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const PIC_EOI: u8 = 0x20;

#[inline]
fn mask_all_impl() {
    // Mask all legacy PIC IRQ lines; SlopOS relies on the APIC path.
    unsafe {
        io::outb(PIC1_DATA, 0xFF);
        io::outb(PIC2_DATA, 0xFF);
    }
}

#[inline]
fn disable_impl() {
    mask_all_impl();
    // Send spurious EOI to clear any pending interrupt.
    unsafe {
        io::outb(PIC1_COMMAND, PIC_EOI);
        io::outb(PIC2_COMMAND, PIC_EOI);
    }
}

pub fn pic_quiesce_mask_all() {
    mask_all_impl();
}
pub fn pic_quiesce_disable() {
    disable_impl();
}
