//! Managed-resource (`Devres`) and identity-DMA-mapper regression tests.
//!
//! The DMA release test asserts on slot state and on the run page account: the
//! buddy absorbs a double free without faulting, so "it did not crash" is no
//! evidence.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_mm::paging_defs::PAGE_SIZE_4KB_USIZE;
use slopos_ostd::dev::Devres;
use slopos_ostd::irq::IrqAllocator;
use slopos_ostd::mm::dma::{
    MapperHiddenForTest, register_identity_dma_mapper_for_test, run_page_account,
};
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

    // What the release owes is that the vector is claimable again and that its
    // dispatch slot is empty — not that the allocator's next pick lands on it.
    let reclaimed = match IrqAllocator::reserve_specific(vector) {
        Ok(l) => l,
        Err(e) => return fail!("released vector {} is not free again: {:?}", vector, e),
    };
    match reclaimed.register_callback_owned(|_ctx| {}) {
        Ok(o) => drop(o),
        Err(_) => return fail!("dispatch slot not cleared on release"),
    }
    pass!()
}

const DMA_RUN_PAGES: usize = 2;

/// Repetition is stress, not statistics: the account below is exact, so one
/// leaked round would already show.
const DMA_CHURN_ROUNDS: usize = 1024;

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

    // Accounted on the runs themselves, not on the page allocator's free count:
    // that count moves for every CPU, so a neighbour's allocation and a leak
    // read alike.
    let (taken_before, returned_before) = run_page_account();
    for round in 0..DMA_CHURN_ROUNDS {
        match DmaCoherent::alloc(DMA_RUN_PAGES) {
            Ok(d) => drop(d),
            Err(e) => return fail!("DMA alloc failed at churn round {}: {:?}", round, e),
        }
    }
    let (taken_after, returned_after) = run_page_account();

    let taken = taken_after.saturating_sub(taken_before);
    let returned = returned_after.saturating_sub(returned_before);
    let expected = (DMA_CHURN_ROUNDS * DMA_RUN_PAGES) as u64;
    if taken < expected {
        return fail!(
            "the run account saw only {} pages taken across {} rounds of {} pages",
            taken,
            DMA_CHURN_ROUNDS,
            DMA_RUN_PAGES
        );
    }
    let outstanding = taken.saturating_sub(returned);
    if outstanding != 0 {
        return fail!(
            "{} of the {} DMA pages taken during the churn were never handed back",
            outstanding,
            taken
        );
    }
    pass!()
}

pub fn test_identity_mapper_iova_and_default_deny() -> TestResult {
    register_identity_dma_mapper_for_test();

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

    // Hidden from this CPU rather than unregistered: a driver allocating DMA on
    // another CPU must not see this test's deny.
    let hidden = MapperHiddenForTest::for_current_cpu();
    let denied = DmaCoherent::alloc(2);
    drop(hidden);

    match denied {
        Err(DmaError::NotInitialised) => pass!(),
        Ok(_) => fail!("expected NotInitialised with the mapper hidden, got Ok"),
        Err(e) => fail!("expected NotInitialised, got {:?}", e),
    }
}

slopos_testing::stest!(name = test_devres_lifo_drop_order, suite = devres);
slopos_testing::stest!(name = test_devres_releases_irq_vector, suite = devres);
slopos_testing::stest!(name = test_devres_releases_dma, suite = devres);
slopos_testing::stest!(
    name = test_identity_mapper_iova_and_default_deny,
    suite = devres
);
