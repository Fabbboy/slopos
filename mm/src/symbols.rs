use core::ffi::c_void;

/// Access to linker-provided section symbols, isolated here so other modules
/// avoid raw `extern "C"` declarations.
mod externs {
    use core::ffi::c_void;

    unsafe extern "C" {
        pub(crate) static _kernel_start: c_void;
        pub(crate) static _kernel_end: c_void;

        pub(crate) static _user_text_start: u8;
        pub(crate) static _user_text_end: u8;
        pub(crate) static _user_rodata_start: u8;
        pub(crate) static _user_rodata_end: u8;
        pub(crate) static _user_data_start: u8;
        pub(crate) static _user_data_end: u8;
        pub(crate) static _user_bss_start: u8;
        pub(crate) static _user_bss_end: u8;
    }
}

#[inline]
pub fn kernel_bounds() -> (*const c_void, *const c_void) {
    unsafe { (&externs::_kernel_start, &externs::_kernel_end) }
}

#[inline]
pub fn user_text_bounds() -> (*const u8, *const u8) {
    unsafe { (&externs::_user_text_start, &externs::_user_text_end) }
}

#[inline]
pub fn user_rodata_bounds() -> (*const u8, *const u8) {
    unsafe { (&externs::_user_rodata_start, &externs::_user_rodata_end) }
}

#[inline]
pub fn user_data_bounds() -> (*const u8, *const u8) {
    unsafe { (&externs::_user_data_start, &externs::_user_data_end) }
}

#[inline]
pub fn user_bss_bounds() -> (*const u8, *const u8) {
    unsafe { (&externs::_user_bss_start, &externs::_user_bss_end) }
}
