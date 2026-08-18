//! Managed-resource (`Devres`) and identity-DMA-mapper regression tests; the
//! binding-path integration lives in `pci_binding.rs`.
//!
//! The DMA release test asserts on slot state and frame accounting: the buddy
//! absorbs a double free without faulting, so "it did not crash" is no evidence.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::paging_defs::PAGE_SIZE_4KB_USIZE;
use slopos_ostd::dev::Devres;
use slopos_ostd::irq::IrqAllocator;
use slopos_ostd::mm::dma::{register_identity_dma_mapper_for_test, reset_for_test};
use slopos_ostd::mm::frame::{SlotMetaKind, slot_snapshot};
use slopos_ostd::mm::{DmaCoherent, DmaError};
use slopos_testing::{TestResult, fail, pass};

static DROP_SEQ: AtomicU32 = AtomicU32::new(0);
static DROP_LOG: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

struct DropSentinel {
    id: u32,
}

impl Drop for DropSentinel {
    fn drop(&mut self) {
        let pos = DROP_SEQ.fetch_add(1, Ordering::Relaxed) as usize;
        if pos < DROP_LOG.len() {
            DROP_LOG[pos].store(self.id, Ordering::Relaxed);
        }
    }
}

pub fn test_devres_lifo_drop_order() -> TestResult {
    DROP_SEQ.store(0, Ordering::Relaxed);
    {
        let mut res = Devres::new();
        for id in 1..=3u32 {
            if res.attach(DropSentinel { id }).is_err() {
                return fail!("attach out of memory");
            }
        }
        if res.len() != 3 {
            return fail!("expected 3 resources, got {}", res.len());
        }
    }
    let order = [
        DROP_LOG[0].load(Ordering::Relaxed),
        DROP_LOG[1].load(Ordering::Relaxed),
        DROP_LOG[2].load(Ordering::Relaxed),
    ];
    if order != [3, 2, 1] {
        return fail!("expected LIFO drop order [3,2,1], got {:?}", order);
    }
    pass!()
}

pub fn test_devres_releases_irq_vector() -> TestResult {
    let mut res = Devres::new();
    let line = match IrqAllocator::alloc() {
        Ok(l) => l,
        Err(_) => return fail!("vector pool exhausted"),
    };
    let vector = line.vector();
    let owned = match line.register_callback_owned(|_ctx| {}) {
        Ok(o) => o,
        Err(_) => return fail!("callback install failed"),
    };
    if res.attach(owned).is_err() {
        return fail!("attach out of memory");
    }

    let busy = match IrqAllocator::alloc() {
        Ok(l) => l,
        Err(_) => return fail!("vector pool exhausted (held)"),
    };
    if busy.vector() == vector {
        return fail!("held vector {} was handed out again", vector);
    }
    drop(busy);

    drop(res);

    // The freed vector is the lowest free one again, so it must come back; the
    // re-registration below is what proves its dispatch slot was cleared.
    let reclaimed = match IrqAllocator::alloc() {
        Ok(l) => l,
        Err(_) => return fail!("vector pool exhausted (after release)"),
    };
    if reclaimed.vector() != vector {
        return fail!(
            "freed vector {} not reclaimed (got {})",
            vector,
            reclaimed.vector()
        );
    }
    match reclaimed.register_callback_owned(|_ctx| {}) {
        Ok(o) => drop(o),
        Err(_) => return fail!("dispatch slot not cleared on release"),
    }
    pass!()
}

const DMA_RUN_PAGES: usize = 2;

/// Enough rounds that a leak costs two orders of magnitude more frames than
/// `DMA_ACCOUNTING_SLACK`.
const DMA_CHURN_ROUNDS: usize = 1024;

/// Frames the free count may legitimately move by: another CPU's allocations and
/// any slab growth the churn itself provokes.
const DMA_ACCOUNTING_SLACK: u32 = 256;

fn free_frames() -> u32 {
    let free = get_page_allocator_stats().free;
    free
}

pub fn test_devres_releases_dma() -> TestResult {
    register_identity_dma_mapper_for_test();

    let head;
    {
        let mut res = Devres::new();
        let dma = match DmaCoherent::alloc(DMA_RUN_PAGES) {
            Ok(d) => d,
            Err(e) => return fail!("DMA alloc failed: {:?}", e),
        };
        head = dma.phys_base();
        if res.attach(dma).is_err() {
            return fail!("attach out of memory");
        }
    }

    // A page reaching the free lists while its slot still reads `DmaCoherent`
    // would fail the next owner's `from_unused`.
    for i in 0..DMA_RUN_PAGES {
        let paddr = PhysAddr::new(head.as_u64() + (i * PAGE_SIZE_4KB_USIZE) as u64);
        let kind = slot_snapshot(paddr).kind;
        if kind != SlotMetaKind::Unused {
            return fail!(
                "slot for page {} of the released run reads {:?}, want Unused",
                i,
                kind
            );
        }
    }

    // One round trip proves nothing while the allocator still has free memory;
    // churn until a leak would dwarf the noise a concurrent CPU contributes.
    let free_before = free_frames();
    for round in 0..DMA_CHURN_ROUNDS {
        match DmaCoherent::alloc(DMA_RUN_PAGES) {
            Ok(d) => drop(d),
            Err(e) => return fail!("DMA alloc failed at churn round {}: {:?}", round, e),
        }
    }
    let free_after = free_frames();
    let lost = free_before.saturating_sub(free_after);
    if lost > DMA_ACCOUNTING_SLACK {
        return fail!(
            "{} DMA runs of {} pages lost {} frames (free {} -> {}); the runs are not being released",
            DMA_CHURN_ROUNDS,
            DMA_RUN_PAGES,
            lost,
            free_before,
            free_after
        );
    }
    pass!()
}

pub fn test_identity_mapper_iova_and_default_deny() -> TestResult {
    register_identity_dma_mapper_for_test();
    let result = (|| {
        let dma = match DmaCoherent::alloc(2) {
            Ok(d) => d,
            Err(e) => return fail!("DMA alloc with identity mapper failed: {:?}", e),
        };
        if dma.iova() != dma.phys_base().as_u64() {
            return fail!(
                "identity mapper: iova {:#x} != phys {:#x}",
                dma.iova(),
                dma.phys_base().as_u64()
            );
        }
        drop(dma);

        reset_for_test();
        match DmaCoherent::alloc(2) {
            Err(DmaError::NotInitialised) => pass!(),
            Ok(_) => fail!("expected NotInitialised with no mapper, got Ok"),
            Err(e) => fail!("expected NotInitialised, got {:?}", e),
        }
    })();
    // Subsequent tests need the mapper back regardless of the outcome here.
    register_identity_dma_mapper_for_test();
    result
}

slopos_testing::stest!(name = test_devres_lifo_drop_order, suite = devres);
slopos_testing::stest!(name = test_devres_releases_irq_vector, suite = devres);
slopos_testing::stest!(name = test_devres_releases_dma, suite = devres);
slopos_testing::stest!(
    name = test_identity_mapper_iova_and_default_deny,
    suite = devres
);
