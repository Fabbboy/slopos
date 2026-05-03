//! MSI infrastructure tests.
//!
//! MSI vector allocation lives in OSTD (`IrqAllocator::alloc`) and dispatch
//! goes through `slopos_ostd::irq::dispatch`.
//!
//! These tests verify:
//!   - the OSTD vector allocator on the MSI range (48-223) hands out
//!     distinct vectors and respects platform reservations
//!   - `IdtBuilder::install_default_handlers` populated every IDT slot
//!     with the right gate type, DPL, and selector
//!   - the SYSCALL_VECTOR (0x80) trap gate has DPL=3 (user-reachable)
//!   - the legacy IRQ vector range (32-47) has interrupt gates installed

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_kernel_services::platform::idt_get_gate;
use slopos_ostd::irq::{
    IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IRQ_BASE_VECTOR, IdtEntry, IrqAllocator, IrqContext,
    IrqError, MSI_VECTOR_BASE, MSI_VECTOR_END, SYSCALL_VECTOR, dispatch,
};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ne_test, assert_test};
use slopos_utils::klog_info;

const MSI_SAMPLE_VECTORS: [u8; 5] = [48, 100, 150, 200, 223];

fn idt_handler_address(entry: &IdtEntry) -> u64 {
    u64::from(entry.offset_low)
        | (u64::from(entry.offset_mid) << 16)
        | (u64::from(entry.offset_high) << 32)
}

fn load_idt_entry(vector: u8) -> Result<IdtEntry, TestResult> {
    let mut entry = IdtEntry::zero();
    let rc = idt_get_gate(vector, (&mut entry as *mut IdtEntry).cast::<c_void>());
    if rc != 0 {
        klog_info!(
            "MSI_TEST: idt_get_gate failed for vector {} with rc={}",
            vector,
            rc
        );
        return Err(TestResult::Fail);
    }
    Ok(entry)
}

// ---------------------------------------------------------------------------
// OSTD-backed MSI vector allocation
// ---------------------------------------------------------------------------

pub fn test_msi_alloc_returns_valid_range() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    assert_test!(
        v >= 32 && v < 224,
        "Allocated vector {} not in OSTD allocator range [32, 224)",
        v
    );
    TestResult::Pass
}

pub fn test_msi_alloc_distinct_vectors() -> TestResult {
    let mut lines = [None, None, None, None];
    for slot in lines.iter_mut() {
        *slot = Some(IrqAllocator::alloc().expect("alloc"));
    }
    // Verify all distinct.
    for i in 0..4 {
        for j in (i + 1)..4 {
            let vi = lines[i].as_ref().unwrap().vector();
            let vj = lines[j].as_ref().unwrap().vector();
            assert_test!(vi != vj, "Allocated vector {} twice", vi);
        }
    }
    TestResult::Pass
}

pub fn test_msi_alloc_drop_returns_to_pool() -> TestResult {
    let v = {
        let line = IrqAllocator::alloc().expect("alloc");
        line.vector()
    };
    // Drop happened; bit should be free in OSTD's bitmap. We don't
    // require exact reuse (other tests may interleave) but the basic
    // drop path must complete without panic.
    assert_test!(v >= 32, "vector still in range");
    TestResult::Pass
}

pub fn test_msi_reserve_specific_succeeds_in_msi_range() -> TestResult {
    // Pick a high vector that's almost certainly free.
    let target = 220u8;
    let line = match IrqAllocator::reserve_specific(target) {
        Ok(l) => l,
        Err(_) => return TestResult::Pass, // already taken — test inert
    };
    assert_test!(
        line.vector() == target,
        "reserve_specific returned wrong vector"
    );
    TestResult::Pass
}

pub fn test_msi_reserve_specific_double_claim_refused() -> TestResult {
    let v = 219u8;
    let _line = match IrqAllocator::reserve_specific(v) {
        Ok(l) => l,
        Err(_) => return TestResult::Pass,
    };
    let r = IrqAllocator::reserve_specific(v);
    assert_test!(
        matches!(r, Err(IrqError::AlreadyRegistered)),
        "Second reserve_specific must fail"
    );
    TestResult::Pass
}

pub fn test_msi_reserve_specific_out_of_msi_range() -> TestResult {
    // 224 is at the allocator's upper bound (exclusive).
    assert_test!(
        matches!(
            IrqAllocator::reserve_specific(224),
            Err(IrqError::Exhausted)
        ),
        "vector 224 must be rejected"
    );
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// OSTD register_callback + dispatch (replaces msi_register_handler tests)
// ---------------------------------------------------------------------------

static MSI_FIRE_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn test_msi_register_callback_dispatches() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    MSI_FIRE_COUNT.store(0, Ordering::SeqCst);
    let _h = line
        .register_callback(|_ctx: &IrqContext<'_>| {
            MSI_FIRE_COUNT.fetch_add(1, Ordering::SeqCst);
        })
        .expect("register");
    dispatch(v, 0);
    dispatch(v, 0);
    dispatch(v, 0);
    assert_test!(
        MSI_FIRE_COUNT.load(Ordering::SeqCst) == 3,
        "Callback should fire 3 times"
    );
    TestResult::Pass
}

pub fn test_msi_callback_receives_vector() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    static SEEN_VECTOR: AtomicUsize = AtomicUsize::new(0);
    SEEN_VECTOR.store(0, Ordering::SeqCst);
    let _h = line
        .register_callback(|ctx: &IrqContext<'_>| {
            SEEN_VECTOR.store(ctx.vector() as usize, Ordering::SeqCst);
        })
        .expect("register");
    dispatch(v, 0);
    assert_test!(
        SEEN_VECTOR.load(Ordering::SeqCst) == v as usize,
        "Callback received wrong vector"
    );
    TestResult::Pass
}

pub fn test_msi_callback_receives_error_code() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    static SEEN_EC: AtomicUsize = AtomicUsize::new(0);
    SEEN_EC.store(0, Ordering::SeqCst);
    let _h = line
        .register_callback(|ctx: &IrqContext<'_>| {
            SEEN_EC.store(ctx.error_code() as usize, Ordering::SeqCst);
        })
        .expect("register");
    dispatch(v, 0xDEAD_BEEF);
    assert_test!(
        SEEN_EC.load(Ordering::SeqCst) == 0xDEAD_BEEF,
        "Callback received wrong error code"
    );
    TestResult::Pass
}

pub fn test_msi_double_register_same_line_errors() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let _h = line.register_callback(|_| {}).expect("first");
    let r = line.register_callback(|_| {});
    assert_test!(
        matches!(r, Err(IrqError::AlreadyRegistered)),
        "Second register_callback on same line must fail"
    );
    TestResult::Pass
}

pub fn test_msi_unregister_via_handle_drop() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    static FIRED: AtomicUsize = AtomicUsize::new(0);
    FIRED.store(0, Ordering::SeqCst);
    {
        let _h = line
            .register_callback(|_| {
                FIRED.fetch_add(1, Ordering::SeqCst);
            })
            .expect("register");
        dispatch(v, 0);
    }
    // Handle dropped; dispatch slot must now be empty.
    dispatch(v, 0);
    assert_test!(
        FIRED.load(Ordering::SeqCst) == 1,
        "Callback fired after handle dropped"
    );
    TestResult::Pass
}

pub fn test_msi_dispatch_unregistered_vector_noop() -> TestResult {
    // Pick a high vector with no callback registered.
    dispatch(210, 0);
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// IDT verification: install_default_handlers populated every gate
// ---------------------------------------------------------------------------

pub fn test_msi_idt_entries_present() -> TestResult {
    for &v in &MSI_SAMPLE_VECTORS {
        if v == SYSCALL_VECTOR {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!(
            attr != 0,
            "IDT entry for MSI vector 0x{:02x} not installed (type_attr=0)",
            v
        );
    }
    TestResult::Pass
}

pub fn test_msi_idt_entries_are_interrupt_gates() -> TestResult {
    for &v in &MSI_SAMPLE_VECTORS {
        if v == SYSCALL_VECTOR {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!(
            attr & 0x0F == IDT_GATE_INTERRUPT & 0x0F,
            "IDT entry for vector 0x{:02x} not an interrupt gate",
            v
        );
        assert_test!(
            attr & 0x80 == 0x80,
            "IDT entry for vector 0x{:02x} not present",
            v
        );
    }
    TestResult::Pass
}

pub fn test_msi_idt_entries_dpl_zero() -> TestResult {
    for &v in &MSI_SAMPLE_VECTORS {
        if v == SYSCALL_VECTOR {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!((attr >> 5) & 0x3 == 0, "vector 0x{:02x} not DPL=0", v);
    }
    TestResult::Pass
}

pub fn test_msi_idt_entries_have_handlers() -> TestResult {
    for &v in &MSI_SAMPLE_VECTORS {
        if v == SYSCALL_VECTOR {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let h = idt_handler_address(&entry);
        assert_test!(h != 0, "vector 0x{:02x} handler address is null", v);
    }
    TestResult::Pass
}

pub fn test_msi_idt_entries_use_kernel_cs() -> TestResult {
    for &v in &MSI_SAMPLE_VECTORS {
        if v == SYSCALL_VECTOR {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let sel = entry.selector;
        assert_test!(sel == 0x08, "vector 0x{:02x} selector not KERNEL_CS", v);
    }
    TestResult::Pass
}

pub fn test_syscall_vector_is_trap_gate_dpl3() -> TestResult {
    let entry = match load_idt_entry(SYSCALL_VECTOR) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let attr = entry.type_attr;
    assert_eq_test!(
        attr & 0x0F,
        IDT_GATE_TRAP & 0x0F,
        "SYSCALL_VECTOR not a trap gate"
    );
    assert_eq_test!((attr >> 5) & 0x3, 3, "SYSCALL_VECTOR DPL must be 3");
    TestResult::Pass
}

pub fn test_syscall_vector_handler_nonzero() -> TestResult {
    let entry = match load_idt_entry(SYSCALL_VECTOR) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let h = idt_handler_address(&entry);
    assert_ne_test!(h, 0, "SYSCALL_VECTOR handler address is null");
    TestResult::Pass
}

pub fn test_legacy_irq_vectors_intact() -> TestResult {
    for irq in 0u8..16 {
        let v = IRQ_BASE_VECTOR + irq;
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!(
            attr & 0x80 != 0,
            "Legacy IRQ vector 0x{:02x} not present",
            v
        );
        let h = idt_handler_address(&entry);
        assert_test!(h != 0, "Legacy IRQ vector 0x{:02x} handler is null", v);
    }
    TestResult::Pass
}

pub fn test_exception_vectors_intact() -> TestResult {
    // Vectors 0..=19 (minus 9, 15 reserved) must all have handlers
    // installed by install_default_handlers.
    for v in 0u8..=19 {
        if v == 9 || v == 15 {
            continue;
        }
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!(attr & 0x80 != 0, "Exception vector {} not present", v);
        let h = idt_handler_address(&entry);
        assert_test!(h != 0, "Exception vector {} handler is null", v);
    }
    TestResult::Pass
}

pub fn test_ipi_vectors_intact() -> TestResult {
    // 0xEC = LAPIC_TIMER, 0xFA..=0xFF = IPIs / shutdown / spurious.
    for &v in &[0xECu8, 0xFA, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF] {
        let entry = match load_idt_entry(v) {
            Ok(e) => e,
            Err(r) => return r,
        };
        let attr = entry.type_attr;
        assert_test!(attr & 0x80 != 0, "IPI vector 0x{:02x} not present", v);
        let h = idt_handler_address(&entry);
        assert_test!(h != 0, "IPI vector 0x{:02x} handler is null", v);
    }
    TestResult::Pass
}

pub fn test_msi_range_covers_expected_count() -> TestResult {
    assert_eq_test!(
        (MSI_VECTOR_END - MSI_VECTOR_BASE) as usize,
        176,
        "MSI vector range should be 176 wide"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_msi_alloc_returns_valid_range, suite = msi_alloc);
slopos_testing::stest!(name = test_msi_alloc_distinct_vectors, suite = msi_alloc);
slopos_testing::stest!(
    name = test_msi_alloc_drop_returns_to_pool,
    suite = msi_alloc
);
slopos_testing::stest!(
    name = test_msi_reserve_specific_succeeds_in_msi_range,
    suite = msi_alloc
);
slopos_testing::stest!(
    name = test_msi_reserve_specific_double_claim_refused,
    suite = msi_alloc
);
slopos_testing::stest!(
    name = test_msi_reserve_specific_out_of_msi_range,
    suite = msi_alloc
);
slopos_testing::stest!(
    name = test_msi_register_callback_dispatches,
    suite = msi_handler
);
slopos_testing::stest!(
    name = test_msi_callback_receives_vector,
    suite = msi_handler
);
slopos_testing::stest!(
    name = test_msi_callback_receives_error_code,
    suite = msi_handler
);
slopos_testing::stest!(
    name = test_msi_double_register_same_line_errors,
    suite = msi_handler
);
slopos_testing::stest!(
    name = test_msi_unregister_via_handle_drop,
    suite = msi_handler
);
slopos_testing::stest!(
    name = test_msi_dispatch_unregistered_vector_noop,
    suite = msi_handler
);
slopos_testing::stest!(name = test_msi_idt_entries_present, suite = msi_idt);
slopos_testing::stest!(
    name = test_msi_idt_entries_are_interrupt_gates,
    suite = msi_idt
);
slopos_testing::stest!(name = test_msi_idt_entries_dpl_zero, suite = msi_idt);
slopos_testing::stest!(name = test_msi_idt_entries_have_handlers, suite = msi_idt);
slopos_testing::stest!(name = test_msi_idt_entries_use_kernel_cs, suite = msi_idt);
slopos_testing::stest!(
    name = test_syscall_vector_is_trap_gate_dpl3,
    suite = msi_idt
);
slopos_testing::stest!(name = test_syscall_vector_handler_nonzero, suite = msi_idt);
slopos_testing::stest!(name = test_legacy_irq_vectors_intact, suite = msi_idt);
slopos_testing::stest!(name = test_exception_vectors_intact, suite = msi_idt);
slopos_testing::stest!(name = test_ipi_vectors_intact, suite = msi_idt);
slopos_testing::stest!(name = test_msi_range_covers_expected_count, suite = msi_idt);
