use core::ffi::c_int;

use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::DamageRect;
use slopos_abi::video_traits::VideoResult;
use slopos_abi::DisplayInfo;
use slopos_abi::WindowInfo;

slopos_service_core::define_service! {
    video => VideoServices {
        get_display_info() -> Option<DisplayInfo>;
        surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32;
        @no_wrapper fb_flip(phys_addr: PhysAddr, size: usize, damage: *const DamageRect, damage_count: u32) -> c_int;
        @no_wrapper roulette_draw(fate: u32) -> VideoResult;
    }
}

#[inline(always)]
pub fn fb_flip_from_shm(
    phys_addr: PhysAddr,
    size: usize,
    damage: *const DamageRect,
    damage_count: u32,
) -> c_int {
    (video_services().fb_flip)(phys_addr, size, damage, damage_count)
}

#[inline(always)]
pub fn roulette_draw(fate: u32) -> VideoResult {
    (video_services().roulette_draw)(fate)
}
