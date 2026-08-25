//! GDT, TSS and SYSCALL-MSR configuration checks.

use slopos_arch::arch::gdt::IstSlot;
use slopos_arch::cpu;
use slopos_arch::cpu::msr::{EFER_SCE, Msr};
use slopos_arch::get_current_cpu;
use slopos_hermetic::KernelStackTop;
use slopos_ostd::cpu::x86_64::interrupts::IrqDisabled;
use slopos_ostd::klog_info;
use slopos_ostd::test_support::{arch as ts_arch, gdt as ts_gdt, pcr as ts_pcr};
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::{TestResult, assert_test};

use crate::gdt::{gdt_init, gdt_set_ist, gdt_set_kernel_rsp0, syscall_msr_init};
use crate::idt::{IdtEntry, idt_get_gate};
use crate::ist_stacks::{IST_STACK_COUNT, stack_bounds_for_cpu};

/// Probe stack top `n`, taken from `cpu`'s live IST stacks: `cli` masks only the
/// IRQ-routed slots, so an installed probe still has to be guard-paged and
/// inside the SafeStack IST region.
fn probe_stack_top(cpu: usize, n: usize) -> KernelStackTop<'static> {
    let (_guard_start, _guard_end, _stack_base, top) =
        stack_bounds_for_cpu(cpu, n % IST_STACK_COUNT);
    KernelStackTop::from_kernel_va(top - 16 * (n / IST_STACK_COUNT) as u64)
}

/// Slot `i` is probed with slot `i+1`'s stack, so a write landing one TSS entry
/// over is a mismatch rather than a coincidence.
fn ist_probe_top(cpu: usize, slot: IstSlot) -> KernelStackTop<'static> {
    probe_stack_top(cpu, slot.as_tss_offset() + 1)
}

fn live_tss_base() -> Option<u64> {
    let tr = read_tr();
    if tr == 0 {
        return None;
    }
    let (_limit, gdt_base) = read_gdtr();
    let (tss_base, _tss_limit) = ts_gdt::read_tss_descriptor(gdt_base, (tr >> 3) as usize);
    Some(tss_base)
}

/// IST1..IST7 are seven 8-byte entries at TSS64 offset 36; neither they nor
/// `rsp0` at offset 4 are 8-aligned, hence the byte-array helper.
fn live_tss_ist(tss_base: u64, slot: IstSlot) -> u64 {
    u64::from_le_bytes(ts_gdt::read_bytes_at::<8>(
        tss_base + 36 + slot.as_tss_offset() as u64 * 8,
    ))
}

fn live_tss_rsp0(tss_base: u64) -> u64 {
    u64::from_le_bytes(ts_gdt::read_bytes_at::<8>(tss_base + 4))
}

fn live_rsp0() -> Option<u64> {
    live_tss_base().map(live_tss_rsp0)
}

fn read_gdtr() -> (u16, u64) {
    let g = ts_arch::read_gdtr();
    (g.limit, g.base)
}

pub fn test_gdt_loaded_valid_limit() -> TestResult {
    let (limit, base) = read_gdtr();

    // null + 4 segments + a double-wide TSS descriptor = 56 bytes.
    if limit < 55 {
        klog_info!(
            "GDT_TEST: BUG - GDT limit too small: {} (expected >= 55)",
            limit
        );
        return TestResult::Fail;
    }

    if base == 0 {
        klog_info!("GDT_TEST: BUG - GDT base is NULL");
        return TestResult::Fail;
    }

    if base < 0xFFFF_8000_0000_0000 {
        klog_info!("GDT_TEST: BUG - GDT base 0x{:x} not in kernel space", base);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_current_cs_is_kernel() -> TestResult {
    let cs = ts_arch::read_cs();

    // Kernel CS 0x08 = index 1, TI=0, RPL=0.
    if cs != 0x08 {
        klog_info!("GDT_TEST: BUG - Current CS is 0x{:x}, expected 0x08", cs);
        return TestResult::Fail;
    }

    let rpl = cs & 0x3;
    if rpl != 0 {
        klog_info!("GDT_TEST: BUG - CS RPL is {}, expected 0 (kernel)", rpl);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_current_ss_is_kernel() -> TestResult {
    let ss = ts_arch::read_ss();

    // Kernel SS 0x10 = index 2, TI=0, RPL=0.
    if ss != 0x10 {
        klog_info!("GDT_TEST: BUG - Current SS is 0x{:x}, expected 0x10", ss);
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_data_segment_selectors() -> TestResult {
    let ds = ts_arch::read_ds();
    let es = ts_arch::read_es();
    let fs = ts_arch::read_fs();
    let gs = ts_arch::read_gs();

    // In 64-bit mode these may legitimately be null or a data selector.
    for (name, sel) in [("DS", ds), ("ES", es)] {
        if sel != 0 && sel != 0x10 {
            klog_info!("GDT_TEST: WARNING - {} is 0x{:x}, not 0 or 0x10", name, sel);
        }
    }

    // FS/GS carry TLS bases, so only the RPL is worth checking.
    if (fs & 0x3) == 3 || (gs & 0x3) == 3 {
        klog_info!(
            "GDT_TEST: WARNING - FS=0x{:x} GS=0x{:x} have user RPL in kernel",
            fs,
            gs
        );
    }

    TestResult::Pass
}

fn read_tr() -> u16 {
    ts_arch::read_tr()
}

pub fn test_tss_loaded() -> TestResult {
    let tr = read_tr();

    if tr == 0 {
        klog_info!("GDT_TEST: BUG - TSS not loaded (TR is 0)");
        return TestResult::Fail;
    }

    // TSS selector 0x28 = index 5, TI=0, RPL=0.
    if tr != 0x28 {
        klog_info!(
            "GDT_TEST: WARNING - TSS selector is 0x{:x}, expected 0x28",
            tr
        );
    }

    TestResult::Pass
}

const RSP0_PROBE_INDEX: usize = 0;

struct Rsp0Probe {
    probe: u64,
    pcr_before: u64,
    tss_before: u64,
    tss_installed: u64,
    tss_restored: u64,
}

/// One install-and-restore round trip of the live RSP0. `RSP0` is loaded only
/// on a ring transition, which a kernel-mode window cannot take; the mask is
/// what keeps the install and the restore adjacent.
fn probe_kernel_rsp0() -> Option<Rsp0Probe> {
    let tss_base = live_tss_base()?;
    let probe = probe_stack_top(get_current_cpu(), RSP0_PROBE_INDEX).as_u64();
    IrqDisabled::with(|irq| {
        let pcr_before = ts_pcr::bsp_kernel_rsp_snapshot(irq)?;
        let tss_before = live_tss_rsp0(tss_base);
        gdt_set_kernel_rsp0(probe);
        let tss_installed = live_tss_rsp0(tss_base);
        ts_pcr::bsp_kernel_rsp_restore(irq, pcr_before);
        Some(Rsp0Probe {
            probe,
            pcr_before,
            tss_before,
            tss_installed,
            tss_restored: live_tss_rsp0(tss_base),
        })
    })
}

/// The value handed to `gdt_set_kernel_rsp0` reaches the live TSS, and the
/// PCR's `kernel_rsp` and that TSS field name one stack rather than two.
pub fn test_gdt_set_kernel_rsp0_valid() -> TestResult {
    let Some(observed) = probe_kernel_rsp0() else {
        klog_info!("GDT_TEST: BUG - no TSS loaded, or the BSP PCR is uninitialised");
        return TestResult::Fail;
    };

    assert_test!(
        observed.pcr_before == observed.tss_before,
        "PCR kernel_rsp and TSS.RSP0 named different stacks before the write"
    );
    assert_test!(
        observed.tss_installed == observed.probe,
        "gdt_set_kernel_rsp0 did not reach the live TSS"
    );
    assert_test!(
        observed.tss_restored == observed.tss_before,
        "the round trip did not put the original RSP0 back"
    );
    TestResult::Pass
}

/// The setter has no validator, so whatever is installed is taken as a stack by
/// the next ring transition. One test rather than three: null, a user address
/// and a misaligned value are the same invariant read three ways, and split
/// across tests none of them could fail without the others.
///
/// Alignment is the part that is not implied by the other two: `SS:RSP` is
/// loaded verbatim and the CPU's first push is 8-byte, so a truncated or
/// off-by-one write lands mid-frame rather than nowhere.
pub fn test_gdt_kernel_rsp0_is_a_usable_stack_top() -> TestResult {
    const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;
    let Some(rsp0) = live_rsp0() else {
        klog_info!("GDT_TEST: BUG - no TSS loaded");
        return TestResult::Fail;
    };

    assert_test!(rsp0 != 0, "the live TSS.RSP0 is NULL");
    assert_test!(rsp0 >= KERNEL_HALF, "the live TSS.RSP0 is a user address");
    assert_test!(
        rsp0 % 16 == 0,
        "the live TSS.RSP0 0x{:x} is not 16-byte aligned",
        rsp0
    );
    TestResult::Pass
}

const IST_SLOTS: [IstSlot; 7] = [
    IstSlot::DoubleFault,
    IstSlot::StackFault,
    IstSlot::GeneralProtection,
    IstSlot::PageFault,
    IstSlot::KeyboardIrq,
    IstSlot::MouseIrq,
    IstSlot::Reserved7,
];

/// Each IST slot takes the stack it was given, in its own TSS entry.
pub fn test_gdt_set_ist_valid_indices() -> TestResult {
    let mut scope = KernelTestScope::enter();
    let cpu = get_current_cpu();
    let Some(tss_base) = live_tss_base() else {
        klog_info!("GDT_TEST: BUG - no TSS loaded");
        return TestResult::Fail;
    };

    let observed = IrqDisabled::with(|irq| {
        let saved = ts_pcr::bsp_ist_snapshot(irq)?;
        let mut installed = [0u64; 7];
        // Reverse: the permutation is `slot i takes stack i+1`, so writing
        // forward leaves each just-written slot transiently holding the boot
        // value of the next, not-yet-written one — an alias between two
        // vectors `cli` does not mask.
        for slot in IST_SLOTS.iter().rev() {
            let top = ist_probe_top(cpu, *slot);
            scope.with_boot(|ctx| gdt_set_ist(ctx, *slot, top));
        }
        for (idx, slot) in IST_SLOTS.iter().enumerate() {
            installed[idx] = live_tss_ist(tss_base, *slot);
        }
        ts_pcr::bsp_ist_restore(irq, saved);
        Some((installed, saved))
    });

    let Some((installed, saved)) = observed else {
        klog_info!("GDT_TEST: BUG - the BSP PCR is uninitialised");
        return TestResult::Fail;
    };

    for (idx, slot) in IST_SLOTS.iter().enumerate() {
        let expected = ist_probe_top(cpu, *slot).as_u64();
        assert_test!(
            installed[idx] == expected,
            "IST{} holds 0x{:x}, expected 0x{:x}",
            slot.as_index(),
            installed[idx],
            expected
        );
        assert_test!(
            live_tss_ist(tss_base, *slot) == saved[slot.as_tss_offset()],
            "IST{} was not restored to its boot-installed stack",
            slot.as_index()
        );
    }
    TestResult::Pass
}

/// Empty by design: `IstSlot` has no zero variant; the rejection is a compile_fail
/// doctest on the enum.
pub fn test_gdt_set_ist_index_zero() -> TestResult {
    TestResult::Pass
}

/// Empty for the same reason: `IstSlot` tops out at `Reserved7`.
pub fn test_gdt_set_ist_index_overflow() -> TestResult {
    TestResult::Pass
}

pub fn test_efer_sce_enabled() -> TestResult {
    let efer = cpu::read_msr(Msr::EFER);

    if (efer & EFER_SCE) == 0 {
        klog_info!("GDT_TEST: BUG - EFER.SCE not set, SYSCALL will #UD");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_star_msr_valid() -> TestResult {
    let star = cpu::read_msr(Msr::STAR);

    // STAR[47:32] is the SYSCALL CS/SS pair, STAR[63:48] the SYSRET base.
    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_base = ((star >> 48) & 0xFFFF) as u16;

    if syscall_cs != 0x08 {
        klog_info!(
            "GDT_TEST: BUG - STAR SYSCALL CS is 0x{:x}, expected 0x08",
            syscall_cs
        );
        return TestResult::Fail;
    }

    // This layout's base is 0x13; 0x10 belongs to a GDT that orders user code
    // and data the other way.
    if sysret_base != 0x13 && sysret_base != 0x10 {
        klog_info!(
            "GDT_TEST: WARNING - STAR SYSRET base is 0x{:x}, expected 0x13 or 0x10",
            sysret_base
        );
    }

    TestResult::Pass
}

/// An LSTAR in the user half would be an arbitrary-code-execution primitive.
pub fn test_lstar_msr_valid() -> TestResult {
    let lstar = cpu::read_msr(Msr::LSTAR);

    if lstar == 0 {
        klog_info!("GDT_TEST: BUG - LSTAR is 0, SYSCALL will crash");
        return TestResult::Fail;
    }

    if lstar < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - LSTAR 0x{:x} is not in kernel space!",
            lstar
        );
        return TestResult::Fail;
    }

    if lstar > 0xFFFF_FFFF_FFFF_0000 {
        klog_info!("GDT_TEST: WARNING - LSTAR 0x{:x} is unusually high", lstar);
    }

    TestResult::Pass
}

pub fn test_sfmask_msr_valid() -> TestResult {
    let sfmask = cpu::read_msr(Msr::SFMASK);

    // RFLAGS.IF is bit 9 and RFLAGS.TF is bit 8; syscall entry must mask both.
    let if_masked = (sfmask & (1 << 9)) != 0;
    let tf_masked = (sfmask & (1 << 8)) != 0;

    if !if_masked {
        klog_info!("GDT_TEST: WARNING - SFMASK doesn't clear IF, syscall entry may be interrupted");
    }

    if !tf_masked {
        klog_info!("GDT_TEST: WARNING - SFMASK doesn't clear TF, single-step may fire in syscall");
    }

    TestResult::Pass
}

pub fn test_double_fault_uses_ist() -> TestResult {
    let mut entry = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    if idt_get_gate(8, &mut entry) != 0 {
        klog_info!("GDT_TEST: Failed to read IDT entry 8 (Double Fault)");
        return TestResult::Fail;
    }

    if entry.ist == 0 {
        klog_info!("GDT_TEST: BUG - Double fault handler doesn't use IST!");
        klog_info!("GDT_TEST: This means stack overflow -> triple fault");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_page_fault_handler_valid() -> TestResult {
    let mut entry = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    if idt_get_gate(14, &mut entry) != 0 {
        klog_info!("GDT_TEST: Failed to read IDT entry 14 (Page Fault)");
        return TestResult::Fail;
    }

    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - Page fault handler is NULL");
        return TestResult::Fail;
    }

    if handler < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - Page fault handler 0x{:x} not in kernel space",
            handler
        );
        return TestResult::Fail;
    }

    // Copied out of the packed struct before it can be borrowed.
    let selector = { entry.selector };
    if selector != 0x08 {
        klog_info!(
            "GDT_TEST: BUG - Page fault handler uses selector 0x{:x}, not 0x08",
            selector
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_gp_fault_handler_valid() -> TestResult {
    let mut entry = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    if idt_get_gate(13, &mut entry) != 0 {
        klog_info!("GDT_TEST: Failed to read IDT entry 13 (GP Fault)");
        return TestResult::Fail;
    }

    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - GP fault handler is NULL");
        return TestResult::Fail;
    }

    if handler < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - GP fault handler 0x{:x} not in kernel space",
            handler
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_syscall_idt_entry() -> TestResult {
    let mut entry = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    if idt_get_gate(0x80, &mut entry) != 0 {
        klog_info!("GDT_TEST: Failed to read IDT entry 0x80 (Syscall)");
        return TestResult::Fail;
    }

    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - INT 0x80 handler is NULL");
        return TestResult::Fail;
    }

    // DPL is type_attr[6:5]; a user-reachable gate needs 3.
    let dpl = (entry.type_attr >> 5) & 0x3;
    if dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - INT 0x80 DPL is {}, should be 3 for user access",
            dpl
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_gdt_double_init() -> TestResult {
    let (limit_before, _base_before) = read_gdtr();
    let cs_before = ts_arch::read_cs();
    let ss_before = ts_arch::read_ss();

    gdt_init();

    let (limit_after, _base_after) = read_gdtr();
    let cs_after = ts_arch::read_cs();
    let ss_after = ts_arch::read_ss();

    if cs_before != cs_after {
        klog_info!(
            "GDT_TEST: BUG - CS changed after gdt_init: 0x{:x} -> 0x{:x}",
            cs_before,
            cs_after
        );
        return TestResult::Fail;
    }

    if ss_before != ss_after {
        klog_info!(
            "GDT_TEST: BUG - SS changed after gdt_init: 0x{:x} -> 0x{:x}",
            ss_before,
            ss_after
        );
        return TestResult::Fail;
    }

    if limit_before != limit_after {
        klog_info!(
            "GDT_TEST: WARNING - GDT limit changed: {} -> {}",
            limit_before,
            limit_after
        );
    }

    TestResult::Pass
}

pub fn test_syscall_msr_double_init() -> TestResult {
    let efer_before = cpu::read_msr(Msr::EFER);
    let star_before = cpu::read_msr(Msr::STAR);
    let lstar_before = cpu::read_msr(Msr::LSTAR);

    // The BSP brand has already minted and dropped by the time the harness
    // runs, so a fresh BSP-init scope is needed for the `CpuInitWitness`.
    slopos_ostd::sync::run_bsp_init_for_test(|t| syscall_msr_init(t));

    let efer_after = cpu::read_msr(Msr::EFER);
    let star_after = cpu::read_msr(Msr::STAR);
    let lstar_after = cpu::read_msr(Msr::LSTAR);

    if (efer_before & EFER_SCE) != (efer_after & EFER_SCE) {
        klog_info!("GDT_TEST: BUG - EFER.SCE changed after syscall_msr_init");
        return TestResult::Fail;
    }

    if star_before != star_after {
        klog_info!(
            "GDT_TEST: WARNING - STAR changed: 0x{:x} -> 0x{:x}",
            star_before,
            star_after
        );
    }

    if lstar_before != lstar_after {
        klog_info!(
            "GDT_TEST: WARNING - LSTAR changed: 0x{:x} -> 0x{:x}",
            lstar_before,
            lstar_after
        );
    }

    TestResult::Pass
}

/// SYSRET derives the user selectors from the GDT order, so a reordered table
/// loads the wrong segments on return.
pub fn test_gdt_entry_order_matches_selectors() -> TestResult {
    let (_limit, base) = read_gdtr();

    // Selectors 0x08/0x10/0x18/0x20 are indices 1-4; DPL is bits [46:45].
    let entry1 = ts_gdt::read_entry(base, 1);
    let entry2 = ts_gdt::read_entry(base, 2);
    let entry3 = ts_gdt::read_entry(base, 3);
    let entry4 = ts_gdt::read_entry(base, 4);

    let entry1_dpl = (entry1 >> 45) & 0x3;
    if entry1_dpl != 0 {
        klog_info!(
            "GDT_TEST: BUG - Kernel code segment DPL is {}, expected 0",
            entry1_dpl
        );
        return TestResult::Fail;
    }

    let entry2_dpl = (entry2 >> 45) & 0x3;
    if entry2_dpl != 0 {
        klog_info!(
            "GDT_TEST: BUG - Kernel data segment DPL is {}, expected 0",
            entry2_dpl
        );
        return TestResult::Fail;
    }

    let entry3_dpl = (entry3 >> 45) & 0x3;
    if entry3_dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - User data segment DPL is {}, expected 3",
            entry3_dpl
        );
        return TestResult::Fail;
    }

    let entry4_dpl = (entry4 >> 45) & 0x3;
    if entry4_dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - User code segment DPL is {}, expected 3",
            entry4_dpl
        );
        return TestResult::Fail;
    }

    // Bit 43 is the executable bit.
    let entry1_exec = (entry1 >> 43) & 1;
    let entry4_exec = (entry4 >> 43) & 1;
    if entry1_exec != 1 {
        klog_info!("GDT_TEST: BUG - Kernel code segment not executable");
        return TestResult::Fail;
    }
    if entry4_exec != 1 {
        klog_info!("GDT_TEST: BUG - User code segment not executable");
        return TestResult::Fail;
    }

    let entry2_exec = (entry2 >> 43) & 1;
    let entry3_exec = (entry3 >> 43) & 1;
    if entry2_exec != 0 {
        klog_info!("GDT_TEST: BUG - Kernel data segment is executable (security issue!)");
        return TestResult::Fail;
    }
    if entry3_exec != 0 {
        klog_info!("GDT_TEST: BUG - User data segment is executable (security issue!)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// In 64-bit mode SYSRET builds CS from `STAR[63:48] + 16` and SS from
/// `STAR[63:48] + 8`, both with RPL forced to 3.
pub fn test_star_sysret_selector_calculation() -> TestResult {
    let star = cpu::read_msr(Msr::STAR);
    let sysret_base = ((star >> 48) & 0xFFFF) as u16;

    let expected_user_cs = sysret_base + 16;
    let expected_user_ss = sysret_base + 8;

    // User code is index 4 and user data index 3, so with RPL 3 the selectors
    // land on 0x23 and 0x1B.
    if expected_user_cs != 0x23 {
        klog_info!(
            "GDT_TEST: BUG - SYSRET will set CS to 0x{:x}, expected 0x23",
            expected_user_cs
        );
        return TestResult::Fail;
    }

    if expected_user_ss != 0x1B {
        klog_info!(
            "GDT_TEST: BUG - SYSRET will set SS to 0x{:x}, expected 0x1B",
            expected_user_ss
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_tss_rsp0_value_valid() -> TestResult {
    let tr = read_tr();
    if tr == 0 {
        klog_info!("GDT_TEST: BUG - No TSS loaded");
        return TestResult::Fail;
    }

    let (_limit, gdt_base) = read_gdtr();
    let tss_index = (tr >> 3) as usize;

    // The TSS descriptor is double-wide.
    let (tss_base, _limit) = ts_gdt::read_tss_descriptor(gdt_base, tss_index);

    // RSP0 sits at TSS64 offset 4 — eight bytes, but not 8-aligned, hence the
    // byte-array helper.
    let rsp0 = u64::from_le_bytes(ts_gdt::read_bytes_at::<8>(tss_base + 4));

    if rsp0 == 0 {
        klog_info!("GDT_TEST: BUG - TSS.RSP0 is NULL!");
        return TestResult::Fail;
    }

    if rsp0 < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - TSS.RSP0 0x{:x} is not in kernel space!",
            rsp0
        );
        return TestResult::Fail;
    }

    if (rsp0 & 0xF) != 0 {
        klog_info!(
            "GDT_TEST: WARNING - TSS.RSP0 0x{:x} is not 16-byte aligned",
            rsp0
        );
    }

    TestResult::Pass
}

pub fn test_ist_stacks_have_guard_pages() -> TestResult {
    let tr = read_tr();
    if tr == 0 {
        return TestResult::Fail;
    }

    let (_limit, gdt_base) = read_gdtr();
    let tss_index = (tr >> 3) as usize;
    let (tss_base, _limit) = ts_gdt::read_tss_descriptor(gdt_base, tss_index);

    // IST1-IST7 are seven 8-byte entries at TSS64 offset 36.
    let ist_base = tss_base + 36;

    let mut issues = 0u32;
    for i in 0..7u64 {
        let ist_ptr = u64::from_le_bytes(ts_gdt::read_bytes_at::<8>(ist_base + i * 8));

        if ist_ptr == 0 {
            continue;
        }

        if ist_ptr < 0xFFFF_8000_0000_0000 {
            klog_info!(
                "GDT_TEST: BUG - IST{} at 0x{:x} is not in kernel space!",
                i + 1,
                ist_ptr
            );
            issues += 1;
        }

        let rsp0 = u64::from_le_bytes(ts_gdt::read_bytes_at::<8>(tss_base + 4));
        if ist_ptr == rsp0 {
            klog_info!(
                "GDT_TEST: WARNING - IST{} shares address with RSP0 (no isolation)",
                i + 1
            );
        }
    }

    if issues > 0 {
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Heuristic: no function prologue starts with NUL padding or INT3 filler.
pub fn test_lstar_points_to_executable_code() -> TestResult {
    let lstar = cpu::read_msr(Msr::LSTAR);

    let first_bytes = ts_gdt::read_bytes_at::<4>(lstar);

    if first_bytes == [0, 0, 0, 0] {
        klog_info!("GDT_TEST: BUG - LSTAR points to zeros (likely uninitialized/data)");
        return TestResult::Fail;
    }

    if first_bytes == [0xCC, 0xCC, 0xCC, 0xCC] {
        klog_info!("GDT_TEST: BUG - LSTAR points to INT3 padding");
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(name = test_gdt_loaded_valid_limit, suite = gdt);
slopos_testing::stest!(name = test_current_cs_is_kernel, suite = gdt);
slopos_testing::stest!(name = test_current_ss_is_kernel, suite = gdt);
slopos_testing::stest!(name = test_data_segment_selectors, suite = gdt);
slopos_testing::stest!(name = test_tss_loaded, suite = gdt);
slopos_testing::stest!(name = test_gdt_set_kernel_rsp0_valid, suite = gdt);
slopos_testing::stest!(
    name = test_gdt_kernel_rsp0_is_a_usable_stack_top,
    suite = gdt
);
slopos_testing::stest!(name = test_gdt_set_ist_valid_indices, suite = gdt);
slopos_testing::stest!(name = test_gdt_set_ist_index_zero, suite = gdt);
slopos_testing::stest!(name = test_gdt_set_ist_index_overflow, suite = gdt);
slopos_testing::stest!(name = test_efer_sce_enabled, suite = gdt);
slopos_testing::stest!(name = test_star_msr_valid, suite = gdt);
slopos_testing::stest!(name = test_lstar_msr_valid, suite = gdt);
slopos_testing::stest!(name = test_sfmask_msr_valid, suite = gdt);
slopos_testing::stest!(name = test_double_fault_uses_ist, suite = gdt);
slopos_testing::stest!(name = test_page_fault_handler_valid, suite = gdt);
slopos_testing::stest!(name = test_gp_fault_handler_valid, suite = gdt);
slopos_testing::stest!(name = test_syscall_idt_entry, suite = gdt);
slopos_testing::stest!(name = test_gdt_double_init, suite = gdt);
slopos_testing::stest!(name = test_syscall_msr_double_init, suite = gdt);
slopos_testing::stest!(name = test_gdt_entry_order_matches_selectors, suite = gdt);
slopos_testing::stest!(name = test_star_sysret_selector_calculation, suite = gdt);
slopos_testing::stest!(name = test_tss_rsp0_value_valid, suite = gdt);
slopos_testing::stest!(name = test_ist_stacks_have_guard_pages, suite = gdt);
slopos_testing::stest!(name = test_lstar_points_to_executable_code, suite = gdt);
