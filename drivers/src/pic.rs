use slopos_lib::ports::{PIC_EOI, PIC1_COMMAND, PIC1_DATA, PIC2_COMMAND, PIC2_DATA};

pub fn pic_quiesce_disable() {
    unsafe {
        PIC1_DATA.write(0xFF);
        PIC2_DATA.write(0xFF);
        PIC1_COMMAND.write(PIC_EOI);
        PIC2_COMMAND.write(PIC_EOI);
    }
}
