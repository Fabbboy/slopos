use core::ffi::c_void;

#[inline]
pub fn kernel_bounds() -> (*const c_void, *const c_void) {
    let range = slopos_ostd::arch::x86_64::linker::kernel_image_range();
    (range.start as *const c_void, range.end as *const c_void)
}
