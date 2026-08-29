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

slopos_ostd::kcommand! {
    name = cachetypes,
    key = b'c',
    help = "per-CPU PAT / MTRR / CR0 / CR4 memory-type census",
    flags = KCMD_INFORMATIONAL,
    run = run_cache_types,
}

fn run_cache_types(kc: &mut KConsole<'_>) {
    use crate::cache_census::{FB_PAT_INDEX, cpu_state, expected_pat, memory_type_name, pat_entry};

    let expected = expected_pat();
    kline!(
        kc,
        "expected PAT=0x{:016x} (index {} = {})",
        expected,
        FB_PAT_INDEX,
        memory_type_name(pat_entry(expected, FB_PAT_INDEX))
    );

    let mut divergent = 0u32;
    let mut unreported = 0u32;
    for cpu in 0..slopos_ostd::cpu::x86_64::pcr::get_cpu_count() {
        let Some(state) = cpu_state(cpu) else {
            unreported += 1;
            kline!(kc, "  cpu {}: no sample (never ticked)", cpu);
            continue;
        };
        let fb_type = pat_entry(state.pat, FB_PAT_INDEX);
        let ok = state.pat == expected;
        if !ok {
            divergent += 1;
        }
        let mtrr = if state.mtrr_def_type == u64::MAX {
            "absent"
        } else if state.mtrr_def_type & (1 << 11) != 0 {
            "enabled"
        } else {
            "DISABLED"
        };
        kline!(
            kc,
            "  cpu {:>2}: PAT=0x{:016x} fb={} {} mtrr={} def={} cr0=0x{:x} cr4=0x{:x}",
            cpu,
            state.pat,
            memory_type_name(fb_type),
            if ok { "ok" } else { "DIVERGENT" },
            mtrr,
            if state.mtrr_def_type == u64::MAX {
                "--"
            } else {
                memory_type_name((state.mtrr_def_type & 0xFF) as u8)
            },
            state.cr0,
            state.cr4
        );
    }

    if divergent == 0 && unreported == 0 {
        kline!(kc, "verdict: every CPU carries the expected PAT");
    } else {
        kline!(
            kc,
            "verdict: {} CPU(s) DIVERGENT, {} unreported -- framebuffer writes on those CPUs are not write-combining",
            divergent,
            unreported
        );
    }
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
