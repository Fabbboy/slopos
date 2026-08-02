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
//! Wiring is one-shot at boot — the boot caller in
//! `boot::boot_memory::boot_step_register_luf_hook_fn` swaps a
//! `&'static &'static dyn CursorUnmapHook` into OSTD's internal slot
//! by passing `LUF_HOOK_REF` to `register_cursor_unmap_hook`. After
//! that point, every cursor unmap on a user-half leaf and every
//! address-space activation routes through this module.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::vm_space::CursorUnmapHook;

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
        let _ = paddr;
        luf::queue_unmap(vaddr);
    }

    fn on_activate(&self, mm_ctx_handle: u64) {
        luf::current_cpu_set_active_mm_ctx(mm_ctx_handle);
    }
}

/// Process-wide instance. The double-reference is what
/// `register_cursor_unmap_hook` expects — the outer `&'static`
/// stores a vtable pointer, the inner `&'static dyn ...` stores the
/// trait object's pointer pair. This pattern matches the other
/// one-shot OSTD registrations (frame allocator, preempt backend, …).
/// `pub` because the boot caller in
/// `boot::boot_memory::boot_step_register_luf_hook_fn` registers it
/// inline (the former `register_with_ostd(token)` shim has been
/// inlined, taking `&BspToken<'_>` from the boot ctx).
static LUF_HOOK: LufHook = LufHook;
pub static LUF_HOOK_REF: &'static dyn CursorUnmapHook = &LUF_HOOK;
