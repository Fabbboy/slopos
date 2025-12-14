#![allow(static_mut_refs)]

use core::arch::asm;

use slopos_lib::klog_debug;

const GDT_CODE_SELECTOR: u16 = 0x08;
const GDT_DATA_SELECTOR: u16 = 0x10;
const GDT_TSS_SELECTOR: u16 = 0x28;

// GDT Access Byte bit fields (bits 40-47)
// Bit 47: Present (P)
const GDT_ACCESS_PRESENT: u8 = 1 << 7;
// Bits 45-46: Descriptor Privilege Level (DPL)
const GDT_ACCESS_DPL_KERNEL: u8 = 0 << 5; // Ring 0
const GDT_ACCESS_DPL_USER: u8 = 3 << 5;   // Ring 3
// Bit 44: Segment type (S) - 1 for code/data segment
const GDT_ACCESS_SEGMENT: u8 = 1 << 4;
// Bits 43-40: Type field
// Code segment: executable (1), readable (1), conforming (0), accessed (0) = 1010
const GDT_ACCESS_CODE_TYPE: u8 = 0b1010;
// Data segment: executable (0), writable (1), expand-down (0), accessed (0) = 0010
const GDT_ACCESS_DATA_TYPE: u8 = 0b0010;

// GDT Flags (bits 52-55)
// Bit 55: Granularity (G) = 1
const GDT_FLAG_GRANULARITY: u8 = 1 << 3;
// Bit 54: Size (D/B) = 0 (64-bit mode)
// Bit 53: Long (L) = 1 (64-bit code segment)
const GDT_FLAG_LONG_MODE: u8 = 1 << 1;
// Bit 52: Available (AVL) = 0
// Combined flags for 64-bit segments: G=1, D/B=0, L=1, AVL=0 = 1010 = 0xA
const GDT_FLAGS_64BIT: u8 = GDT_FLAG_GRANULARITY | GDT_FLAG_LONG_MODE;

// GDT Limit values for 64-bit segments
const GDT_LIMIT_LOW: u16 = 0xFFFF;
const GDT_LIMIT_HIGH: u8 = 0xF;

// GDT Base values (ignored in 64-bit mode, but must be set)
const GDT_BASE_LOW: u16 = 0x0000;
const GDT_BASE_MID: u8 = 0x00;
const GDT_BASE_HIGH: u8 = 0x00;

/// Constructs a 64-bit GDT descriptor from individual fields.
/// 
/// According to OSDev GDT structure:
/// - Bits 0-15: Limit (low 16 bits)
/// - Bits 16-31: Base (low 16 bits)
/// - Bits 32-39: Base (middle 8 bits)
/// - Bits 40-47: Access byte
/// - Bits 48-51: Limit (high 4 bits)
/// - Bits 52-55: Flags
/// - Bits 56-63: Base (high 8 bits)
const fn gdt_make_descriptor(
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    limit_high: u8,
    flags: u8,
    base_high: u8,
) -> u64 {
    (limit_low as u64)
        | ((base_low as u64) << 16)
        | ((base_mid as u64) << 32)
        | ((access as u64) << 40)
        | ((limit_high as u64) << 48)
        | ((flags as u64) << 52)
        | ((base_high as u64) << 56)
}

const GDT_NULL_DESCRIPTOR: u64 = 0x0000_0000_0000_0000;
const GDT_CODE_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    GDT_LIMIT_LOW,
    GDT_BASE_LOW,
    GDT_BASE_MID,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    GDT_LIMIT_HIGH,
    GDT_FLAGS_64BIT,
    GDT_BASE_HIGH,
);
const GDT_DATA_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    GDT_LIMIT_LOW,
    GDT_BASE_LOW,
    GDT_BASE_MID,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    GDT_LIMIT_HIGH,
    GDT_FLAGS_64BIT,
    GDT_BASE_HIGH,
);
const GDT_USER_CODE_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    GDT_LIMIT_LOW,
    GDT_BASE_LOW,
    GDT_BASE_MID,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    GDT_LIMIT_HIGH,
    GDT_FLAGS_64BIT,
    GDT_BASE_HIGH,
);
const GDT_USER_DATA_DESCRIPTOR_64: u64 = gdt_make_descriptor(
    GDT_LIMIT_LOW,
    GDT_BASE_LOW,
    GDT_BASE_MID,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    GDT_LIMIT_HIGH,
    GDT_FLAGS_64BIT,
    GDT_BASE_HIGH,
);

#[repr(C, packed)]
struct Tss64 {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed)]
struct GdtTssEntry {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    granularity: u8,
    base_high: u8,
    base_upper: u32,
    reserved: u32,
}

#[repr(C, packed)]
struct GdtLayout {
    entries: [u64; 5],
    tss_entry: GdtTssEntry,
}

#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u64,
}

static mut GDT_TABLE: GdtLayout = GdtLayout {
    entries: [0; 5],
    tss_entry: GdtTssEntry {
        limit_low: 0,
        base_low: 0,
        base_mid: 0,
        access: 0,
        granularity: 0,
        base_high: 0,
        base_upper: 0,
        reserved: 0,
    },
};

static mut KERNEL_TSS: Tss64 = Tss64 {
    reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: 0,
};

unsafe extern "C" {
    static kernel_stack_top: u8;
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn load_gdt(descriptor: &GdtDescriptor) {
    unsafe { asm!("lgdt [{0}]", in(reg) descriptor, options(nostack, preserves_flags)) };

    unsafe {
        asm!(
            "pushq ${code}",
            "lea 2f(%rip), %rax",
            "pushq %rax",
            "lretq",
            "2:",
            "movw ${data}, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "movw %ax, %fs",
            "movw %ax, %gs",
            code = const GDT_CODE_SELECTOR as usize,
            data = const GDT_DATA_SELECTOR as usize,
            out("rax") _,
            options(att_syntax, nostack)
        );
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn load_tss() {
    let selector = GDT_TSS_SELECTOR;
    unsafe { asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags)) };
}

#[unsafe(no_mangle)]
pub fn gdt_init() {
    klog_debug!("GDT: Initializing descriptor tables");

    unsafe {
        GDT_TABLE.entries = [
            GDT_NULL_DESCRIPTOR,
            GDT_CODE_DESCRIPTOR_64,
            GDT_DATA_DESCRIPTOR_64,
            GDT_USER_DATA_DESCRIPTOR_64,
            GDT_USER_CODE_DESCRIPTOR_64,
        ];

        let tss_base = &KERNEL_TSS as *const _ as u64;
        let tss_limit = core::mem::size_of::<Tss64>() as u16 - 1;

        let tss_entry = &mut GDT_TABLE.tss_entry;
        tss_entry.limit_low = tss_limit & 0xFFFF;
        tss_entry.base_low = (tss_base & 0xFFFF) as u16;
        tss_entry.base_mid = ((tss_base >> 16) & 0xFF) as u8;
        tss_entry.access = 0x89; // Present | type=64-bit available TSS
        tss_entry.granularity = (((tss_limit as u32) >> 16) & 0x0F) as u8;
        tss_entry.base_high = ((tss_base >> 24) & 0xFF) as u8;
        tss_entry.base_upper = (tss_base >> 32) as u32;
        tss_entry.reserved = 0;

        KERNEL_TSS.iomap_base = core::mem::size_of::<Tss64>() as u16;
        KERNEL_TSS.rsp0 = (&kernel_stack_top as *const u8) as u64;

        let descriptor = GdtDescriptor {
            limit: (core::mem::size_of::<GdtLayout>() - 1) as u16,
            base: &GDT_TABLE as *const _ as u64,
        };

        load_gdt(&descriptor);
        load_tss();
    }

    klog_debug!("GDT: Initialized with TSS loaded");
}

#[unsafe(no_mangle)]
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    unsafe {
        KERNEL_TSS.rsp0 = rsp0;
    }
}

#[unsafe(no_mangle)]
pub fn gdt_set_ist(index: u8, stack_top: u64) {
    if index == 0 || index > 7 {
        return;
    }
    unsafe {
        KERNEL_TSS.ist[(index - 1) as usize] = stack_top;
    }
}
