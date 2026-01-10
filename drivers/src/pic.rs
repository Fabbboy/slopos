use slopos_lib::ports::{PIC_EOI, PIC1_COMMAND, PIC1_DATA, PIC2_COMMAND, PIC2_DATA};

#[inline]
fn mask_all_impl() {
    unsafe {
        PIC1_DATA.write(0xFF);
        PIC2_DATA.write(0xFF);
    }
}

#[inline]
fn disable_impl() {
    mask_all_impl();
    unsafe {
        PIC1_COMMAND.write(PIC_EOI);
        PIC2_COMMAND.write(PIC_EOI);
    }
}

pub fn pic_quiesce_mask_all() {
    mask_all_impl();
}

pub fn pic_quiesce_disable() {
    disable_impl();
}
