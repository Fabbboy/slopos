use slopos_utils::io::Port;

const PIC1_COMMAND: Port<u8> = Port::new(0x20);
const PIC1_DATA: Port<u8> = Port::new(0x21);
const PIC2_COMMAND: Port<u8> = Port::new(0xA0);
const PIC2_DATA: Port<u8> = Port::new(0xA1);
const PIC_EOI: u8 = 0x20;

pub fn pic_quiesce_disable() {
    unsafe {
        PIC1_DATA.write(0xFF);
        PIC2_DATA.write(0xFF);
        PIC1_COMMAND.write(PIC_EOI);
        PIC2_COMMAND.write(PIC_EOI);
    }
}
