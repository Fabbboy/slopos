pub(crate) mod regs;
#[cfg(feature = "test-hooks")]
pub mod tests;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::sync::{InitFlag, StateFlag};
use slopos_utils::{klog_debug, klog_info};

use regs::*;
use slopos_abi::addr::PhysAddr;
use slopos_acpi::madt::{InterruptOverride, Madt, MadtEntry, Polarity, TriggerMode};
use slopos_acpi::tables::{AcpiTables, Rsdp};
use slopos_kernel_services::platform;
use slopos_mm::hhdm;
use slopos_mm::mmio::MmioRegion;

const IOAPIC_REGION_SIZE: usize = 0x20;

#[derive(Clone, Copy)]
struct IoapicController {
    id: u8,
    gsi_base: u32,
    gsi_count: u32,
    version: u32,
    phys_addr: u64,
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

    #[inline]
    fn read_reg(&self, reg: u8) -> u32 {
        let region = match self.mmio {
            Some(region) => region,
            None => return 0,
        };
        region.write::<u32>(0x00, reg as u32);
        region.read::<u32>(0x10)
    }

    #[inline]
    fn write_reg(&self, reg: u8, value: u32) {
        let region = match self.mmio {
            Some(region) => region,
            None => return,
        };
        region.write::<u32>(0x00, reg as u32);
        region.write::<u32>(0x10, value);
    }
}

#[derive(Clone, Copy)]
struct IoapicIso {
    irq_source: u8,
    gsi: u32,
    redir_flags: u32,
}

impl IoapicIso {
    const fn new() -> Self {
        Self {
            irq_source: 0,
            gsi: 0,
            redir_flags: 0,
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
static IOAPIC_READY: InitFlag = InitFlag::new();
static IOAPIC_INIT_IN_PROGRESS: StateFlag = StateFlag::new();

#[inline]
fn map_ioapic_mmio(phys: u64) -> Option<MmioRegion> {
    if phys == 0 {
        return None;
    }
    MmioRegion::map(PhysAddr::new(phys), IOAPIC_REGION_SIZE)
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
        "IOAPIC: ISO IRQ {} -> GSI {}, redir_flags 0x{:x}",
        iso.irq_source,
        iso.gsi,
        iso.redir_flags
    );
}

fn redir_flags_from_override(ov: &InterruptOverride) -> u32 {
    let polarity = match ov.polarity() {
        Polarity::ActiveLow => IOAPIC_FLAG_POLARITY_LOW,
        Polarity::ActiveHigh | Polarity::BusDefault => IOAPIC_FLAG_POLARITY_HIGH,
    };

    let trigger = match ov.trigger_mode() {
        TriggerMode::Level => IOAPIC_FLAG_TRIGGER_LEVEL,
        TriggerMode::Edge | TriggerMode::BusDefault => IOAPIC_FLAG_TRIGGER_EDGE,
    };

    polarity | trigger
}

fn find_iso(irq: u8) -> Option<&'static IoapicIso> {
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
    0
}

fn populate_from_madt(madt: &Madt) {
    IOAPIC_COUNT.store(0, Ordering::Relaxed);
    ISO_COUNT.store(0, Ordering::Relaxed);

    for entry in madt.entries() {
        match entry {
            MadtEntry::Ioapic(info) => unsafe {
                let idx = IOAPIC_COUNT.load(Ordering::Relaxed);
                if idx >= IOAPIC_MAX_CONTROLLERS {
                    klog_info!("IOAPIC: Too many controllers, ignoring extra entries");
                    continue;
                }
                let ctrl = &mut *IOAPIC_TABLE.ptr().add(idx);
                IOAPIC_COUNT.store(idx + 1, Ordering::Relaxed);
                ctrl.id = info.id;
                ctrl.gsi_base = info.gsi_base;
                ctrl.phys_addr = info.address as u64;
                ctrl.mmio = map_ioapic_mmio(ctrl.phys_addr);
                ctrl.version = ctrl.read_reg(IOAPIC_REG_VER);
                ctrl.gsi_count = ((ctrl.version >> 16) & 0xFF) + 1;
                ioapic_log_controller(ctrl);
            },
            MadtEntry::InterruptOverride(ov) => unsafe {
                let idx = ISO_COUNT.load(Ordering::Relaxed);
                if idx >= IOAPIC_MAX_ISO_ENTRIES {
                    klog_info!("IOAPIC: Too many source overrides, ignoring extras");
                    continue;
                }
                let iso = &mut *ISO_TABLE.ptr().add(idx);
                ISO_COUNT.store(idx + 1, Ordering::Relaxed);
                iso.irq_source = ov.irq_source;
                iso.gsi = ov.gsi;
                iso.redir_flags = redir_flags_from_override(&ov);
                ioapic_log_iso(iso);
            },
            MadtEntry::Unknown { .. } => {}
        }
    }
}

pub fn init() -> i32 {
    if IOAPIC_READY.is_set() {
        return 0;
    }
    if !IOAPIC_INIT_IN_PROGRESS.enter() {
        while !IOAPIC_READY.is_set() {
            core::hint::spin_loop();
        }
        return 0;
    }

    let init_fail = || {
        IOAPIC_INIT_IN_PROGRESS.leave();
        -1
    };

    if !hhdm::is_available() {
        klog_info!("IOAPIC: HHDM unavailable, cannot map MMIO registers");
        return init_fail();
    }

    if !platform::is_rsdp_available() {
        klog_info!("IOAPIC: ACPI RSDP unavailable, skipping IOAPIC init");
        return init_fail();
    }

    let rsdp = platform::get_rsdp_address() as *const Rsdp;
    let Some(tables) = AcpiTables::from_rsdp(rsdp) else {
        klog_info!("IOAPIC: ACPI tables validation failed");
        return init_fail();
    };

    let Some(madt) = Madt::from_tables(&tables) else {
        klog_info!("IOAPIC: MADT not found in ACPI tables");
        return init_fail();
    };

    populate_from_madt(&madt);

    let count = IOAPIC_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        klog_info!("IOAPIC: No controllers discovered");
        return init_fail();
    }

    klog_info!("IOAPIC: Discovery complete");
    IOAPIC_READY.mark_set();
    IOAPIC_INIT_IN_PROGRESS.leave();
    0
}

pub fn config_irq(gsi: u32, vector: u8, lapic_id: u8, flags: u32) -> i32 {
    if !IOAPIC_READY.is_set() {
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
    if IOAPIC_READY.is_set() { 1 } else { 0 }
}

pub fn legacy_irq_info(legacy_irq: u8, out_gsi: &mut u32, out_flags: &mut u32) -> i32 {
    if !IOAPIC_READY.is_set() {
        klog_info!("IOAPIC: Legacy route query before initialization");
        return -1;
    }

    let mut gsi = legacy_irq as u32;
    let mut flags = IOAPIC_FLAG_POLARITY_HIGH | IOAPIC_FLAG_TRIGGER_EDGE;

    if let Some(iso) = find_iso(legacy_irq) {
        gsi = iso.gsi;
        flags = iso.redir_flags;
        ioapic_log_iso(iso);
    }

    *out_gsi = gsi;
    *out_flags = flags;
    0
}
