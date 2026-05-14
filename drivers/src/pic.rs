use slopos_ostd::io::Pic;
use slopos_ostd::io::port::IoPortRegistry;

pub fn pic_quiesce_disable() {
    let pic = Pic::new(
        IoPortRegistry::reserve::<u8>(0x20).expect("PIC1 command port"),
        IoPortRegistry::reserve::<u8>(0x21).expect("PIC1 data port"),
        IoPortRegistry::reserve::<u8>(0xA0).expect("PIC2 command port"),
        IoPortRegistry::reserve::<u8>(0xA1).expect("PIC2 data port"),
    );
    pic.quiesce_disable();
}
