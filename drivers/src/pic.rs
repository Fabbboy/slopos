use slopos_lib::io;

use slopos_abi::arch::x86_64::ports::{
    PIC1_COMMAND, PIC1_DATA, PIC2_COMMAND, PIC2_DATA, PIC_EOI,
};

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
