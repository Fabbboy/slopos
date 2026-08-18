//! Diagnostic-console command over memory state: buddy, per-CPU cache, slab
//! and the TLB quiesce epoch, read together in one command.

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

    // Heap pages are a subset of the buddy's `allocated`, so a charged count
    // above it is a leak in the accounting rather than in the allocator.
    let heap_pages = crate::slab::page::charged_heap_pages();
    kline!(
        kc,
        "heap backing: {} pages charged to the root ({} KiB of the {} allocated)",
        heap_pages,
        (heap_pages as u64) * 4,
        pages.allocated
    );

    slopos_ostd::mm::reclaim::for_each_reclaimer(|name, pages| {
        kline!(kc, "reclaim: {:<18} {} pages", name, pages);
    });

    let (epoch, advance_requested, last_deferred) = crate::mmu::quiesce::stats();
    kline!(
        kc,
        "tlb-quiesce: epoch={} advance_requested={} last_deferred={}",
        epoch,
        advance_requested,
        last_deferred
    );

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
