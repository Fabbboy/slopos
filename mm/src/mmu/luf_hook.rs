//! `CursorUnmapHook` adapter that bridges OSTD `VmSpace` cursor unmaps
//! and `VmSpace::activate` calls into slopos-mm's Lazy-Unmap-Flush
//! ring.
//!
//! OSTD invokes [`CursorUnmapHook::after_unmap`] at the end of every
//! [`CursorMut::unmap`] whose leaf carried the `USER` bit, and
//! [`CursorUnmapHook::on_activate`] at the start of every
//! [`VmSpace::activate`]. The hook is consumer-defined precisely so
//! the OSTD core stays free of TLB-policy: slopos-mm decides whether
//! to queue, broadcast immediately, or coalesce.
//!
//! Wiring is one-shot at boot — `register_with_ostd` swaps a
//! `&'static &'static dyn CursorUnmapHook` into OSTD's internal slot.
//! After that point, every cursor unmap on a user-half leaf and every
//! address-space activation routes through this module.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::vm_space::{CursorUnmapHook, register_cursor_unmap_hook};

use super::cr3::MmContextId;
use super::luf;

/// Zero-sized hook implementation. Holds no state — every callback
/// translates directly into a slopos-mm LUF call.
pub struct LufHook;

impl CursorUnmapHook for LufHook {
    fn after_unmap(&self, vaddr: VirtAddr, paddr: PhysAddr, mm_ctx_handle: u64) {
        // Hook fires for kernel-master cursor mutations too — the kernel
        // master's `mm_ctx_handle` is `0` (unset), since
        // `set_mm_ctx_handle` is only called from `create_process_vm`.
        // LUF entries are useful only for per-process address spaces
        // whose deferred flush rides the per-CPU PCID rotation; kernel-
        // master leaves are GLOBAL and would never sit in the queue
        // anyway, so the early-out below also keeps the queue free of
        // entries that drain logic could not interpret.
        if mm_ctx_handle == 0 {
            return;
        }
        // PCID `0` here is a placeholder — the LUF drain consults the
        // active PCID via the per-CPU slot binding (see `luf::drain_*`
        // call sites). The plumbing accepts a `u16` for symmetry with
        // future `INVPCID type 0` per-entry refinement.
        luf::queue_unmap(vaddr, paddr, MmContextId::from_raw(mm_ctx_handle), 0);
    }

    fn on_activate(&self, mm_ctx_handle: u64) {
        luf::current_cpu_set_active_mm_ctx(mm_ctx_handle);
    }
}

/// Process-wide instance. The double-reference is what
/// [`register_cursor_unmap_hook`] expects — the outer `&'static`
/// stores a vtable pointer, the inner `&'static dyn ...` stores the
/// trait object's pointer pair. This pattern matches the other
/// one-shot OSTD registrations (frame allocator, preempt backend, …).
static LUF_HOOK: LufHook = LufHook;
static LUF_HOOK_REF: &'static dyn CursorUnmapHook = &LUF_HOOK;

/// Boot-time installer. Hands OSTD's `VmSpace` machinery this module's
/// hook so that all subsequent cursor unmaps and activations fire into
/// slopos-mm's LUF.
///
/// # Safety
///
/// One-shot — OSTD's `register_cursor_unmap_hook` panics on second
/// call. Caller must invoke after [`super::luf`] is reachable (LUF
/// state is in zero-initialised statics, so this is implicitly true
/// from boot start).
pub unsafe fn register_with_ostd() {
    // SAFETY: `LUF_HOOK_REF` lives in `static` storage with `'static`
    // lifetime; `LufHook` is `Send + Sync` (zero-sized, no interior
    // state). Concurrent invocation across CPUs is sound because
    // `after_unmap` and `on_activate` only touch atomics and
    // per-CPU LUF state keyed by the caller's CPU.
    unsafe {
        register_cursor_unmap_hook(&LUF_HOOK_REF);
    }
}
