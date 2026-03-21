//! MSI infrastructure tests - vector allocator, handler table, and IDT verification.

use core::ffi::c_void;
use core::ptr;

use slopos_arch::InterruptFrame;
use slopos_arch::arch::idt::{
    IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IdtEntry, MSI_VECTOR_BASE, MSI_VECTOR_COUNT, MSI_VECTOR_END,
    SYSCALL_VECTOR,
};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ne_test, assert_test};
use slopos_utils::klog_info;

use crate::irq::{
    msi_alloc_vector, msi_allocated_count, msi_free_vector, msi_register_handler,
    msi_unregister_handler, msi_vector_is_allocated,
};
use slopos_kernel_services::platform::idt_get_gate;

const MSI_SAMPLE_VECTORS: [u8; 5] = [48, 100, 150, 200, 223];

extern "C" fn dummy_msi_handler(_vector: u8, _frame: *mut InterruptFrame, _ctx: *mut c_void) {}

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

pub fn test_msi_alloc_returns_valid_range() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    assert_test!(
        (MSI_VECTOR_BASE..MSI_VECTOR_END).contains(&vector),
        "allocated vector {} out of MSI range [{}, {})",
        vector,
        MSI_VECTOR_BASE,
        MSI_VECTOR_END
    );

    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_alloc_and_free_roundtrip() -> TestResult {
    let baseline = msi_allocated_count();
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    msi_free_vector(vector);
    assert_eq_test!(
        msi_allocated_count(),
        baseline,
        "allocated count did not return to baseline"
    );

    TestResult::Pass
}

pub fn test_msi_alloc_uniqueness() -> TestResult {
    let mut vectors = [0u8; 10];
    let mut used = 0usize;

    for i in 0..10 {
        let vector = match msi_alloc_vector() {
            Some(v) => v,
            None => {
                klog_info!("MSI_TEST: allocator exhausted while collecting uniqueness sample");
                for v in &vectors[..used] {
                    msi_free_vector(*v);
                }
                return TestResult::Fail;
            }
        };

        for seen in &vectors[..used] {
            assert_test!(*seen != vector, "duplicate vector allocated: {}", vector);
        }

        vectors[i] = vector;
        used += 1;
    }

    for vector in &vectors[..used] {
        msi_free_vector(*vector);
    }

    TestResult::Pass
}

pub fn test_msi_free_makes_vector_available() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    msi_free_vector(vector);

    let reallocated = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator failed after free");
            return TestResult::Fail;
        }
    };

    msi_free_vector(reallocated);
    TestResult::Pass
}

pub fn test_msi_alloc_count_tracking() -> TestResult {
    let baseline = msi_allocated_count();

    let v1 = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: failed first allocation for count tracking");
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        msi_allocated_count(),
        baseline + 1,
        "allocated count did not increment after first allocation"
    );

    let v2 = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            msi_free_vector(v1);
            klog_info!("MSI_TEST: failed second allocation for count tracking");
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        msi_allocated_count(),
        baseline + 2,
        "allocated count did not increment after second allocation"
    );

    msi_free_vector(v1);
    assert_eq_test!(
        msi_allocated_count(),
        baseline + 1,
        "allocated count did not decrement after first free"
    );

    msi_free_vector(v2);
    assert_eq_test!(
        msi_allocated_count(),
        baseline,
        "allocated count did not return to baseline after frees"
    );

    TestResult::Pass
}

pub fn test_msi_vector_is_allocated_check() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    assert_test!(
        msi_vector_is_allocated(vector),
        "vector {} not marked as allocated",
        vector
    );

    msi_free_vector(vector);

    assert_test!(
        !msi_vector_is_allocated(vector),
        "vector {} still marked allocated after free",
        vector
    );

    TestResult::Pass
}

pub fn test_msi_free_invalid_vector_no_panic() -> TestResult {
    msi_free_vector(0);
    msi_free_vector(255);
    TestResult::Pass
}

pub fn test_msi_free_unallocated_no_panic() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    msi_free_vector(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_alloc_skips_syscall_vector() -> TestResult {
    let mut allocated = [0u8; MSI_VECTOR_COUNT];
    let mut used = 0usize;

    while let Some(vector) = msi_alloc_vector() {
        assert_ne_test!(
            vector,
            SYSCALL_VECTOR,
            "allocator returned syscall vector 0x80"
        );

        allocated[used] = vector;
        used += 1;

        if used >= MSI_VECTOR_COUNT {
            break;
        }
    }

    for vector in &allocated[..used] {
        msi_free_vector(*vector);
    }

    TestResult::Pass
}

pub fn test_msi_register_handler_success() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    let rc = msi_register_handler(vector, dummy_msi_handler, ptr::null_mut(), 0);
    assert_eq_test!(rc, 0, "register handler failed for valid vector");

    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_register_handler_invalid_vector() -> TestResult {
    let rc = msi_register_handler(0, dummy_msi_handler, ptr::null_mut(), 0);
    assert_test!(rc != 0, "register succeeded for invalid vector 0");
    TestResult::Pass
}

pub fn test_msi_register_handler_above_range() -> TestResult {
    let rc = msi_register_handler(MSI_VECTOR_END, dummy_msi_handler, ptr::null_mut(), 0);
    assert_test!(rc != 0, "register succeeded for vector >= MSI range");
    TestResult::Pass
}

pub fn test_msi_unregister_handler_cleans_up() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    let rc = msi_register_handler(vector, dummy_msi_handler, ptr::null_mut(), 0);
    assert_eq_test!(rc, 0, "register failed before unregister cleanup test");

    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_unregister_unregistered_no_panic() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    msi_unregister_handler(vector);
    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_register_with_context() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    let mut context_value: u64 = 0x1234_5678_9ABC_DEF0;
    let context = (&mut context_value as *mut u64).cast::<c_void>();
    let rc = msi_register_handler(vector, dummy_msi_handler, context, 0);
    assert_eq_test!(rc, 0, "register with non-null context failed");

    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_register_with_device_bdf() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    let rc = msi_register_handler(vector, dummy_msi_handler, ptr::null_mut(), 0x0002_1f_03);
    assert_eq_test!(rc, 0, "register with BDF failed");

    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_double_register_same_vector() -> TestResult {
    let vector = match msi_alloc_vector() {
        Some(v) => v,
        None => {
            klog_info!("MSI_TEST: allocator returned None");
            return TestResult::Fail;
        }
    };

    let first = msi_register_handler(vector, dummy_msi_handler, ptr::null_mut(), 0x0000_00_01);
    assert_eq_test!(first, 0, "first register on vector failed");

    let second = msi_register_handler(vector, dummy_msi_handler, ptr::null_mut(), 0x0000_00_02);
    assert_eq_test!(
        second,
        0,
        "second register should overwrite existing handler"
    );

    msi_unregister_handler(vector);
    msi_free_vector(vector);
    TestResult::Pass
}

pub fn test_msi_idt_entries_present() -> TestResult {
    for vector in MSI_SAMPLE_VECTORS {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        assert_test!(
            (entry.type_attr & 0x80) != 0,
            "vector {} missing present bit",
            vector
        );
    }

    TestResult::Pass
}

pub fn test_msi_idt_entries_are_interrupt_gates() -> TestResult {
    for vector in MSI_SAMPLE_VECTORS {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        assert_eq_test!(
            entry.type_attr & 0x0F,
            IDT_GATE_INTERRUPT & 0x0F,
            "vector is not an interrupt gate"
        );
    }

    TestResult::Pass
}

pub fn test_msi_idt_entries_dpl_zero() -> TestResult {
    for vector in MSI_SAMPLE_VECTORS {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        assert_eq_test!(
            (entry.type_attr >> 5) & 0x03,
            0,
            "MSI vector has non-kernel DPL"
        );
    }

    TestResult::Pass
}

pub fn test_msi_idt_entries_have_handlers() -> TestResult {
    for vector in MSI_SAMPLE_VECTORS {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        let handler = idt_handler_address(&entry);
        assert_ne_test!(handler, 0, "MSI vector has zero handler address");
    }

    TestResult::Pass
}

pub fn test_msi_idt_entries_use_kernel_cs() -> TestResult {
    for vector in MSI_SAMPLE_VECTORS {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        assert_eq_test!(entry.selector, 0x08, "MSI vector selector is not kernel CS");
    }

    TestResult::Pass
}

pub fn test_syscall_vector_not_overwritten() -> TestResult {
    let entry = match load_idt_entry(SYSCALL_VECTOR) {
        Ok(v) => v,
        Err(fail) => return fail,
    };

    assert_eq_test!(
        (entry.type_attr >> 5) & 0x03,
        3,
        "syscall vector DPL regressed from user-accessible"
    );
    assert_eq_test!(
        entry.type_attr & 0x0F,
        IDT_GATE_TRAP & 0x0F,
        "syscall vector regressed from trap gate"
    );

    TestResult::Pass
}

pub fn test_syscall_vector_handler_nonzero() -> TestResult {
    let entry = match load_idt_entry(SYSCALL_VECTOR) {
        Ok(v) => v,
        Err(fail) => return fail,
    };

    let handler = idt_handler_address(&entry);
    assert_ne_test!(handler, 0, "syscall vector has zero handler address");

    TestResult::Pass
}

pub fn test_legacy_irq_vectors_intact() -> TestResult {
    for vector in 32u8..48u8 {
        let entry = match load_idt_entry(vector) {
            Ok(v) => v,
            Err(fail) => return fail,
        };

        assert_test!(
            (entry.type_attr & 0x80) != 0,
            "legacy IRQ vector {} missing present bit",
            vector
        );
        assert_test!(
            (entry.type_attr >> 5) & 0x03 == 0,
            "legacy IRQ vector {} DPL is not 0",
            vector
        );
    }

    TestResult::Pass
}

slopos_testing::define_test_suite!(
    msi_alloc,
    [
        test_msi_alloc_returns_valid_range,
        test_msi_alloc_and_free_roundtrip,
        test_msi_alloc_uniqueness,
        test_msi_free_makes_vector_available,
        test_msi_alloc_count_tracking,
        test_msi_vector_is_allocated_check,
        test_msi_free_invalid_vector_no_panic,
        test_msi_free_unallocated_no_panic,
        test_msi_alloc_skips_syscall_vector,
    ]
);

slopos_testing::define_test_suite!(
    msi_handler,
    [
        test_msi_register_handler_success,
        test_msi_register_handler_invalid_vector,
        test_msi_register_handler_above_range,
        test_msi_unregister_handler_cleans_up,
        test_msi_unregister_unregistered_no_panic,
        test_msi_register_with_context,
        test_msi_register_with_device_bdf,
        test_msi_double_register_same_vector,
    ]
);

slopos_testing::define_test_suite!(
    msi_idt,
    [
        test_msi_idt_entries_present,
        test_msi_idt_entries_are_interrupt_gates,
        test_msi_idt_entries_dpl_zero,
        test_msi_idt_entries_have_handlers,
        test_msi_idt_entries_use_kernel_cs,
        test_syscall_vector_not_overwritten,
        test_syscall_vector_handler_nonzero,
        test_legacy_irq_vectors_intact,
    ]
);
