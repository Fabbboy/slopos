use slopos_mm::mmio::MmioRegion;

#[inline]
pub fn read32(mmio: &MmioRegion, offset: usize) -> u32 {
    mmio.read_u32(offset)
}

#[inline]
pub fn write32(mmio: &MmioRegion, offset: usize, value: u32) {
    mmio.write_u32(offset, value)
}
