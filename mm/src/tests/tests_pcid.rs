//! PCID assignment must never hand a live address space another one's cached
//! translations.
//!
//! The implementation this replaced took PCIDs from a global monotonic counter
//! masked to the architectural 12 bits, and set the NOFLUSH bit on every CR3
//! write. Address-space creation 4096 therefore received a tag still holding
//! address space 1's translations, and used them without a flush.

use slopos_testing::TestResult;
use slopos_testing::{assert_test, pass};

use crate::mmu::asid::{DYN_ASIDS_PER_CPU, select_cr3};
use crate::mmu::cr3::{Cr3Value, MmContextId, Pcid};
use slopos_abi::addr::PhysAddr;

const PML4: u64 = 0x1_000;

/// Return every frame the address spaces below borrowed, so the suite does not
/// carry a growing quarantine from here to whatever runs next.
fn settle_teardown() {
    // Teardown stages a frame through three holds before it is reusable: the
    // quiesce epoch has to close, then the quarantine rotates, then the
    // per-CPU cache drains.
    let _ = crate::mmu::quiesce::force_close_epoch_for_test();
    for _ in 0..4 {
        crate::page_alloc::quarantine_rotate();
        while crate::page_alloc::quarantine_has_releasable() {
            if crate::page_alloc::quarantine_release_some(u32::MAX) == 0 {
                break;
            }
        }
    }
    crate::page_alloc::pcp_drain_all();
}

/// `cpu` is a parameter rather than a fresh `get_current_cpu()` per call: the
/// pool a sequence of selections reasons about belongs to one CPU, and a
/// sequence that changed pools mid-way would compare two of them.
fn selection(cpu: usize, ctx_raw: u64, tlb_gen: u64) -> (u16, bool) {
    let value = select_cr3(
        cpu,
        MmContextId::from_raw(ctx_raw),
        PhysAddr::new(PML4),
        tlb_gen,
    );
    (
        (value.bits() & Cr3Value::PCID_MASK) as u16,
        value.bits() & Cr3Value::NOFLUSH_BIT != 0,
    )
}

/// The PCID a context receives must come from the per-CPU pool, which knows
/// what each tag currently holds — never from a counter that aliases.
///
/// Cycling more contexts than the pool has slots must reuse tags, and every
/// reuse must be a flushing load. Under the old counter this test's second
/// half was the bug: a reused tag arrived with NOFLUSH set.
pub fn test_pcid_reuse_always_flushes() -> TestResult {
    if !crate::mmu::asid::pcid_enabled() {
        // PCID is off (unsupported, or errata-disabled): every load is a
        // flushing PCID-0 load, which is trivially safe. Nothing to prove.
        return pass!();
    }

    let contexts = DYN_ASIDS_PER_CPU as u64 + 1;

    // Masked throughout: a context switch on this CPU selects from the same
    // pool, and one landing between the binding loop and the re-selection
    // below could evict the slot whose residency is the property under test.
    slopos_arch::cpu::IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();

        for i in 0..contexts {
            let ctx = 1_000 + i;
            let (pcid, no_flush) = selection(cpu, ctx, 1);
            assert_test!(
                pcid != Pcid::KERNEL.bits() as u16,
                "a user context must never be issued the kernel PCID (context {})",
                ctx
            );
            assert_test!(
                !no_flush,
                "a context newly bound to a slot must load flushing (context {})",
                ctx
            );
        }

        // Re-selecting a context still resident at the same generation is the
        // hit case, and only that case may skip the flush.
        let (_, no_flush) = selection(cpu, 1_000 + contexts - 1, 1);
        assert_test!(
            no_flush,
            "a resident context at an unchanged generation must not re-flush"
        );

        pass!()
    })
}

/// A page-table mutation bumps the address space's generation. A slot cached
/// at an older generation may hold stale translations, so the next load under
/// that tag must flush rather than trust it.
pub fn test_stale_generation_forces_a_flush() -> TestResult {
    if !crate::mmu::asid::pcid_enabled() {
        return pass!();
    }

    let ctx = 2_000u64;

    // The four selections are one sequence over one CPU's pool; a context
    // switch between any two of them would re-bind the slot under it.
    slopos_arch::cpu::IrqDisabled::with(|_irq| {
        let cpu = slopos_arch::pcr::get_current_cpu();

        let (pcid_a, _) = selection(cpu, ctx, 1);
        let (pcid_b, no_flush_b) = selection(cpu, ctx, 1);
        assert_test!(pcid_a == pcid_b, "a resident context must keep its tag");
        assert_test!(no_flush_b, "an unchanged generation is the hit case");

        let (pcid_c, _) = selection(cpu, ctx, 2);
        assert_test!(
            pcid_c == pcid_a,
            "a generation bump must not change the tag, only refresh it"
        );

        let (_, no_flush_d) = selection(cpu, ctx, 2);
        assert_test!(
            no_flush_d,
            "a slot refreshed to the current generation is a hit"
        );

        pass!()
    })
}

/// PCID 0 is the kernel's. No user address space may be issued it, which is
/// exactly what `4096 & 0x0FFF == 0` did under the old counter.
pub fn test_kernel_pcid_is_never_issued_to_a_user_context() -> TestResult {
    if !crate::mmu::asid::pcid_enabled() {
        return pass!();
    }

    let cpu = slopos_arch::pcr::get_current_cpu();
    for i in 0..(DYN_ASIDS_PER_CPU as u64 * 4) {
        let (pcid, _) = selection(cpu, 3_000 + i, 1);
        assert_test!(
            pcid != Pcid::KERNEL.bits() as u16,
            "context {} was issued the kernel PCID",
            3_000 + i
        );
        assert_test!(
            (pcid as usize) <= DYN_ASIDS_PER_CPU,
            "context {} was issued PCID {}, outside the pool",
            3_000 + i,
            pcid
        );
    }
    pass!()
}

slopos_testing::stest!(name = test_pcid_reuse_always_flushes, suite = pcid);
slopos_testing::stest!(name = test_stale_generation_forces_a_flush, suite = pcid);
slopos_testing::stest!(
    name = test_kernel_pcid_is_never_issued_to_a_user_context,
    suite = pcid
);

/// The end-to-end property, read out of the register rather than out of the
/// pool: activating a user address space must not load a PCID the pool did
/// not issue, and must not set NOFLUSH on a tag it did not just validate.
///
/// This is the test that sees the whole path — `VmSpace::activate`, the hook,
/// and the pool. A hook that ignored the pool and derived a tag itself (the
/// wrapping counter this fix removed) is caught here and nowhere else.
pub fn test_activate_loads_a_pool_issued_pcid() -> TestResult {
    use slopos_ostd::cpu::x86_64::control_regs::read_cr3;

    if !crate::mmu::asid::pcid_enabled() {
        return pass!();
    }

    let pid = crate::process_vm::create_process_vm();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return slopos_testing::fail!("could not create an address space");
    }
    let process = super::tests::resolve_pid(pid);
    let Some(vm_space) = crate::process_vm::process_vm_get_vm_space(process) else {
        crate::process_vm::destroy_process_vm(process);
        settle_teardown();
        return slopos_testing::fail!("no vm_space for a live process");
    };

    let ctx = crate::process_vm::process_vm_get_mm_ctx_id(process);
    let tlb_gen = vm_space.generation();

    // Activating and restoring both happen with interrupts disabled, so no
    // dispatch can observe the borrowed address space.
    let observed = slopos_arch::cpu::IrqDisabled::with(|_irq| {
        let kernel_cr3 = read_cr3();
        vm_space.activate_at_context_switch();
        let user_cr3 = read_cr3();
        // Restore the exact CR3 this CPU entered with, before anything else
        // can run on it. Writing back the saved value reloads the kernel
        // master under its own tag with the flush bit clear, which is what
        // was loaded a moment ago and is still current.
        crate::mmu::cr3::write_cr3_value(Cr3Value::from_raw(kernel_cr3));
        (kernel_cr3, user_cr3)
    });

    // Before teardown: this clone is an owning reference, and the address
    // space's frames are not released while it is alive.
    drop(vm_space);
    crate::process_vm::destroy_process_vm(process);
    settle_teardown();

    let (_kernel_cr3, user_cr3) = observed;
    let pcid = (user_cr3 & Cr3Value::PCID_MASK) as u16;
    // CR3 bit 63 is a write-only control bit: the processor consumes it on the
    // write and reads back zero, so the flush decision is not observable from
    // the register. It is asserted against the pool instead, in
    // `test_pcid_reuse_always_flushes`.

    assert_test!(
        pcid != Pcid::KERNEL.bits() as u16,
        "activating a user address space loaded the kernel PCID"
    );
    assert_test!(
        (pcid as usize) <= DYN_ASIDS_PER_CPU,
        "activate loaded PCID {}, which the pool never issues (pool is 1..={})",
        pcid,
        DYN_ASIDS_PER_CPU
    );

    // The tag must be the one this CPU's pool holds for that context, not one
    // derived from the context id. A counter masked to 12 bits agrees with the
    // pool only by coincidence, and stops agreeing as soon as either wraps.
    let pool_pcid = crate::mmu::asid::select_pcid_for_activate(ctx, tlb_gen).map(|(p, _)| p);
    assert_test!(
        pool_pcid == Some(pcid),
        "activate loaded PCID {} but the pool holds {:?} for that context",
        pcid,
        pool_pcid
    );
    pass!()
}

slopos_testing::stest!(name = test_activate_loads_a_pool_issued_pcid, suite = pcid);
