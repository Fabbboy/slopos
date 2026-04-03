use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::DamageRect;
use slopos_abi::video_traits::VideoResult;
use slopos_abi::DisplayInfo;

/// Task ID of the compositor process, set on first framebuffer flip.
static COMPOSITOR_TASK_ID: AtomicU32 = AtomicU32::new(0);

/// Record the compositor's task ID (called from the fb_flip syscall handler).
#[inline]
pub fn set_compositor_task_id(task_id: u32) {
    COMPOSITOR_TASK_ID.store(task_id, Ordering::Relaxed);
}

/// Get the recorded compositor task ID (0 if none).
#[inline]
pub fn compositor_task_id() -> u32 {
    COMPOSITOR_TASK_ID.load(Ordering::Relaxed)
}

slopos_service_core::define_service! {
    video => VideoServices {
        get_display_info() -> Option<DisplayInfo>;
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
