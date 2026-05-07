use slopos_ostd::io::port::IoPortRegistry;

const PIC_EOI: u8 = 0x20;

pub fn pic_quiesce_disable() {
    let pic1_cmd = IoPortRegistry::reserve::<u8>(0x20).expect("PIC1 command port");
    let pic1_data = IoPortRegistry::reserve::<u8>(0x21).expect("PIC1 data port");
    let pic2_cmd = IoPortRegistry::reserve::<u8>(0xA0).expect("PIC2 command port");
    let pic2_data = IoPortRegistry::reserve::<u8>(0xA1).expect("PIC2 data port");
    unsafe {
        pic1_data.write(0xFF);
        pic2_data.write(0xFF);
        pic1_cmd.write(PIC_EOI);
        pic2_cmd.write(PIC_EOI);
    }
}
