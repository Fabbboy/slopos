#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem;
use core::ptr::{read_unaligned, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{klog_debug, klog_info};

use crate::wl_currency;

unsafe extern "C" {
    fn is_hhdm_available() -> i32;
    fn get_hhdm_offset() -> u64;
    fn is_rsdp_available() -> i32;
    fn get_rsdp_address() -> *const c_void;
}

const IOAPIC_MAX_CONTROLLERS: usize = 8;
const IOAPIC_MAX_ISO_ENTRIES: usize = 32;

const IOAPIC_REG_ID: u8 = 0x00;
const IOAPIC_REG_VER: u8 = 0x01;
const IOAPIC_REG_REDIR_BASE: u8 = 0x10;

const IOAPIC_REDIR_WRITABLE_MASK: u32 = (7 << 8) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16);

const MADT_ENTRY_LOCAL_APIC: u8 = 0;
const MADT_ENTRY_IOAPIC: u8 = 1;
const MADT_ENTRY_INTERRUPT_OVERRIDE: u8 = 2;

const ACPI_MADT_POLARITY_MASK: u16 = 0x3;
const ACPI_MADT_TRIGGER_MASK: u16 = 0xC;
const ACPI_MADT_TRIGGER_SHIFT: u16 = 2;

// Redirection entry flag helpers (kept to mirror the C interface)
pub const IOAPIC_FLAG_DELIVERY_FIXED: u32 = 0u32 << 8;
pub const IOAPIC_FLAG_DELIVERY_LOWEST_PRI: u32 = 1u32 << 8;
pub const IOAPIC_FLAG_DELIVERY_SMI: u32 = 2u32 << 8;
pub const IOAPIC_FLAG_DELIVERY_NMI: u32 = 4u32 << 8;
pub const IOAPIC_FLAG_DELIVERY_INIT: u32 = 5u32 << 8;
pub const IOAPIC_FLAG_DELIVERY_EXTINT: u32 = 7u32 << 8;

pub const IOAPIC_FLAG_DEST_PHYSICAL: u32 = 0u32 << 11;
pub const IOAPIC_FLAG_DEST_LOGICAL: u32 = 1u32 << 11;

pub const IOAPIC_FLAG_POLARITY_HIGH: u32 = 0u32 << 13;
pub const IOAPIC_FLAG_POLARITY_LOW: u32 = 1u32 << 13;

pub const IOAPIC_FLAG_TRIGGER_EDGE: u32 = 0u32 << 15;
pub const IOAPIC_FLAG_TRIGGER_LEVEL: u32 = 1u32 << 15;

pub const IOAPIC_FLAG_MASK: u32 = 1u32 << 16;
pub const IOAPIC_FLAG_UNMASKED: u32 = 0u32;

#[repr(C, packed)]
struct AcpiRsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    reserved: [u8; 3],
}

#[repr(C, packed)]
struct AcpiSdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

#[repr(C, packed)]
struct AcpiMadt {
    header: AcpiSdtHeader,
    lapic_address: u32,
    flags: u32,
    entries: [u8; 0],
}

#[repr(C, packed)]
struct AcpiMadtEntryHeader {
    entry_type: u8,
    length: u8,
}

#[repr(C, packed)]
struct AcpiMadtIoapicEntry {
    header: AcpiMadtEntryHeader,
    ioapic_id: u8,
    reserved: u8,
    ioapic_address: u32,
    gsi_base: u32,
}

#[repr(C, packed)]
struct AcpiMadtIsoEntry {
    header: AcpiMadtEntryHeader,
    bus_source: u8,
    irq_source: u8,
    gsi: u32,
    flags: u16,
}

#[derive(Clone, Copy)]
struct IoapicController {
    id: u8,
    gsi_base: u32,
    gsi_count: u32,
    version: u32,
    phys_addr: u64,
    reg_select: *mut u32,
    reg_window: *mut u32,
}

impl IoapicController {
    const fn new() -> Self {
        Self {
            id: 0,
            gsi_base: 0,
            gsi_count: 0,
            version: 0,
            phys_addr: 0,
            reg_select: core::ptr::null_mut(),
            reg_window: core::ptr::null_mut(),
        }
    }
}

#[derive(Clone, Copy)]
struct IoapicIso {
    bus_source: u8,
    irq_source: u8,
    gsi: u32,
    flags: u16,
}

impl IoapicIso {
    const fn new() -> Self {
        Self {
            bus_source: 0,
            irq_source: 0,
            gsi: 0,
            flags: 0,
        }
    }
}

struct IoapicTable(UnsafeCell<[IoapicController; IOAPIC_MAX_CONTROLLERS]>);

unsafe impl Sync for IoapicTable {}

impl IoapicTable {
    const fn new() -> Self {
        Self(UnsafeCell::new([IoapicController::new(); IOAPIC_MAX_CONTROLLERS]))
    }

    fn ptr(&self) -> *mut IoapicController {
        self.0.get() as *mut IoapicController
    }
}

static IOAPIC_TABLE: IoapicTable = IoapicTable::new();
static mut ISO_TABLE: [IoapicIso; IOAPIC_MAX_ISO_ENTRIES] =
    [IoapicIso::new(); IOAPIC_MAX_ISO_ENTRIES];
static mut IOAPIC_COUNT: usize = 0;
static mut ISO_COUNT: usize = 0;
static IOAPIC_READY: AtomicBool = AtomicBool::new(false);

#[inline]
fn phys_to_virt(phys: u64) -> *mut u8 {
    if phys == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        if is_hhdm_available() != 0 {
            (phys + get_hhdm_offset()) as *mut u8
        } else {
            phys as *mut u8
        }
    }
}

fn acpi_checksum(table: *const u8, length: usize) -> u8 {
    let mut sum: u8 = 0;
    for i in 0..length {
        unsafe {
            sum = sum.wrapping_add(*table.add(i));
        }
    }
    sum
}

fn acpi_validate_rsdp(rsdp: *const AcpiRsdp) -> bool {
    if rsdp.is_null() {
        return false;
    }
    let rsdp_ref = unsafe { &*rsdp };
    if acpi_checksum(rsdp as *const u8, 20) != 0 {
        return false;
    }
    if rsdp_ref.revision >= 2 && rsdp_ref.length as usize >= mem::size_of::<AcpiRsdp>() {
        if acpi_checksum(rsdp as *const u8, rsdp_ref.length as usize) != 0 {
            return false;
        }
    }
    true
}

fn acpi_validate_table(header: *const AcpiSdtHeader) -> bool {
    if header.is_null() {
        return false;
    }
    let hdr = unsafe { &*header };
    if hdr.length < mem::size_of::<AcpiSdtHeader>() as u32 {
        return false;
    }
    acpi_checksum(header as *const u8, hdr.length as usize) == 0
}

fn acpi_map_table(phys_addr: u64) -> *const AcpiSdtHeader {
    if phys_addr == 0 {
        return core::ptr::null();
    }
    phys_to_virt(phys_addr) as *const AcpiSdtHeader
}

fn acpi_scan_table(
    sdt: *const AcpiSdtHeader,
    entry_size: usize,
    signature: &[u8; 4],
) -> *const AcpiSdtHeader {
    if sdt.is_null() {
        return core::ptr::null();
    }

    let hdr = unsafe { &*sdt };
    if hdr.length < mem::size_of::<AcpiSdtHeader>() as u32 {
        return core::ptr::null();
    }

    let payload_bytes = hdr.length as usize - mem::size_of::<AcpiSdtHeader>();
    let entry_count = payload_bytes / entry_size;
    let entries = (sdt as *const u8).wrapping_add(mem::size_of::<AcpiSdtHeader>());

    for i in 0..entry_count {
        let entry_ptr = unsafe { entries.add(i * entry_size) };
        let phys = if entry_size == 8 {
            unsafe { read_unaligned(entry_ptr as *const u64) }
        } else {
            unsafe { read_unaligned(entry_ptr as *const u32) as u64 }
        };

        let candidate = acpi_map_table(phys);
        if candidate.is_null() {
            continue;
        }
        let candidate_ref = unsafe { &*candidate };
        if candidate_ref.signature != *signature {
            continue;
        }
        if !acpi_validate_table(candidate) {
            klog_info!("ACPI: Found table with invalid checksum, skipping");
            continue;
        }
        return candidate;
    }
    core::ptr::null()
}

fn acpi_find_table(rsdp: *const AcpiRsdp, signature: &[u8; 4]) -> *const AcpiSdtHeader {
    if rsdp.is_null() {
        return core::ptr::null();
    }
    let rsdp_ref = unsafe { &*rsdp };

    if rsdp_ref.revision >= 2 && rsdp_ref.xsdt_address != 0 {
        let xsdt = acpi_map_table(rsdp_ref.xsdt_address);
        if !xsdt.is_null() && acpi_validate_table(xsdt) {
            let hit = acpi_scan_table(xsdt, mem::size_of::<u64>(), signature);
            if !hit.is_null() {
                return hit;
            }
        }
    }

    if rsdp_ref.rsdt_address != 0 {
        let rsdt = acpi_map_table(rsdp_ref.rsdt_address as u64);
        if !rsdt.is_null() && acpi_validate_table(rsdt) {
            let hit = acpi_scan_table(rsdt, mem::size_of::<u32>(), signature);
            if !hit.is_null() {
                return hit;
            }
        }
    }

    core::ptr::null()
}

fn ioapic_find_controller(gsi: u32) -> Option<*mut IoapicController> {
    unsafe {
        let base_ptr = IOAPIC_TABLE.ptr();
        for i in 0..IOAPIC_COUNT {
            let ctrl = &*base_ptr.add(i);
            let start = ctrl.gsi_base;
            let end = ctrl.gsi_base + ctrl.gsi_count.saturating_sub(1);
            if gsi >= start && gsi <= end {
                return Some(base_ptr.add(i));
            }
        }
        None
    }
}

fn ioapic_read(ctrl: &IoapicController, reg: u8) -> u32 {
    if ctrl.reg_select.is_null() || ctrl.reg_window.is_null() {
        return 0;
    }
    unsafe {
        write_volatile(ctrl.reg_select, reg as u32);
        read_volatile(ctrl.reg_window)
    }
}

fn ioapic_write(ctrl: &IoapicController, reg: u8, value: u32) {
    if ctrl.reg_select.is_null() || ctrl.reg_window.is_null() {
        return;
    }
    unsafe {
        write_volatile(ctrl.reg_select, reg as u32);
        write_volatile(ctrl.reg_window, value);
    }
}

#[inline]
fn ioapic_entry_low_index(pin: u32) -> u8 {
    (IOAPIC_REG_REDIR_BASE + (pin * 2) as u8) as u8
}

#[inline]
fn ioapic_entry_high_index(pin: u32) -> u8 {
    ioapic_entry_low_index(pin) + 1
}

fn ioapic_log_controller(ctrl: &IoapicController) {
    klog_info!(
        "IOAPIC: ID 0x{:x} @ phys 0x{:x}, GSIs {}-{}, version 0x{:x}",
        ctrl.id,
        ctrl.phys_addr,
        ctrl.gsi_base,
        ctrl.gsi_base + ctrl.gsi_count.saturating_sub(1),
        ctrl.version & 0xFF
    );
}

fn ioapic_log_iso(iso: &IoapicIso) {
    klog_debug!(
        "IOAPIC: ISO bus {}, IRQ {} -> GSI {}, flags 0x{:x}",
        iso.bus_source,
        iso.irq_source,
        iso.gsi,
        iso.flags
    );
}

fn ioapic_flags_from_acpi(_bus_source: u8, flags: u16) -> u32 {
    let polarity = flags & ACPI_MADT_POLARITY_MASK;
    let mut result = match polarity {
        0 | 1 => IOAPIC_FLAG_POLARITY_HIGH,
        3 => IOAPIC_FLAG_POLARITY_LOW,
        _ => IOAPIC_FLAG_POLARITY_HIGH,
    };

    let trigger = (flags & ACPI_MADT_TRIGGER_MASK) >> ACPI_MADT_TRIGGER_SHIFT;
    result |= match trigger {
        0 | 1 => IOAPIC_FLAG_TRIGGER_EDGE,
        3 => IOAPIC_FLAG_TRIGGER_LEVEL,
        _ => IOAPIC_FLAG_TRIGGER_EDGE,
    };

    result
}

fn ioapic_find_iso(irq: u8) -> Option<&'static IoapicIso> {
    unsafe {
        for i in 0..ISO_COUNT {
            if ISO_TABLE[i].irq_source == irq {
                return Some(&ISO_TABLE[i]);
            }
        }
    }
    None
}

fn ioapic_update_mask(gsi: u32, mask: bool) -> i32 {
    let Some(ctrl_ptr) = ioapic_find_controller(gsi) else {
        klog_info!("IOAPIC: No controller for requested GSI");
        return -1;
    };

    let ctrl = unsafe { &mut *ctrl_ptr };
    let pin = gsi.saturating_sub(ctrl.gsi_base);
    if pin >= ctrl.gsi_count {
        klog_info!("IOAPIC: Pin out of range for mask request");
        return -1;
    }

    let reg = ioapic_entry_low_index(pin);
    let mut value = ioapic_read(ctrl, reg);
    if mask {
        value |= IOAPIC_FLAG_MASK;
    } else {
        value &= !IOAPIC_FLAG_MASK;
    }

    ioapic_write(ctrl, reg, value);
    klog_debug!(
        "IOAPIC: {} GSI {} (pin {}) -> low=0x{:x}",
        if mask { "Masked" } else { "Unmasked" },
        gsi,
        pin,
        value
    );
    0
}

fn ioapic_parse_madt(madt: *const AcpiMadt) {
    if madt.is_null() {
        return;
    }

    unsafe {
        IOAPIC_COUNT = 0;
        ISO_COUNT = 0;
    }

    let cursor = madt as *const u8;
    let end = unsafe { cursor.add((*madt).header.length as usize) };
    let mut ptr = unsafe { (*madt).entries.as_ptr() };

    while unsafe { ptr.add(mem::size_of::<AcpiMadtEntryHeader>()) } <= end {
        let hdr = unsafe { &*(ptr as *const AcpiMadtEntryHeader) };
        if hdr.length == 0 || unsafe { ptr.add(hdr.length as usize) } > end {
            break;
        }

        match hdr.entry_type {
            MADT_ENTRY_IOAPIC => {
                if hdr.length as usize >= mem::size_of::<AcpiMadtIoapicEntry>() {
                    unsafe {
                        if IOAPIC_COUNT >= IOAPIC_MAX_CONTROLLERS {
                            klog_info!(
                                "IOAPIC: Too many controllers, ignoring extra entries"
                            );
                        } else {
                            let entry = &*(ptr as *const AcpiMadtIoapicEntry);
                            let ctrl = &mut *IOAPIC_TABLE.ptr().add(IOAPIC_COUNT);
                            IOAPIC_COUNT += 1;
                            ctrl.id = entry.ioapic_id;
                            ctrl.gsi_base = entry.gsi_base;
                            ctrl.phys_addr = entry.ioapic_address as u64;
                            ctrl.reg_select = phys_to_virt(ctrl.phys_addr) as *mut u32;
                            ctrl.reg_window = phys_to_virt(ctrl.phys_addr + 0x10) as *mut u32;
                            ctrl.version = ioapic_read(ctrl, IOAPIC_REG_VER);
                            ctrl.gsi_count = ((ctrl.version >> 16) & 0xFF) + 1;
                            ioapic_log_controller(ctrl);
                        }
                    }
                }
            }
            MADT_ENTRY_INTERRUPT_OVERRIDE => {
                if hdr.length as usize >= mem::size_of::<AcpiMadtIsoEntry>() {
                    unsafe {
                        if ISO_COUNT >= IOAPIC_MAX_ISO_ENTRIES {
                            klog_info!(
                                "IOAPIC: Too many source overrides, ignoring extras"
                            );
                        } else {
                            let entry = &*(ptr as *const AcpiMadtIsoEntry);
                            let iso = &mut ISO_TABLE[ISO_COUNT];
                            ISO_COUNT += 1;
                            iso.bus_source = entry.bus_source;
                            iso.irq_source = entry.irq_source;
                            iso.gsi = entry.gsi;
                            iso.flags = entry.flags;
                            ioapic_log_iso(iso);
                        }
                    }
                }
            }
            _ => {}
        }

        unsafe {
            ptr = ptr.add(hdr.length as usize);
        }
    }
}

pub fn init() -> i32 {
    if IOAPIC_READY.load(Ordering::Relaxed) {
        return 0;
    }

    if unsafe { is_hhdm_available() } == 0 {
        klog_info!("IOAPIC: HHDM unavailable, cannot map MMIO registers");
        wl_currency::award_loss();
        return -1;
    }

    if unsafe { is_rsdp_available() } == 0 {
        klog_info!("IOAPIC: ACPI RSDP unavailable, skipping IOAPIC init");
        wl_currency::award_loss();
        return -1;
    }

    let rsdp = unsafe { get_rsdp_address() as *const AcpiRsdp };
    if !acpi_validate_rsdp(rsdp) {
        klog_info!("IOAPIC: ACPI RSDP checksum failed");
        wl_currency::award_loss();
        return -1;
    }

    let madt_header = acpi_find_table(rsdp, b"APIC");
    if madt_header.is_null() {
        klog_info!("IOAPIC: MADT not found in ACPI tables");
        wl_currency::award_loss();
        return -1;
    }
    if !acpi_validate_table(madt_header) {
        klog_info!("IOAPIC: MADT checksum invalid");
        wl_currency::award_loss();
        return -1;
    }

    ioapic_parse_madt(madt_header as *const AcpiMadt);

    let count = unsafe { IOAPIC_COUNT };
    if count == 0 {
        klog_info!("IOAPIC: No controllers discovered");
        wl_currency::award_loss();
        return -1;
    }

    klog_info!("IOAPIC: Discovery complete");
    IOAPIC_READY.store(true, Ordering::Relaxed);
    wl_currency::award_win();
    0
}

pub fn config_irq(gsi: u32, vector: u8, lapic_id: u8, flags: u32) -> i32 {
    if !IOAPIC_READY.load(Ordering::Relaxed) {
        klog_info!("IOAPIC: Driver not initialized");
        return -1;
    }

    let Some(ctrl_ptr) = ioapic_find_controller(gsi) else {
        klog_info!("IOAPIC: No IOAPIC handles requested GSI");
        return -1;
    };

    let ctrl = unsafe { &mut *ctrl_ptr };
    let pin = gsi.saturating_sub(ctrl.gsi_base);
    if pin >= ctrl.gsi_count {
        klog_info!("IOAPIC: Calculated pin outside controller range");
        return -1;
    }

    let writable_flags = flags & IOAPIC_REDIR_WRITABLE_MASK;
    let low = vector as u32 | writable_flags;
    let high = (lapic_id as u32) << 24;

    ioapic_write(ctrl, ioapic_entry_high_index(pin), high);
    ioapic_write(ctrl, ioapic_entry_low_index(pin), low);

    klog_info!(
        "IOAPIC: Configured GSI {} (pin {}) -> vector 0x{:x}, LAPIC 0x{:x}, low=0x{:x}, high=0x{:x}",
        gsi,
        pin,
        vector,
        lapic_id,
        low,
        high
    );

    0
}

pub fn mask_gsi(gsi: u32) -> i32 {
    ioapic_update_mask(gsi, true)
}

pub fn unmask_gsi(gsi: u32) -> i32 {
    ioapic_update_mask(gsi, false)
}

pub fn is_ready() -> i32 {
    if IOAPIC_READY.load(Ordering::Relaxed) {
        1
    } else {
        0
    }
}

pub fn legacy_irq_info(legacy_irq: u8, out_gsi: &mut u32, out_flags: &mut u32) -> i32 {
    if IOAPIC_READY.load(Ordering::Relaxed) == false {
        klog_info!("IOAPIC: Legacy route query before initialization");
        return -1;
    }

    let mut gsi = legacy_irq as u32;
    let mut flags = IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE;

    if let Some(iso) = ioapic_find_iso(legacy_irq) {
        gsi = iso.gsi;
        flags = ioapic_flags_from_acpi(iso.bus_source, iso.flags);
        ioapic_log_iso(iso);
    }

    *out_gsi = gsi;
    *out_flags = flags;
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_init() -> i32 {
    init()
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_config_irq(gsi: u32, vector: u8, lapic_id: u8, flags: u32) -> i32 {
    config_irq(gsi, vector, lapic_id, flags)
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_mask_gsi(gsi: u32) -> i32 {
    mask_gsi(gsi)
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_unmask_gsi(gsi: u32) -> i32 {
    unmask_gsi(gsi)
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_is_ready() -> i32 {
    is_ready()
}

#[unsafe(no_mangle)]
pub extern "C" fn ioapic_legacy_irq_info(
    legacy_irq: u8,
    out_gsi: *mut u32,
    out_flags: *mut u32,
) -> i32 {
    if out_gsi.is_null() || out_flags.is_null() {
        return -1;
    }
    unsafe { legacy_irq_info(legacy_irq, &mut *out_gsi, &mut *out_flags) }
}
