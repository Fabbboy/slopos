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
//! 4. **`install_kernel_vm_space`** (priority 55) — wraps the live
//!    kernel-master PML4 with `Pcid::KERNEL`. From here on every
//!    kernel-side paging mutation can flow through OSTD's cursor API.
//!
//! Use [`kernel_vm_space`] from any post-init code path; the accessor
//! panics with a clear use-before-init message if invoked before step 4.

use slopos_abi::addr::PhysAddr;
use slopos_ostd::arch::x86_64::cr3::Pcid;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::sync::{OnceLock, SpinLock, LOCK_LEVEL_REGISTRY};

/// Singleton wrapping the live kernel-master PML4. Mutations
/// (cursor_mut, kernel-half map / unmap / protect) happen across
/// every CPU at runtime, so the inner `VmSpace` is guarded by a
/// `SpinLock` — the BSP boot caller takes the lock unconstested,
/// runtime callers serialise.
static KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>> = OnceLock::new();

/// Boot-time installer. Wraps the live kernel-master PML4 (whose paddr
/// is the value `register_kernel_master_pml4` saw at early-init time)
/// with `Pcid::KERNEL` and stores the resulting `VmSpace` in the
/// process-wide singleton.
///
/// # Safety
///
/// Caller asserts:
///
/// 1. [`slopos_ostd::mm::vm_space::register_kernel_master_pml4`] has
///    been called with the same paddr that CR3 still holds (no CR3
///    swap has happened in between).
/// 2. `init_meta_slots` and `register_frame_allocator` have both run
///    successfully.
/// 3. This function has not been called before — installation is
///    one-shot via the underlying `OnceLock`.
pub unsafe fn install_kernel_vm_space(pml4_phys: PhysAddr) {
    assert!(
        !KERNEL_VM_SPACE.is_completed(),
        "install_kernel_vm_space called twice"
    );
    KERNEL_VM_SPACE.call_once(|| {
        // SAFETY: caller's contract — pml4_phys is the live kernel
        // master PML4, META_SLOTS / FrameAlloc are registered.
        let space = unsafe { VmSpace::wrap_existing(pml4_phys, Pcid::KERNEL) }
            .expect("install_kernel_vm_space: wrap_existing failed (pml4 slot already TYPED?)");
        // LOCK_LEVEL_REGISTRY: kernel-half mutations sit between
        // resource (per-process state) and allocator levels — they
        // touch the kernel master PML4 which is shared registry-style
        // across every address space.
        SpinLock::new(space, LOCK_LEVEL_REGISTRY)
    });
}

/// Read accessor. Panics with a clear message if invoked before
/// [`install_kernel_vm_space`] has run. Returns the `SpinLock`-guarded
/// VmSpace; callers `.lock()` to get a mutable handle.
pub fn kernel_vm_space() -> &'static SpinLock<VmSpace> {
    KERNEL_VM_SPACE
        .get()
        .expect("kernel_vm_space() called before install_kernel_vm_space()")
}

/// Test-friendly variant: returns `None` instead of panicking when
/// the singleton hasn't been installed yet. Production paths should
/// stick to [`kernel_vm_space`] for the use-before-init razor.
pub fn try_kernel_vm_space() -> Option<&'static SpinLock<VmSpace>> {
    KERNEL_VM_SPACE.get()
}
