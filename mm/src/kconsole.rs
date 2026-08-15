//! Diagnostic-console command over memory state.
//!
//! One command rather than several: buddy, per-CPU cache, slab and the TLB
//! quiesce epoch are the four numbers that answer "is memory the reason this
//! machine has stopped", and reading them together is what makes them useful.

use slopos_ostd::kconsole::{KCMD_INFORMATIONAL, KConsole};
use slopos_ostd::kline;

slopos_ostd::kcommand! {
    name = memory,
    key = b'm',
    help = "buddy / per-CPU cache / slab / TLB-quiesce state",
    flags = KCMD_INFORMATIONAL,
    run = run_memory,
}

fn run_memory(kc: &mut KConsole<'_>) {
    let pages = crate::page_alloc::get_page_allocator_stats();
    kline!(
        kc,
        "pages: total={} free={} allocated={} ({} KiB free)",
        pages.total,
        pages.free,
        pages.allocated,
        (pages.free as u64) * 4
    );

    let heap = crate::slab::compat::get_heap_stats_owned();
    kline!(
        kc,
        "heap: {}/{} bytes in {}/{} blocks, {} allocs {} frees",
        heap.allocated_size,
        heap.total_size,
        heap.allocated_blocks,
        heap.total_blocks,
        heap.allocation_count,
        heap.free_count
    );

    // The heap's own backing, charged to the root as `KernelMeta`. Printed
    // beside the buddy's `allocated` because the pair is what makes the root's
    // row reconcilable: heap pages are a subset of allocated ones, so a
    // charged count above `allocated` is a leak in the accounting rather than
    // in the allocator.
    let heap_pages = crate::slab::page::charged_heap_pages();
    kline!(
        kc,
        "heap backing: {} pages charged to the root ({} KiB of the {} allocated)",
        heap_pages,
        (heap_pages as u64) * 4,
        pages.allocated
    );

    let (epoch, advance_requested, last_deferred) = crate::mmu::quiesce::stats();
    kline!(
        kc,
        "tlb-quiesce: epoch={} advance_requested={} last_deferred={}",
        epoch,
        advance_requested,
        last_deferred
    );

    // Bounded by CPU count. A per-CPU cache that stopped draining is the shape
    // an allocator stall takes before it becomes an out-of-memory panic.
    for cpu in 0..slopos_ostd::cpu::x86_64::pcr::get_cpu_count() {
        let Some(pcp) = crate::page_alloc::get_pcp_stats(cpu) else {
            continue;
        };
        kline!(
            kc,
            "  cpu {}: pcp held={} allocs={} frees={}",
            cpu,
            pcp.count,
            pcp.allocs,
            pcp.frees
        );
    }
}
