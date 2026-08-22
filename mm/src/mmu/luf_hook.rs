//! `CursorUnmapHook` adapter bridging OSTD `VmSpace` cursor unmaps and
//! activations into slopos-mm's Lazy-Unmap-Flush ring, so TLB policy stays out
//! of the OSTD core. Wiring is one-shot at boot, from
//! `boot::boot_memory::boot_step_register_luf_hook_fn`.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::vm_space::CursorUnmapHook;

use super::luf;

pub struct LufHook;

impl CursorUnmapHook for LufHook {
    fn after_unmap(&self, vaddr: VirtAddr, paddr: PhysAddr, mm_ctx_handle: u64) {
        // Unconditional, including the kernel master (`mm_ctx_handle == 0`):
        // gating on the handle would let an address space that never got one
        // skip arming the quarantine, releasing a frame that still needed it.
        let _ = (paddr, mm_ctx_handle);
        luf::queue_unmap(vaddr);
    }

    fn on_activate(&self, mm_ctx_handle: u64) {
        luf::current_cpu_set_active_mm_ctx(mm_ctx_handle);
    }

    fn select_cr3(&self, mm_ctx_handle: u64, tlb_gen: u64) -> Option<(u16, bool)> {
        super::asid::select_pcid_for_activate(super::MmContextId::from_raw(mm_ctx_handle), tlb_gen)
    }
}

/// The double reference is what `register_cursor_unmap_hook` expects.
static LUF_HOOK: LufHook = LufHook;
pub static LUF_HOOK_REF: &'static dyn CursorUnmapHook = &LUF_HOOK;
