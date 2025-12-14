#![allow(static_mut_refs)]

use core::arch::asm;

use slopos_lib::klog_debug;

const GDT_CODE_SELECTOR: u16 = 0x08;
const GDT_DATA_SELECTOR: u16 = 0x10;
const GDT_TSS_SELECTOR: u16 = 0x28;

const GDT_NULL_DESCRIPTOR: u64 = 0x0000_0000_0000_0000;
const GDT_CODE_DESCRIPTOR_64: u64 = 0x00AF_9A00_0000_FFFF;
const GDT_DATA_DESCRIPTOR_64: u64 = 0x00AF_9200_0000_FFFF;
const GDT_USER_CODE_DESCRIPTOR_64: u64 = 0x00AF_FA00_0000_FFFF;
const GDT_USER_DATA_DESCRIPTOR_64: u64 = 0x00AF_F200_0000_FFFF;

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

unsafe fn load_tss() {
    let selector = GDT_TSS_SELECTOR;
    unsafe { asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags)) };
}

#[unsafe(no_mangle)]
pub extern "C" fn gdt_init() {
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
pub extern "C" fn gdt_set_kernel_rsp0(rsp0: u64) {
    unsafe {
        KERNEL_TSS.rsp0 = rsp0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gdt_set_ist(index: u8, stack_top: u64) {
    if index == 0 || index > 7 {
        return;
    }
    unsafe {
        KERNEL_TSS.ist[(index - 1) as usize] = stack_top;
    }
}
