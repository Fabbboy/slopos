pub const GMD_ID: usize = 0x0d8c;

pub const GMD_ID_ARCH_MASK: u32 = 0xFFC0_0000;
pub const GMD_ID_RELEASE_MASK: u32 = 0x003F_C000;
pub const GMD_ID_REVID_MASK: u32 = 0x0000_003F;

pub const FORCEWAKE_RENDER: usize = 0x0a278;
pub const FORCEWAKE_ACK_RENDER: usize = 0x0d84;

pub const GTTMMADR_GGTT_OFFSET: usize = 0x800000;
pub const GGTT_PTE_BYTES: usize = 8;
pub const GGTT_TABLE_SIZE_8MB: usize = 8 * 1024 * 1024;
pub const GGTT_TABLE_SIZE_4MB: usize = 4 * 1024 * 1024;
pub const GGTT_PTE_PRESENT: u64 = 1;
pub const GGTT_PTE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
pub const GGTT_START_ENTRY: u32 = 0x1000;

pub const PLANE_CTL_A: usize = 0x70180;
pub const PLANE_STRIDE_A: usize = 0x70188;
pub const PLANE_POS_A: usize = 0x7018c;
pub const PLANE_SIZE_A: usize = 0x70190;
pub const PLANE_SURF_A: usize = 0x7019c;
pub const PLANE_OFFSET_A: usize = 0x701a4;

pub const PLANE_CTL_ENABLE: u32 = 1 << 31;
pub const PLANE_CTL_FORMAT_XRGB_8888: u32 = 4 << 24;
pub const PLANE_STRIDE_ALIGN: u32 = 64;

pub const fn bit(shift: u32) -> u32 {
    1u32 << shift
}

pub const fn reg_field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}
