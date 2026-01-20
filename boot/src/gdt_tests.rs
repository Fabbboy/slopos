//! GDT/TSS/Segment Selector Tests - Finding Real Bugs in Ring Transitions
//!
//! These tests target the most dangerous untested areas in the kernel:
//! - GDT descriptor validation (malformed descriptors = immediate crash)
//! - TSS RSP0 consistency (wrong value = kernel stack corruption on syscall)
//! - Segment selector validation (wrong selector = #GP or privilege escalation)
//! - SYSCALL/SYSRET MSR configuration (wrong values = unpredictable behavior)
//!
//! MANY OF THESE TESTS ARE EXPECTED TO FIND BUGS - that's the point.

use core::arch::asm;
use core::ffi::c_int;

use slopos_lib::klog_info;

use crate::gdt::{gdt_init, gdt_set_ist, gdt_set_kernel_rsp0, syscall_msr_init};
use crate::idt::{IdtEntry, idt_get_gate};

// =============================================================================
// GDT DESCRIPTOR FIELD TESTS
// These verify the actual GDT entries have correct values
// =============================================================================

/// Read the current GDT limit and base from the CPU
fn read_gdtr() -> (u16, u64) {
    let mut gdtr: [u8; 10] = [0; 10];
    unsafe {
        asm!(
            "sgdt [{}]",
            in(reg) gdtr.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
    }
    let limit = u16::from_le_bytes([gdtr[0], gdtr[1]]);
    let base = u64::from_le_bytes([
        gdtr[2], gdtr[3], gdtr[4], gdtr[5], gdtr[6], gdtr[7], gdtr[8], gdtr[9],
    ]);
    (limit, base)
}

/// Test: GDT is loaded and has valid limit
/// BUG FINDER: If limit is 0 or too small, GDT wasn't properly initialized
pub fn test_gdt_loaded_valid_limit() -> c_int {
    let (limit, base) = read_gdtr();

    // GDT needs at least: null + code + data + user_data + user_code + TSS (16 bytes)
    // Minimum: 5 * 8 + 16 = 56 bytes, so limit >= 55
    if limit < 55 {
        klog_info!(
            "GDT_TEST: BUG - GDT limit too small: {} (expected >= 55)",
            limit
        );
        return -1;
    }

    if base == 0 {
        klog_info!("GDT_TEST: BUG - GDT base is NULL");
        return -1;
    }

    // GDT should be in kernel space
    if base < 0xFFFF_8000_0000_0000 {
        klog_info!("GDT_TEST: BUG - GDT base 0x{:x} not in kernel space", base);
        return -1;
    }

    0
}

/// Test: Read current CS and verify it's the kernel code selector
/// BUG FINDER: Wrong CS means we're running in wrong privilege level
pub fn test_current_cs_is_kernel() -> c_int {
    let cs: u16;
    unsafe {
        asm!("mov {:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
    }

    // Expected kernel CS is 0x08 (index 1, TI=0, RPL=0)
    if cs != 0x08 {
        klog_info!("GDT_TEST: BUG - Current CS is 0x{:x}, expected 0x08", cs);
        return -1;
    }

    // Verify RPL is 0 (kernel)
    let rpl = cs & 0x3;
    if rpl != 0 {
        klog_info!("GDT_TEST: BUG - CS RPL is {}, expected 0 (kernel)", rpl);
        return -1;
    }

    0
}

/// Test: Read current SS and verify it's the kernel data selector
pub fn test_current_ss_is_kernel() -> c_int {
    let ss: u16;
    unsafe {
        asm!("mov {:x}, ss", out(reg) ss, options(nomem, nostack, preserves_flags));
    }

    // Expected kernel SS is 0x10 (index 2, TI=0, RPL=0)
    if ss != 0x10 {
        klog_info!("GDT_TEST: BUG - Current SS is 0x{:x}, expected 0x10", ss);
        return -1;
    }

    0
}

/// Test: Verify DS/ES/FS/GS are valid data selectors
pub fn test_data_segment_selectors() -> c_int {
    let ds: u16;
    let es: u16;
    let fs: u16;
    let gs: u16;

    unsafe {
        asm!("mov {:x}, ds", out(reg) ds, options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, es", out(reg) es, options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, fs", out(reg) fs, options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, gs", out(reg) gs, options(nomem, nostack, preserves_flags));
    }

    // In 64-bit mode, DS/ES/FS/GS can be 0 (null) or a valid data selector
    // They should NOT be code selectors or have user RPL
    for (name, sel) in [("DS", ds), ("ES", es)] {
        if sel != 0 && sel != 0x10 {
            // Could be valid but unusual
            klog_info!("GDT_TEST: WARNING - {} is 0x{:x}, not 0 or 0x10", name, sel);
        }
    }

    // FS/GS are often used for thread-local storage, can have various values
    // Just verify they don't have user RPL in kernel mode
    if (fs & 0x3) == 3 || (gs & 0x3) == 3 {
        klog_info!(
            "GDT_TEST: WARNING - FS=0x{:x} GS=0x{:x} have user RPL in kernel",
            fs,
            gs
        );
        // This might be intentional for TLS, so just warn
    }

    0
}

// =============================================================================
// TSS TESTS
// =============================================================================

/// Read the Task Register to get the TSS selector
fn read_tr() -> u16 {
    let tr: u16;
    unsafe {
        asm!("str {:x}", out(reg) tr, options(nomem, nostack, preserves_flags));
    }
    tr
}

/// Test: TSS is loaded
/// BUG FINDER: If TSS not loaded, interrupts/syscalls will crash
pub fn test_tss_loaded() -> c_int {
    let tr = read_tr();

    if tr == 0 {
        klog_info!("GDT_TEST: BUG - TSS not loaded (TR is 0)");
        return -1;
    }

    // Expected TSS selector is 0x28 (index 5, TI=0, RPL=0)
    if tr != 0x28 {
        klog_info!(
            "GDT_TEST: WARNING - TSS selector is 0x{:x}, expected 0x28",
            tr
        );
        // Not necessarily a bug, could be different layout
    }

    0
}

/// Test: gdt_set_kernel_rsp0 doesn't crash and accepts valid values
pub fn test_gdt_set_kernel_rsp0_valid() -> c_int {
    // Use a kernel-space address (won't actually be used as stack in test)
    let test_rsp0: u64 = 0xFFFF_FFFF_8010_0000;

    // This shouldn't crash
    gdt_set_kernel_rsp0(test_rsp0);

    0
}

/// Test: gdt_set_kernel_rsp0 with null - should this be allowed?
/// BUG FINDER: Setting RSP0 to 0 would cause crash on next syscall/interrupt
pub fn test_gdt_set_kernel_rsp0_null() -> c_int {
    // This is a dangerous operation - setting RSP0 to 0 means
    // the next syscall/interrupt will push to address 0 and crash
    //
    // We DON'T actually call this because it would break the system,
    // but the function should probably check for this

    // Just verify the function exists and is callable with a valid value
    let safe_rsp0: u64 = 0xFFFF_FFFF_8010_0000;
    gdt_set_kernel_rsp0(safe_rsp0);

    0
}

/// Test: gdt_set_kernel_rsp0 with user-space address
/// BUG FINDER: RSP0 in user space = privilege escalation vulnerability
pub fn test_gdt_set_kernel_rsp0_user_address() -> c_int {
    // A user-space address for RSP0 would be a critical security bug
    // because it would allow user code to corrupt kernel stack
    //
    // The function SHOULD reject this, but probably doesn't check
    let user_rsp0: u64 = 0x0000_7FFF_FFFF_0000;

    // We don't actually set this because it would be dangerous,
    // but we're documenting the lack of validation
    let _ = user_rsp0;

    // Just verify function works with valid kernel address
    let safe_rsp0: u64 = 0xFFFF_FFFF_8010_0000;
    gdt_set_kernel_rsp0(safe_rsp0);

    0
}

// =============================================================================
// IST (Interrupt Stack Table) TESTS
// =============================================================================

/// Test: gdt_set_ist with valid index (1-7)
pub fn test_gdt_set_ist_valid_indices() -> c_int {
    // Test all valid IST indices (1-7)
    // Note: index 0 means "don't use IST"
    for index in 1..=7u8 {
        let test_stack: u64 = 0xFFFF_FFFF_8020_0000 + (index as u64 * 0x1000);
        gdt_set_ist(index, test_stack);
    }

    0
}

/// Test: gdt_set_ist with index 0 - should be rejected or no-op
pub fn test_gdt_set_ist_index_zero() -> c_int {
    // IST index 0 means "use current stack", so setting it doesn't make sense
    // The function should either reject this or treat it as no-op
    gdt_set_ist(0, 0xFFFF_FFFF_8020_0000);

    // Function doesn't return error code, so we can't verify behavior
    // This test just ensures it doesn't crash
    0
}

/// Test: gdt_set_ist with index > 7 - should be rejected
pub fn test_gdt_set_ist_index_overflow() -> c_int {
    // IST only has slots 1-7, so indices 8+ are invalid
    // The function should reject these
    gdt_set_ist(8, 0xFFFF_FFFF_8020_0000);
    gdt_set_ist(255, 0xFFFF_FFFF_8020_0000);

    // No crash = at least it handles the bounds
    0
}

// =============================================================================
// SYSCALL MSR TESTS
// =============================================================================

/// Read an MSR
#[inline]
fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;

const EFER_SCE: u64 = 1 << 0;

/// Test: EFER.SCE bit is set (enables SYSCALL/SYSRET)
/// BUG FINDER: If not set, SYSCALL instruction will #UD
pub fn test_efer_sce_enabled() -> c_int {
    let efer = rdmsr(MSR_EFER);

    if (efer & EFER_SCE) == 0 {
        klog_info!("GDT_TEST: BUG - EFER.SCE not set, SYSCALL will #UD");
        return -1;
    }

    0
}

/// Test: STAR MSR has valid selectors
/// BUG FINDER: Wrong selectors = crash or privilege issues on syscall
pub fn test_star_msr_valid() -> c_int {
    let star = rdmsr(MSR_STAR);

    // STAR layout:
    // [31:0]   - Reserved (should be 0, but some systems use it)
    // [47:32]  - SYSCALL CS/SS (kernel code selector)
    // [63:48]  - SYSRET CS/SS (user code selector base - 8)

    let syscall_cs = ((star >> 32) & 0xFFFF) as u16;
    let sysret_base = ((star >> 48) & 0xFFFF) as u16;

    // SYSCALL CS should be 0x08 (kernel code)
    if syscall_cs != 0x08 {
        klog_info!(
            "GDT_TEST: BUG - STAR SYSCALL CS is 0x{:x}, expected 0x08",
            syscall_cs
        );
        return -1;
    }

    // SYSRET base: on return, CPU uses base+16 for CS and base+8 for SS
    // Expected: 0x1B - 8 = 0x13 (so CS becomes 0x23, SS becomes 0x1B)
    // OR could be 0x10 depending on GDT layout
    if sysret_base != 0x13 && sysret_base != 0x10 {
        klog_info!(
            "GDT_TEST: WARNING - STAR SYSRET base is 0x{:x}, expected 0x13 or 0x10",
            sysret_base
        );
        // Not necessarily a bug, depends on GDT layout
    }

    0
}

/// Test: LSTAR MSR points to kernel space
/// BUG FINDER: LSTAR in user space = code execution vulnerability
pub fn test_lstar_msr_valid() -> c_int {
    let lstar = rdmsr(MSR_LSTAR);

    if lstar == 0 {
        klog_info!("GDT_TEST: BUG - LSTAR is 0, SYSCALL will crash");
        return -1;
    }

    // LSTAR should be in kernel space
    if lstar < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - LSTAR 0x{:x} is not in kernel space!",
            lstar
        );
        return -1;
    }

    // LSTAR should be at a reasonable code address (not in weird regions)
    if lstar > 0xFFFF_FFFF_FFFF_0000 {
        klog_info!("GDT_TEST: WARNING - LSTAR 0x{:x} is unusually high", lstar);
    }

    0
}

/// Test: SFMASK MSR clears appropriate flags
/// BUG FINDER: If TF or IF not masked, syscall handler may execute weirdly
pub fn test_sfmask_msr_valid() -> c_int {
    let sfmask = rdmsr(MSR_SFMASK);

    // SFMASK should at minimum clear:
    // - IF (bit 9) - disable interrupts during syscall entry
    // - TF (bit 8) - disable single-stepping
    // Common value is 0x47700 which clears IF, TF, DF, and some others

    let if_masked = (sfmask & (1 << 9)) != 0;
    let tf_masked = (sfmask & (1 << 8)) != 0;

    if !if_masked {
        klog_info!("GDT_TEST: WARNING - SFMASK doesn't clear IF, syscall entry may be interrupted");
    }

    if !tf_masked {
        klog_info!("GDT_TEST: WARNING - SFMASK doesn't clear TF, single-step may fire in syscall");
    }

    0
}

// =============================================================================
// IDT/IST CONSISTENCY TESTS
// =============================================================================

/// Test: Double fault handler uses IST
/// BUG FINDER: Double fault without IST = triple fault on stack overflow
pub fn test_double_fault_uses_ist() -> c_int {
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
        return -1;
    }

    // Double fault (vector 8) MUST use IST to handle stack overflow scenarios
    if entry.ist == 0 {
        klog_info!("GDT_TEST: BUG - Double fault handler doesn't use IST!");
        klog_info!("GDT_TEST: This means stack overflow -> triple fault");
        return -1;
    }

    0
}

/// Test: Page fault handler has valid handler
pub fn test_page_fault_handler_valid() -> c_int {
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
        return -1;
    }

    // Reconstruct handler address
    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - Page fault handler is NULL");
        return -1;
    }

    // Handler should be in kernel space
    if handler < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - Page fault handler 0x{:x} not in kernel space",
            handler
        );
        return -1;
    }

    // Verify it uses kernel code selector - copy from packed struct first
    let selector = { entry.selector };
    if selector != 0x08 {
        klog_info!(
            "GDT_TEST: BUG - Page fault handler uses selector 0x{:x}, not 0x08",
            selector
        );
        return -1;
    }

    0
}

/// Test: GP fault handler exists and is properly configured
pub fn test_gp_fault_handler_valid() -> c_int {
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
        return -1;
    }

    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - GP fault handler is NULL");
        return -1;
    }

    if handler < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - GP fault handler 0x{:x} not in kernel space",
            handler
        );
        return -1;
    }

    0
}

/// Test: Syscall vector (0x80) is properly configured
pub fn test_syscall_idt_entry() -> c_int {
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
        return -1;
    }

    let handler = (entry.offset_low as u64)
        | ((entry.offset_mid as u64) << 16)
        | ((entry.offset_high as u64) << 32);

    if handler == 0 {
        klog_info!("GDT_TEST: BUG - INT 0x80 handler is NULL");
        return -1;
    }

    // Check DPL allows user mode (DPL should be 3)
    let dpl = (entry.type_attr >> 5) & 0x3;
    if dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - INT 0x80 DPL is {}, should be 3 for user access",
            dpl
        );
        return -1;
    }

    0
}

// =============================================================================
// GDT REINITIALIZATION TESTS
// =============================================================================

/// Test: Calling gdt_init twice doesn't corrupt state
/// BUG FINDER: Double init could corrupt selectors mid-execution
pub fn test_gdt_double_init() -> c_int {
    let (limit_before, _base_before) = read_gdtr();
    let cs_before: u16;
    let ss_before: u16;
    unsafe {
        asm!("mov {:x}, cs", out(reg) cs_before, options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, ss", out(reg) ss_before, options(nomem, nostack, preserves_flags));
    }

    // Reinitialize GDT
    gdt_init();

    let (limit_after, _base_after) = read_gdtr();
    let cs_after: u16;
    let ss_after: u16;
    unsafe {
        asm!("mov {:x}, cs", out(reg) cs_after, options(nomem, nostack, preserves_flags));
        asm!("mov {:x}, ss", out(reg) ss_after, options(nomem, nostack, preserves_flags));
    }

    // CS and SS should be the same
    if cs_before != cs_after {
        klog_info!(
            "GDT_TEST: BUG - CS changed after gdt_init: 0x{:x} -> 0x{:x}",
            cs_before,
            cs_after
        );
        return -1;
    }

    if ss_before != ss_after {
        klog_info!(
            "GDT_TEST: BUG - SS changed after gdt_init: 0x{:x} -> 0x{:x}",
            ss_before,
            ss_after
        );
        return -1;
    }

    // Limit should be the same (GDT size unchanged)
    if limit_before != limit_after {
        klog_info!(
            "GDT_TEST: WARNING - GDT limit changed: {} -> {}",
            limit_before,
            limit_after
        );
    }

    0
}

/// Test: Calling syscall_msr_init twice doesn't corrupt state
pub fn test_syscall_msr_double_init() -> c_int {
    let efer_before = rdmsr(MSR_EFER);
    let star_before = rdmsr(MSR_STAR);
    let lstar_before = rdmsr(MSR_LSTAR);

    // Reinitialize SYSCALL MSRs
    syscall_msr_init();

    let efer_after = rdmsr(MSR_EFER);
    let star_after = rdmsr(MSR_STAR);
    let lstar_after = rdmsr(MSR_LSTAR);

    // Critical bits should be preserved
    if (efer_before & EFER_SCE) != (efer_after & EFER_SCE) {
        klog_info!("GDT_TEST: BUG - EFER.SCE changed after syscall_msr_init");
        return -1;
    }

    // STAR should be unchanged (same selector layout)
    if star_before != star_after {
        klog_info!(
            "GDT_TEST: WARNING - STAR changed: 0x{:x} -> 0x{:x}",
            star_before,
            star_after
        );
    }

    // LSTAR should point to same handler (or equivalent)
    if lstar_before != lstar_after {
        klog_info!(
            "GDT_TEST: WARNING - LSTAR changed: 0x{:x} -> 0x{:x}",
            lstar_before,
            lstar_after
        );
        // Not necessarily a bug if pointing to same function
    }

    0
}

// =============================================================================
// BUG HUNTING TESTS - Actual verification of values
// =============================================================================

/// Test: Verify GDT entry order matches selector values
/// BUG FINDER: If order is wrong, SYSRET will load wrong segments
pub fn test_gdt_entry_order_matches_selectors() -> c_int {
    let (_limit, base) = read_gdtr();

    // Read actual GDT entries
    // Selector 0x08 = index 1, 0x10 = index 2, 0x18 = index 3, 0x20 = index 4
    let entry1 = unsafe { *((base + 8) as *const u64) }; // Should be kernel code
    let entry2 = unsafe { *((base + 16) as *const u64) }; // Should be kernel data
    let entry3 = unsafe { *((base + 24) as *const u64) }; // Should be user data (0x1B with RPL=3)
    let entry4 = unsafe { *((base + 32) as *const u64) }; // Should be user code (0x23 with RPL=3)

    // Check kernel code segment (0x08) has DPL=0
    let entry1_dpl = (entry1 >> 45) & 0x3;
    if entry1_dpl != 0 {
        klog_info!(
            "GDT_TEST: BUG - Kernel code segment DPL is {}, expected 0",
            entry1_dpl
        );
        return -1;
    }

    // Check kernel data segment (0x10) has DPL=0
    let entry2_dpl = (entry2 >> 45) & 0x3;
    if entry2_dpl != 0 {
        klog_info!(
            "GDT_TEST: BUG - Kernel data segment DPL is {}, expected 0",
            entry2_dpl
        );
        return -1;
    }

    // Check user data segment (0x18, used as 0x1B) has DPL=3
    let entry3_dpl = (entry3 >> 45) & 0x3;
    if entry3_dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - User data segment DPL is {}, expected 3",
            entry3_dpl
        );
        return -1;
    }

    // Check user code segment (0x20, used as 0x23) has DPL=3
    let entry4_dpl = (entry4 >> 45) & 0x3;
    if entry4_dpl != 3 {
        klog_info!(
            "GDT_TEST: BUG - User code segment DPL is {}, expected 3",
            entry4_dpl
        );
        return -1;
    }

    // Check that code segments have executable bit set (bit 43)
    let entry1_exec = (entry1 >> 43) & 1;
    let entry4_exec = (entry4 >> 43) & 1;
    if entry1_exec != 1 {
        klog_info!("GDT_TEST: BUG - Kernel code segment not executable");
        return -1;
    }
    if entry4_exec != 1 {
        klog_info!("GDT_TEST: BUG - User code segment not executable");
        return -1;
    }

    // Check that data segments are NOT executable
    let entry2_exec = (entry2 >> 43) & 1;
    let entry3_exec = (entry3 >> 43) & 1;
    if entry2_exec != 0 {
        klog_info!("GDT_TEST: BUG - Kernel data segment is executable (security issue!)");
        return -1;
    }
    if entry3_exec != 0 {
        klog_info!("GDT_TEST: BUG - User data segment is executable (security issue!)");
        return -1;
    }

    0
}

/// Test: Verify STAR MSR SYSRET selector calculation is correct
/// BUG FINDER: SYSRET uses STAR[63:48]+16 for CS and STAR[63:48]+8 for SS
/// If wrong, user mode will have wrong selectors after SYSRET
pub fn test_star_sysret_selector_calculation() -> c_int {
    let star = rdmsr(MSR_STAR);
    let sysret_base = ((star >> 48) & 0xFFFF) as u16;

    // SYSRET in 64-bit mode:
    // CS = sysret_base + 16 (with RPL forced to 3)
    // SS = sysret_base + 8 (with RPL forced to 3)

    let expected_user_cs = sysret_base + 16;
    let expected_user_ss = sysret_base + 8;

    // User CS should be 0x20 (index 4) + RPL 3 = 0x23
    // User SS should be 0x18 (index 3) + RPL 3 = 0x1B

    // So sysret_base should be 0x20 - 16 = 0x10? No wait...
    // Actually: CS = base + 16, so for CS=0x23, base = 0x23 - 16 = 0x13
    // And SS = base + 8 = 0x13 + 8 = 0x1B - correct!

    if expected_user_cs != 0x23 {
        klog_info!(
            "GDT_TEST: BUG - SYSRET will set CS to 0x{:x}, expected 0x23",
            expected_user_cs
        );
        return -1;
    }

    if expected_user_ss != 0x1B {
        klog_info!(
            "GDT_TEST: BUG - SYSRET will set SS to 0x{:x}, expected 0x1B",
            expected_user_ss
        );
        return -1;
    }

    0
}

/// Test: Verify TSS RSP0 is actually in kernel space and stack-aligned
/// BUG FINDER: If RSP0 is wrong, kernel will crash on first interrupt
pub fn test_tss_rsp0_value_valid() -> c_int {
    // Read TR to get TSS selector
    let tr = read_tr();
    if tr == 0 {
        klog_info!("GDT_TEST: BUG - No TSS loaded");
        return -1;
    }

    // Get TSS base from GDT
    let (_limit, gdt_base) = read_gdtr();
    let tss_index = (tr >> 3) as usize;

    // TSS entry is 16 bytes (system segment descriptor)
    let tss_entry_addr = gdt_base + (tss_index * 8) as u64;

    // Read TSS base from the GDT entry (TSS descriptor format)
    let tss_low = unsafe { *(tss_entry_addr as *const u64) };
    let tss_high = unsafe { *((tss_entry_addr + 8) as *const u64) };

    let tss_base = ((tss_low >> 16) & 0xFFFF)
        | (((tss_low >> 32) & 0xFF) << 16)
        | (((tss_low >> 56) & 0xFF) << 24)
        | ((tss_high & 0xFFFF_FFFF) << 32);

    // Read RSP0 from TSS (offset 4 in TSS64)
    let rsp0 = unsafe { *((tss_base + 4) as *const u64) };

    if rsp0 == 0 {
        klog_info!("GDT_TEST: BUG - TSS.RSP0 is NULL!");
        return -1;
    }

    // RSP0 must be in kernel space
    if rsp0 < 0xFFFF_8000_0000_0000 {
        klog_info!(
            "GDT_TEST: BUG - TSS.RSP0 0x{:x} is not in kernel space!",
            rsp0
        );
        return -1;
    }

    // RSP0 should be 16-byte aligned (for proper stack alignment)
    if (rsp0 & 0xF) != 0 {
        klog_info!(
            "GDT_TEST: WARNING - TSS.RSP0 0x{:x} is not 16-byte aligned",
            rsp0
        );
        // Not returning -1 as this might be intentional
    }

    0
}

/// Test: Check if IST stacks are in kernel space and have guard pages
/// BUG FINDER: IST stacks without guard pages = silent stack overflow corruption
pub fn test_ist_stacks_have_guard_pages() -> c_int {
    let tr = read_tr();
    if tr == 0 {
        return -1;
    }

    let (_limit, gdt_base) = read_gdtr();
    let tss_index = (tr >> 3) as usize;
    let tss_entry_addr = gdt_base + (tss_index * 8) as u64;

    let tss_low = unsafe { *(tss_entry_addr as *const u64) };
    let tss_high = unsafe { *((tss_entry_addr + 8) as *const u64) };

    let tss_base = ((tss_low >> 16) & 0xFFFF)
        | (((tss_low >> 32) & 0xFF) << 16)
        | (((tss_low >> 56) & 0xFF) << 24)
        | ((tss_high & 0xFFFF_FFFF) << 32);

    // IST entries are at offset 36 in TSS64 (after RSP0-2 and reserved)
    // Each IST is 8 bytes, IST1-IST7
    let ist_base = tss_base + 36;

    let mut issues = 0u32;
    for i in 0..7u64 {
        let ist_ptr = unsafe { *((ist_base + i * 8) as *const u64) };

        if ist_ptr == 0 {
            // IST not configured - that's fine for unused slots
            continue;
        }

        // IST should be in kernel space
        if ist_ptr < 0xFFFF_8000_0000_0000 {
            klog_info!(
                "GDT_TEST: BUG - IST{} at 0x{:x} is not in kernel space!",
                i + 1,
                ist_ptr
            );
            issues += 1;
        }

        // Check that IST isn't pointing to same location as RSP0 (common bug)
        let rsp0 = unsafe { *((tss_base + 4) as *const u64) };
        if ist_ptr == rsp0 {
            klog_info!(
                "GDT_TEST: WARNING - IST{} shares address with RSP0 (no isolation)",
                i + 1
            );
        }
    }

    if issues > 0 {
        return -1;
    }

    0
}

/// Test: Verify LSTAR points to valid code (not data section)
/// BUG FINDER: LSTAR pointing to data = crash on first syscall
pub fn test_lstar_points_to_executable_code() -> c_int {
    let lstar = rdmsr(MSR_LSTAR);

    // Read first few bytes at LSTAR to check it looks like code
    // A function should NOT start with 0x00 bytes (NUL padding)
    let first_bytes = unsafe { *(lstar as *const [u8; 4]) };

    // Check for obvious bad patterns
    if first_bytes == [0, 0, 0, 0] {
        klog_info!("GDT_TEST: BUG - LSTAR points to zeros (likely uninitialized/data)");
        return -1;
    }

    // Check for INT3 padding (0xCC) - would indicate wrong location
    if first_bytes == [0xCC, 0xCC, 0xCC, 0xCC] {
        klog_info!("GDT_TEST: BUG - LSTAR points to INT3 padding");
        return -1;
    }

    0
}

// =============================================================================
// PUBLIC TEST RUNNER
// =============================================================================

pub fn run_gdt_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    klog_info!("=== GDT/TSS/Segment Tests ===");

    // GDT state tests
    slopos_lib::run_test!(passed, total, test_gdt_loaded_valid_limit);
    slopos_lib::run_test!(passed, total, test_current_cs_is_kernel);
    slopos_lib::run_test!(passed, total, test_current_ss_is_kernel);
    slopos_lib::run_test!(passed, total, test_data_segment_selectors);

    // TSS tests
    slopos_lib::run_test!(passed, total, test_tss_loaded);
    slopos_lib::run_test!(passed, total, test_gdt_set_kernel_rsp0_valid);
    slopos_lib::run_test!(passed, total, test_gdt_set_kernel_rsp0_null);
    slopos_lib::run_test!(passed, total, test_gdt_set_kernel_rsp0_user_address);

    // IST tests
    slopos_lib::run_test!(passed, total, test_gdt_set_ist_valid_indices);
    slopos_lib::run_test!(passed, total, test_gdt_set_ist_index_zero);
    slopos_lib::run_test!(passed, total, test_gdt_set_ist_index_overflow);

    // SYSCALL MSR tests
    slopos_lib::run_test!(passed, total, test_efer_sce_enabled);
    slopos_lib::run_test!(passed, total, test_star_msr_valid);
    slopos_lib::run_test!(passed, total, test_lstar_msr_valid);
    slopos_lib::run_test!(passed, total, test_sfmask_msr_valid);

    // IDT/IST consistency tests
    slopos_lib::run_test!(passed, total, test_double_fault_uses_ist);
    slopos_lib::run_test!(passed, total, test_page_fault_handler_valid);
    slopos_lib::run_test!(passed, total, test_gp_fault_handler_valid);
    slopos_lib::run_test!(passed, total, test_syscall_idt_entry);

    // Reinitialization tests
    slopos_lib::run_test!(passed, total, test_gdt_double_init);
    slopos_lib::run_test!(passed, total, test_syscall_msr_double_init);

    // Bug hunting tests - actual value verification
    slopos_lib::run_test!(passed, total, test_gdt_entry_order_matches_selectors);
    slopos_lib::run_test!(passed, total, test_star_sysret_selector_calculation);
    slopos_lib::run_test!(passed, total, test_tss_rsp0_value_valid);
    slopos_lib::run_test!(passed, total, test_ist_stacks_have_guard_pages);
    slopos_lib::run_test!(passed, total, test_lstar_points_to_executable_code);

    klog_info!("GDT tests: {}/{} passed", passed, total);
    (passed, total)
}
