#![allow(static_mut_refs)]

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_abi::PhysAddr;
use slopos_lib::klog_info;
use slopos_lib::ports::{PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA};
use slopos_lib::string::cstr_to_str;
use slopos_mm::mmio::MmioRegion;

use crate::wl_currency;
pub use slopos_abi::arch::x86_64::pci::{PciBarInfo, PciDeviceInfo, *};

const PCI_VENDOR_ID: u8 = PCI_VENDOR_ID_OFFSET;
const PCI_DEVICE_ID: u8 = PCI_DEVICE_ID_OFFSET;
const PCI_CLASS: u8 = PCI_CLASS_CODE_OFFSET;
const PCI_SUBCLASS: u8 = PCI_SUBCLASS_OFFSET;
const PCI_PROG_IF: u8 = PCI_PROG_IF_OFFSET;
const PCI_REVISION: u8 = PCI_REVISION_ID_OFFSET;
const PCI_HEADER_TYPE: u8 = PCI_HEADER_TYPE_OFFSET;
const PCI_INTERRUPT_LINE: u8 = PCI_INTERRUPT_LINE_OFFSET;
const PCI_INTERRUPT_PIN: u8 = PCI_INTERRUPT_PIN_OFFSET;
const PCI_BAR0: u8 = PCI_BAR0_OFFSET;
const PCI_SECONDARY_BUS: u8 = 0x19;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciGpuInfo {
    pub present: c_int,
    pub device: PciDeviceInfo,
    pub mmio_phys_base: u64,
    pub mmio_region: MmioRegion,
    pub mmio_size: u64,
}

impl PciGpuInfo {
    pub const fn zeroed() -> Self {
        Self {
            present: 0,
            device: PciDeviceInfo::zeroed(),
            mmio_phys_base: 0,
            mmio_region: MmioRegion::empty(),
            mmio_size: 0,
        }
    }
}

#[repr(C)]
pub struct PciDriver {
    pub name: *const u8,
    pub match_fn: Option<fn(*const PciDeviceInfo, *mut core::ffi::c_void) -> bool>,
    pub probe: Option<fn(*const PciDeviceInfo, *mut core::ffi::c_void) -> c_int>,
    pub context: *mut core::ffi::c_void,
}

unsafe impl Sync for PciDriver {}

static mut BUS_VISITED: [u8; PCI_MAX_BUSES] = [0; PCI_MAX_BUSES];
static mut DEVICES: [PciDeviceInfo; PCI_MAX_DEVICES] = [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES];
static mut DEVICE_COUNT: usize = 0;
static mut PCI_INITIALIZED: c_int = 0;
static mut PRIMARY_GPU: PciGpuInfo = PciGpuInfo::zeroed();
static mut PCI_REGISTERED_DRIVERS: [*const PciDriver; PCI_DRIVER_MAX] =
    [ptr::null(); PCI_DRIVER_MAX];
static mut PCI_REGISTERED_DRIVER_COUNT: usize = 0;

fn cstr_or_placeholder(ptr: *const u8) -> &'static str {
    unsafe { cstr_to_str(ptr as *const c_char) }
}

#[inline(always)]
fn pci_config_addr(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        PCI_CONFIG_ADDRESS.write(pci_config_addr(bus, device, function, offset));
        PCI_CONFIG_DATA.read()
    }
}

pub fn pci_config_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let value = pci_config_read32(bus, device, function, offset);
    ((value >> ((offset & 0x2) * 8)) & 0xFFFF) as u16
}

pub fn pci_config_read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let value = pci_config_read32(bus, device, function, offset);
    ((value >> ((offset & 0x3) * 8)) & 0xFF) as u8
}

pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        PCI_CONFIG_ADDRESS.write(pci_config_addr(bus, device, function, offset));
        PCI_CONFIG_DATA.write(value);
    }
}

pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let dword = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x2) * 8;
    let mask = !(0xFFFF << shift);
    let new_dword = (dword & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_dword);
}

pub fn pci_config_write8(bus: u8, device: u8, function: u8, offset: u8, value: u8) {
    let dword = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x3) * 8;
    let mask = !(0xFF << shift);
    let new_dword = (dword & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_dword);
}

fn pci_read_vendor_id(bus: u8, device: u8, function: u8) -> u16 {
    pci_config_read16(bus, device, function, PCI_VENDOR_ID)
}

fn pci_read_header_type(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_HEADER_TYPE)
}

fn pci_is_multifunction(bus: u8, device: u8) -> bool {
    (pci_read_header_type(bus, device, 0) & 0x80) != 0
}

fn pci_get_secondary_bus(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_SECONDARY_BUS)
}

fn pci_probe_bar(bus: u8, device: u8, function: u8, bar_idx: u8) -> PciBarInfo {
    let bar_offset = PCI_BAR0 + bar_idx * 4;
    let original = pci_config_read32(bus, device, function, bar_offset);
    let is_io = (original & 1) != 0;

    pci_config_write32(bus, device, function, bar_offset, 0xFFFF_FFFF);
    let size_mask = pci_config_read32(bus, device, function, bar_offset);
    pci_config_write32(bus, device, function, bar_offset, original);

    if size_mask == 0 || size_mask == 0xFFFF_FFFF {
        return PciBarInfo::zeroed();
    }

    if is_io {
        let base = (original & !0x3) as u64;
        let size = (!((size_mask as u64) | 0xFFFF_FFFF_FFFF_0003) + 1) as u64;
        PciBarInfo {
            base,
            size,
            is_io: 1,
            is_64bit: 0,
            prefetchable: 0,
        }
    } else {
        let is_64bit = ((original >> 1) & 0x3) == 2;
        let is_prefetchable = ((original >> 3) & 1) != 0;
        let base_low = (original & !0xF) as u64;
        let base_high = if is_64bit && bar_idx < 5 {
            pci_config_read32(bus, device, function, bar_offset + 4) as u64
        } else {
            0
        };
        let base = base_low | (base_high << 32);
        let size = (!((size_mask as u64) | 0xF) + 1) as u64;
        PciBarInfo {
            base,
            size,
            is_io: 0,
            is_64bit: is_64bit as u8,
            prefetchable: is_prefetchable as u8,
        }
    }
}

fn pci_probe_device(bus: u8, device: u8, function: u8) {
    let vendor = pci_read_vendor_id(bus, device, function);
    if vendor == 0xFFFF {
        return;
    }

    let device_id = pci_config_read16(bus, device, function, PCI_DEVICE_ID);
    let class = pci_config_read8(bus, device, function, PCI_CLASS);
    let subclass = pci_config_read8(bus, device, function, PCI_SUBCLASS);
    let prog_if = pci_config_read8(bus, device, function, PCI_PROG_IF);
    let revision = pci_config_read8(bus, device, function, PCI_REVISION);
    let header_type = pci_read_header_type(bus, device, function) & 0x7F;
    let interrupt_line = pci_config_read8(bus, device, function, PCI_INTERRUPT_LINE);
    let interrupt_pin = pci_config_read8(bus, device, function, PCI_INTERRUPT_PIN);

    let mut bars = [PciBarInfo::zeroed(); PCI_MAX_BARS];
    let mut bar_count = 0u8;
    if header_type == 0 {
        let mut bar_idx = 0u8;
        while bar_idx < 6 {
            let bar = pci_probe_bar(bus, device, function, bar_idx);
            bars[bar_idx as usize] = bar;
            if bar.base != 0 || bar.size != 0 {
                bar_count = bar_idx + 1;
            }
            if bar.is_64bit != 0 {
                bar_idx += 1;
            }
            bar_idx += 1;
        }
    }

    let info = PciDeviceInfo {
        bus,
        device,
        function,
        vendor_id: vendor,
        device_id,
        class_code: class,
        subclass,
        prog_if,
        revision,
        header_type,
        irq_line: interrupt_line,
        irq_pin: interrupt_pin,
        bar_count,
        bars,
    };

    unsafe {
        if DEVICE_COUNT < PCI_MAX_DEVICES {
            DEVICES[DEVICE_COUNT] = info;
            DEVICE_COUNT += 1;
        }
    }

    klog_info!(
        "PCI: [Bus {} Dev {} Func {}] VID=0x{:04x} DID=0x{:04x} Class=0x{:02x}:{:02x} ProgIF=0x{:02x} Rev=0x{:02x}",
        bus,
        device,
        function,
        vendor,
        device_id,
        class,
        subclass,
        prog_if,
        revision
    );

    for (i, bar) in bars.iter().enumerate() {
        if bar.base != 0 || bar.size != 0 {
            if bar.is_io != 0 {
                klog_info!("    BAR{}: IO base=0x{:x} size={}", i, bar.base, bar.size);
            } else {
                let pf = if bar.prefetchable != 0 {
                    "prefetch"
                } else {
                    "non-prefetch"
                };
                let bits = if bar.is_64bit != 0 { "64bit" } else { "32bit" };
                klog_info!(
                    "    BAR{}: MMIO base=0x{:x} size=0x{:x} {} {}",
                    i,
                    bar.base,
                    bar.size,
                    pf,
                    bits
                );
            }
        }
    }

    if class == 0x03 && subclass == 0x00 {
        for bar in &bars {
            if bar.is_io == 0 && bar.base != 0 && bar.size != 0 {
                unsafe {
                    if PRIMARY_GPU.present == 0 {
                        PRIMARY_GPU.present = 1;
                        PRIMARY_GPU.device = info;
                        PRIMARY_GPU.mmio_phys_base = bar.base;
                        PRIMARY_GPU.mmio_size = bar.size;

                        let phys = PhysAddr::new(bar.base);
                        PRIMARY_GPU.mmio_region = MmioRegion::map(phys, bar.size as usize)
                            .unwrap_or_else(MmioRegion::empty);
                        klog_info!(
                            "PCI: Selected display-class GPU candidate at MMIO phys=0x{:x} size=0x{:x} virt=0x{:x}",
                            bar.base,
                            bar.size,
                            PRIMARY_GPU.mmio_region.virt_base()
                        );
                    }
                }
                break;
            }
        }
    }

    if header_type == 1 {
        let secondary = pci_get_secondary_bus(bus, device, function);
        pci_scan_bus(secondary);
    }
}

fn pci_scan_bus(bus: u8) {
    unsafe {
        if BUS_VISITED[bus as usize] != 0 {
            return;
        }
        BUS_VISITED[bus as usize] = 1;
    }

    for device in 0..32u8 {
        let vendor = pci_read_vendor_id(bus, device, 0);
        if vendor == 0xFFFF {
            continue;
        }

        pci_probe_device(bus, device, 0);

        if pci_is_multifunction(bus, device) {
            for function in 1..8u8 {
                if pci_read_vendor_id(bus, device, function) != 0xFFFF {
                    pci_probe_device(bus, device, function);
                }
            }
        }
    }
}

pub fn pci_init() {
    unsafe {
        if PCI_INITIALIZED != 0 {
            return;
        }
        PCI_INITIALIZED = 1;
        DEVICE_COUNT = 0;
        BUS_VISITED = [0; PCI_MAX_BUSES];
        PRIMARY_GPU = PciGpuInfo::zeroed();
    }

    klog_info!("PCI: Initializing PCI subsystem");
    pci_scan_bus(0);

    let header_type = pci_read_header_type(0, 0, 0);
    if (header_type & 0x80) != 0 {
        for function in 1..8u8 {
            if pci_read_vendor_id(0, 0, function) != 0xFFFF {
                pci_scan_bus(function);
            }
        }
    }

    let count = unsafe { DEVICE_COUNT };
    klog_info!("PCI: Enumeration complete. Devices discovered: {}", count);
}

pub fn pci_get_device_count() -> usize {
    unsafe { DEVICE_COUNT }
}

pub fn pci_get_device(index: usize) -> Option<&'static PciDeviceInfo> {
    unsafe {
        if index < DEVICE_COUNT {
            Some(&DEVICES[index])
        } else {
            None
        }
    }
}

pub fn pci_get_primary_gpu() -> *const PciGpuInfo {
    unsafe { &PRIMARY_GPU }
}

pub fn pci_register_driver(driver: &'static PciDriver) -> c_int {
    unsafe {
        if PCI_REGISTERED_DRIVER_COUNT >= PCI_DRIVER_MAX {
            return -1;
        }
        let name = cstr_or_placeholder(driver.name);
        klog_info!("PCI: Registered driver {}", name);
        PCI_REGISTERED_DRIVERS[PCI_REGISTERED_DRIVER_COUNT] = driver;
        PCI_REGISTERED_DRIVER_COUNT += 1;
        wl_currency::award_win();
        0
    }
}

pub fn pci_probe_drivers() {
    unsafe {
        for drv_idx in 0..PCI_REGISTERED_DRIVER_COUNT {
            let drv = &*PCI_REGISTERED_DRIVERS[drv_idx];
            for dev_idx in 0..DEVICE_COUNT {
                let dev = &DEVICES[dev_idx];
                if let Some(mf) = drv.match_fn {
                    if mf(dev, drv.context) {
                        if let Some(probe) = drv.probe {
                            let result = probe(dev, drv.context);
                            if result == 0 {
                                wl_currency::award_win();
                            } else {
                                wl_currency::award_loss();
                            }
                        }
                    }
                }
            }
        }
    }
}
