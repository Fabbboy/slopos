//! Managed-resource (`Devres`) + identity-DMA-mapper regression tests.
//!
//! Exercises the Phase-3 resource lifecycle directly over ostd primitives:
//! LIFO drop order, real IRQ-vector and DMA release on bag drop (with no double
//! free), and the boot identity IOMMU mapper (IOVA == phys, plus default-deny
//! when no mapper is registered). The binding-path integration (a probe that
//! acquires through `BoundDevice` then fails, and the registry releasing the
//! bag) lives in `pci_binding.rs`.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::dev::Devres;
use slopos_ostd::irq::IrqAllocator;
use slopos_ostd::mm::dma::{register_identity_dma_mapper_for_test, reset_for_test};
use slopos_ostd::mm::{DmaCoherent, DmaError};
use slopos_testing::{TestResult, fail, pass};

// ---------------------------------------------------------------------------
// LIFO drop order.
// ---------------------------------------------------------------------------

static DROP_SEQ: AtomicU32 = AtomicU32::new(0);
static DROP_LOG: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// Records its `id` into the next `DROP_LOG` slot when dropped.
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
        // Bag drops here: last attached (3) must drop first.
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

// ---------------------------------------------------------------------------
// Real IRQ-vector release on bag drop (no double free).
// ---------------------------------------------------------------------------

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

    // While the binding is held, its vector is in use: a fresh allocation must
    // hand out a different vector.
    let busy = match IrqAllocator::alloc() {
        Ok(l) => l,
        Err(_) => return fail!("vector pool exhausted (held)"),
    };
    if busy.vector() == vector {
        return fail!("held vector {} was handed out again", vector);
    }
    drop(busy);

    // Dropping the bag clears the dispatch slot, then frees the vector.
    drop(res);

    // The freed vector is the lowest free again and must come back, and its
    // dispatch slot must be clear (re-registration succeeds — proves the slot
    // was released, not double-freed or left populated).
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

// ---------------------------------------------------------------------------
// DMA buffer release on bag drop (no double free; re-allocatable).
// ---------------------------------------------------------------------------

pub fn test_devres_releases_dma() -> TestResult {
    register_identity_dma_mapper_for_test();
    {
        let mut res = Devres::new();
        let dma = match DmaCoherent::alloc(2) {
            Ok(d) => d,
            Err(e) => return fail!("DMA alloc failed: {:?}", e),
        };
        if res.attach(dma).is_err() {
            return fail!("attach out of memory");
        }
        // Bag drops here: DmaCoherent unmaps and frees its frames.
    }
    // Frames returned to the pool: a fresh allocation succeeds (and the prior
    // drop did not double-free, or this would have faulted).
    match DmaCoherent::alloc(2) {
        Ok(d) => drop(d),
        Err(e) => return fail!("DMA re-alloc after release failed: {:?}", e),
    }
    pass!()
}

// ---------------------------------------------------------------------------
// Identity IOMMU mapper: IOVA == phys, and default-deny without a mapper.
// ---------------------------------------------------------------------------

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

        // With no mapper registered, allocation is denied — the default-deny
        // posture that holds before the boot wiring runs.
        reset_for_test();
        match DmaCoherent::alloc(2) {
            Err(DmaError::NotInitialised) => pass!(),
            Ok(_) => fail!("expected NotInitialised with no mapper, got Ok"),
            Err(e) => fail!("expected NotInitialised, got {:?}", e),
        }
    })();
    // Restore the mapper for subsequent tests regardless of outcome.
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
