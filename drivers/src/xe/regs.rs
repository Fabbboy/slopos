pub const GMD_ID: usize = 0x0d8c;

pub const GMD_ID_ARCH_MASK: u32 = 0xFFC0_0000;
pub const GMD_ID_RELEASE_MASK: u32 = 0x003F_C000;
pub const GMD_ID_REVID_MASK: u32 = 0x0000_003F;

pub const FORCEWAKE_RENDER: usize = 0x0a278;
pub const FORCEWAKE_ACK_RENDER: usize = 0x0d84;

pub const fn bit(shift: u32) -> u32 {
    1u32 << shift
}

pub const fn reg_field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}
