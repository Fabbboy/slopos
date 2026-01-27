use slopos_mm::mmio::MmioRegion;

use super::regs;

pub fn xe_display_program_primary(
    mmio: &MmioRegion,
    ggtt_addr: u64,
    width: u32,
    height: u32,
    pitch: u32,
) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    if pitch == 0 || pitch % regs::PLANE_STRIDE_ALIGN != 0 {
        return false;
    }

    if height == 0 || width == 0 {
        return false;
    }

    let stride = pitch / regs::PLANE_STRIDE_ALIGN;
    let size = ((height - 1) << 16) | (width - 1);
    let addr = ggtt_addr as u32;

    mmio.write_u32(regs::PLANE_POS_A, 0);
    mmio.write_u32(regs::PLANE_SIZE_A, size);
    mmio.write_u32(regs::PLANE_STRIDE_A, stride);
    mmio.write_u32(regs::PLANE_OFFSET_A, 0);
    mmio.write_u32(regs::PLANE_SURF_A, addr);

    let ctl = regs::PLANE_CTL_ENABLE | regs::PLANE_CTL_FORMAT_XRGB_8888;
    mmio.write_u32(regs::PLANE_CTL_A, ctl);

    let _ = mmio.read_u32(regs::PLANE_SURF_A);
    true
}

pub fn xe_display_flush(mmio: &MmioRegion, ggtt_addr: u64) -> bool {
    let addr = ggtt_addr as u32;
    mmio.write_u32(regs::PLANE_SURF_A, addr);
    let _ = mmio.read_u32(regs::PLANE_SURF_A);
    true
}
