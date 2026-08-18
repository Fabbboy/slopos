//! Singleton `VmSpace` wrapping the live kernel-master PML4 (built by Limine
//! before `kernel_main`) via [`VmSpace::wrap_existing`], so kernel-side paging
//! flows through the same cursor / activate machinery as every per-process
//! address space without rebuilding the master's HHDM tree under OSTD.
//!
//! Installed by `boot_step_install_kernel_vm_space_fn` (priority 55), after the
//! META_SLOTS (40) and OSTD `FrameAlloc` (50) boot steps it depends on.

use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::sync::{OnceLock, SpinLock};

/// `SpinLock` because kernel-half mutations happen from every CPU at runtime;
/// `pub` so the boot caller can `call_once` it inline.
pub static KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>> = OnceLock::new();

/// Panics if invoked before `boot_step_install_kernel_vm_space_fn` has run.
pub fn kernel_vm_space() -> &'static SpinLock<VmSpace> {
    KERNEL_VM_SPACE
        .get()
        .expect("kernel_vm_space() called before boot_step_install_kernel_vm_space_fn")
}

/// Returns `None` instead of panicking when the singleton is not installed.
/// Production paths should use [`kernel_vm_space`] for the use-before-init razor.
pub fn try_kernel_vm_space() -> Option<&'static SpinLock<VmSpace>> {
    KERNEL_VM_SPACE.get()
}

/// Park the current CPU on the kernel master VM after a fatal user fault. The
/// kernel-half invariant [`VmSpace::activate_kernel_master`] needs holds
/// trivially here: the target is the master itself.
///
/// Returns `false` when `boot_step_install_kernel_vm_space_fn` has not yet run,
/// leaving the caller to halt on whatever PML4 is already loaded.
pub fn activate_post_user_fault() -> bool {
    let Some(slot) = try_kernel_vm_space() else {
        return false;
    };
    slot.lock().activate_kernel_master();
    true
}
