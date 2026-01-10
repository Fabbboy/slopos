use core::cell::UnsafeCell;
use core::mem;
use core::ptr::read_unaligned;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_lib::{klog_debug, klog_info};

use crate::sched_bridge;
use crate::wl_currency;
use slopos_abi::addr::PhysAddr;
use slopos_abi::arch::x86_64::ioapic::*;
use slopos_mm::hhdm;
use slopos_mm::mmio::MmioRegion;

/// IOAPIC register region size (one 4KB page covers both IOREGSEL and IOWIN).
const IOAPIC_REGION_SIZE: usize = 0x20;

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
    /// MMIO region for this controller (mapped via HHDM).
    mmio: Option<MmioRegion>,
}

impl IoapicController {
    const fn new() -> Self {
        Self {
            id: 0,
            gsi_base: 0,
            gsi_count: 0,
            version: 0,
            phys_addr: 0,
            mmio: None,
        }
    }

    /// Read from IOAPIC register via MMIO.
    #[inline]
    fn read_reg(&self, reg: u8) -> u32 {
        let region = match self.mmio {
            Some(region) => region,
            None => return 0,
        };
        region.write_u32(0x00, reg as u32);
        region.read_u32(0x10)
    }

    /// Write to IOAPIC register via MMIO.
    #[inline]
    fn write_reg(&self, reg: u8, value: u32) {
        let region = match self.mmio {
            Some(region) => region,
            None => return,
        };
        region.write_u32(0x00, reg as u32);
        region.write_u32(0x10, value);
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
        Self(UnsafeCell::new(
            [IoapicController::new(); IOAPIC_MAX_CONTROLLERS],
        ))
    }

    fn ptr(&self) -> *mut IoapicController {
        self.0.get() as *mut IoapicController
    }
}

struct IoapicIsoTable(UnsafeCell<[IoapicIso; IOAPIC_MAX_ISO_ENTRIES]>);

unsafe impl Sync for IoapicIsoTable {}

impl IoapicIsoTable {
    const fn new() -> Self {
        Self(UnsafeCell::new([IoapicIso::new(); IOAPIC_MAX_ISO_ENTRIES]))
    }

    fn ptr(&self) -> *mut IoapicIso {
        self.0.get() as *mut IoapicIso
    }
}

static IOAPIC_TABLE: IoapicTable = IoapicTable::new();
static ISO_TABLE: IoapicIsoTable = IoapicIsoTable::new();
static IOAPIC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ISO_COUNT: AtomicUsize = AtomicUsize::new(0);
static IOAPIC_READY: AtomicBool = AtomicBool::new(false);
static IOAPIC_INIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Map IOAPIC MMIO region, returning the virtual base address.
/// Returns 0 if mapping fails (HHDM unavailable or invalid address).
#[inline]
fn map_ioapic_mmio(phys: u64) -> Option<MmioRegion> {
    if phys == 0 {
        return None;
    }
    MmioRegion::map(PhysAddr::new(phys), IOAPIC_REGION_SIZE)
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
    // Map via HHDM for ACPI table access
    use slopos_mm::hhdm::PhysAddrHhdm;
    PhysAddr::new(phys_addr)
        .try_to_virt()
        .map(|v| v.as_ptr())
        .unwrap_or(core::ptr::null())
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
        let count = IOAPIC_COUNT.load(Ordering::Relaxed);
        for i in 0..count {
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
        let count = ISO_COUNT.load(Ordering::Relaxed);
        let base_ptr = ISO_TABLE.ptr();
        for i in 0..count {
            let iso = &*base_ptr.add(i);
            if iso.irq_source == irq {
                return Some(iso);
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

    let ctrl = unsafe { &*ctrl_ptr };
    let pin = gsi.saturating_sub(ctrl.gsi_base);
    if pin >= ctrl.gsi_count {
        klog_info!("IOAPIC: Pin out of range for mask request");
        return -1;
    }

    let reg = ioapic_entry_low_index(pin);
    let mut value = ctrl.read_reg(reg);
    if mask {
        value |= IOAPIC_FLAG_MASK;
    } else {
        value &= !IOAPIC_FLAG_MASK;
    }

    ctrl.write_reg(reg, value);
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

    IOAPIC_COUNT.store(0, Ordering::Relaxed);
    ISO_COUNT.store(0, Ordering::Relaxed);

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
                        let ioapic_index = IOAPIC_COUNT.load(Ordering::Relaxed);
                        if ioapic_index >= IOAPIC_MAX_CONTROLLERS {
                            klog_info!("IOAPIC: Too many controllers, ignoring extra entries");
                        } else {
                            let entry = &*(ptr as *const AcpiMadtIoapicEntry);
                            let ctrl = &mut *IOAPIC_TABLE.ptr().add(ioapic_index);
                            IOAPIC_COUNT.store(ioapic_index + 1, Ordering::Relaxed);
                            ctrl.id = entry.ioapic_id;
                            ctrl.gsi_base = entry.gsi_base;
                            ctrl.phys_addr = entry.ioapic_address as u64;
                            ctrl.mmio = map_ioapic_mmio(ctrl.phys_addr);
                            ctrl.version = ctrl.read_reg(IOAPIC_REG_VER);
                            ctrl.gsi_count = ((ctrl.version >> 16) & 0xFF) + 1;
                            ioapic_log_controller(ctrl);
                        }
                    }
                }
            }
            MADT_ENTRY_INTERRUPT_OVERRIDE => {
                if hdr.length as usize >= mem::size_of::<AcpiMadtIsoEntry>() {
                    unsafe {
                        let iso_index = ISO_COUNT.load(Ordering::Relaxed);
                        if iso_index >= IOAPIC_MAX_ISO_ENTRIES {
                            klog_info!("IOAPIC: Too many source overrides, ignoring extras");
                        } else {
                            let entry = &*(ptr as *const AcpiMadtIsoEntry);
                            let iso = &mut *ISO_TABLE.ptr().add(iso_index);
                            ISO_COUNT.store(iso_index + 1, Ordering::Relaxed);
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
    if IOAPIC_READY.load(Ordering::Acquire) {
        return 0;
    }
    if IOAPIC_INIT_IN_PROGRESS
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        while !IOAPIC_READY.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        return 0;
    }

    let init_fail = || {
        IOAPIC_INIT_IN_PROGRESS.store(false, Ordering::Release);
        -1
    };

    if !hhdm::is_available() {
        klog_info!("IOAPIC: HHDM unavailable, cannot map MMIO registers");
        wl_currency::award_loss();
        return init_fail();
    }

    if sched_bridge::is_rsdp_available() == 0 {
        klog_info!("IOAPIC: ACPI RSDP unavailable, skipping IOAPIC init");
        wl_currency::award_loss();
        return init_fail();
    }

    let rsdp = sched_bridge::get_rsdp_address() as *const AcpiRsdp;
    if !acpi_validate_rsdp(rsdp) {
        klog_info!("IOAPIC: ACPI RSDP checksum failed");
        wl_currency::award_loss();
        return init_fail();
    }

    let madt_header = acpi_find_table(rsdp, b"APIC");
    if madt_header.is_null() {
        klog_info!("IOAPIC: MADT not found in ACPI tables");
        wl_currency::award_loss();
        return init_fail();
    }
    if !acpi_validate_table(madt_header) {
        klog_info!("IOAPIC: MADT checksum invalid");
        wl_currency::award_loss();
        return init_fail();
    }

    ioapic_parse_madt(madt_header as *const AcpiMadt);

    let count = IOAPIC_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        klog_info!("IOAPIC: No controllers discovered");
        wl_currency::award_loss();
        return init_fail();
    }

    klog_info!("IOAPIC: Discovery complete");
    IOAPIC_READY.store(true, Ordering::Release);
    IOAPIC_INIT_IN_PROGRESS.store(false, Ordering::Release);
    wl_currency::award_win();
    0
}

pub fn config_irq(gsi: u32, vector: u8, lapic_id: u8, flags: u32) -> i32 {
    if !IOAPIC_READY.load(Ordering::Acquire) {
        klog_info!("IOAPIC: Driver not initialized");
        return -1;
    }

    let Some(ctrl_ptr) = ioapic_find_controller(gsi) else {
        klog_info!("IOAPIC: No IOAPIC handles requested GSI");
        return -1;
    };

    let ctrl = unsafe { &*ctrl_ptr };
    let pin = gsi.saturating_sub(ctrl.gsi_base);
    if pin >= ctrl.gsi_count {
        klog_info!("IOAPIC: Calculated pin outside controller range");
        return -1;
    }

    let writable_flags = flags & IOAPIC_REDIR_WRITABLE_MASK;
    let low = vector as u32 | writable_flags;
    let high = (lapic_id as u32) << 24;

    ctrl.write_reg(ioapic_entry_high_index(pin), high);
    ctrl.write_reg(ioapic_entry_low_index(pin), low);

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
    if IOAPIC_READY.load(Ordering::Acquire) {
        1
    } else {
        0
    }
}

pub fn legacy_irq_info(legacy_irq: u8, out_gsi: &mut u32, out_flags: &mut u32) -> i32 {
    if IOAPIC_READY.load(Ordering::Acquire) == false {
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
