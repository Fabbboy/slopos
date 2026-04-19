//! Shootdown façade.
//!
//! Re-exports the `mm::tlb` public surface under the
//! `mm::mmu::shootdown` path so callers can depend on the modern
//! address without being tied to the legacy module. The underlying
//! implementation (Amit-style early ACK + concurrent local flush)
//! lives in `mm::tlb`.

pub use crate::tlb::{
    SendIpiFn, TLB_SHOOTDOWN_VECTOR, TlbFlushBatch, enter_lazy_tlb, exit_lazy_tlb, flush_all,
    flush_all_for_process, flush_asid, flush_page, flush_page_for_process, flush_range,
    flush_range_for_process, handle_shootdown_ipi, init, notify_cpu_offline, notify_cpu_online,
    notify_cpu_online_id, notify_mm_switch, register_ipi_sender, register_process_tlb,
    should_flush_tlb, unregister_process_tlb,
};
