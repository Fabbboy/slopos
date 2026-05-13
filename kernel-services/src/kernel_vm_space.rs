//! Singleton `KArc<VmSpace>` wrapping the live kernel-master PML4.
//!
//! `slopos-ostd` exposes [`VmSpace::wrap_existing`] precisely so the
//! kernel master (built by Limine before `kernel_main`) can be folded
//! into the same cursor / activate / kernel-half-resync machinery as
//! every per-process address space — without rebuilding the master's
//! HHDM tree under OSTD.
//!
//! # Boot sequence
//!
//! 1. `register_kernel_master_pml4(read_cr3())` — `early_init` declares
//!    where the master lives. Happens before `boot_init_run_all`.
//! 2. `boot_step_init_meta_slots` (priority 40) — META_SLOTS array
//!    becomes available, which is the prerequisite for any
//!    [`Frame::<PageTableMeta>::from_unused`] call.
//! 3. `boot_step_register_frame_alloc` (priority 50) — registered
//!    OSTD `FrameAlloc` is the prerequisite for cursor mutations
//!    (intermediate page-table allocation goes through it).
//! 4. **boot_step_install_kernel_vm_space_fn** (priority 55) — wraps
//!    the live kernel-master PML4 with `Pcid::KERNEL`. From here on
//!    every kernel-side paging mutation can flow through OSTD's
//!    cursor API. The wrapping happens inline in the boot caller,
//!    reading the live PML4 paddr from CR3 and threading
//!    `&ctx.bsp_token()` as the witness; the `KERNEL_VM_SPACE` static
//!    below is `pub` so the inline call can reach it.
//!
//! Use [`kernel_vm_space`] from any post-init code path; the accessor
//! panics with a clear use-before-init message if invoked before step 4.

use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::sync::{OnceLock, SpinLock};

/// Singleton wrapping the live kernel-master PML4. Mutations
/// (cursor_mut, kernel-half map / unmap / protect) happen across
/// every CPU at runtime, so the inner `VmSpace` is guarded by a
/// `SpinLock` — the BSP boot caller takes the lock unconstested,
/// runtime callers serialise. `pub` because the boot caller in
/// `boot::boot_memory::boot_step_install_kernel_vm_space_fn` calls
/// `KERNEL_VM_SPACE.call_once(...)` inline.
pub static KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>> = OnceLock::new();

/// Read accessor. Panics with a clear message if invoked before
/// `boot_step_install_kernel_vm_space_fn` has run. Returns the
/// `SpinLock`-guarded VmSpace; callers `.lock()` to get a mutable
/// handle.
pub fn kernel_vm_space() -> &'static SpinLock<VmSpace> {
    KERNEL_VM_SPACE
        .get()
        .expect("kernel_vm_space() called before boot_step_install_kernel_vm_space_fn")
}

/// Test-friendly variant: returns `None` instead of panicking when
/// the singleton hasn't been installed yet. Production paths should
/// stick to [`kernel_vm_space`] for the use-before-init razor.
pub fn try_kernel_vm_space() -> Option<&'static SpinLock<VmSpace>> {
    KERNEL_VM_SPACE.get()
}

/// Park the current CPU on the kernel master VM after a fatal user
/// fault. Wraps [`VmSpace::activate`] in the post-fault context where
/// the kernel-half invariant is trivially satisfied (we're switching
/// onto the master itself), so the safety contract is upheld locally
/// and the unsafe block stays inside this crate.
///
/// Returns `true` if the kernel VM was installed and CR3 was switched;
/// `false` when `boot_step_install_kernel_vm_space_fn` has not yet run
/// (pre-init fault — the caller falls back to halting on whatever PML4
/// is already loaded).
pub fn activate_post_user_fault() -> bool {
    let Some(slot) = try_kernel_vm_space() else {
        return false;
    };
    // SAFETY: post-fault, irqs masked at the IDT trampoline; the
    // kernel master always satisfies `activate`'s kernel-half
    // contract because the user-half indices are unused on it.
    unsafe { slot.lock().activate() };
    true
}
