#![allow(static_mut_refs)]

use core::arch::asm;
use core::ffi::{CStr, c_char, c_int};
use core::ptr;

use slopos_lib::{klog_debug, klog_info};

use slopos_abi::arch::x86_64::pci::*;
use slopos_abi::arch::x86_64::ports::{PCI_CONFIG_ADDRESS, PCI_CONFIG_DATA};
use crate::wl_currency;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciBarInfo {
    pub base: u64,
    pub size: u64,
    pub is_io: u8,
    pub is_64bit: u8,
    pub prefetchable: u8,
}

impl PciBarInfo {
    pub const fn zeroed() -> Self {
        Self {
            base: 0,
            size: 0,
            is_io: 0,
            is_64bit: 0,
            prefetchable: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub bar_count: u8,
    pub bars: [PciBarInfo; PCI_MAX_BARS],
}

impl PciDeviceInfo {
    pub const fn zeroed() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0,
            device_id: 0,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            header_type: 0,
            irq_line: 0,
            irq_pin: 0,
            bar_count: 0,
            bars: [PciBarInfo::zeroed(); PCI_MAX_BARS],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PciGpuInfo {
    pub present: c_int,
    pub device: PciDeviceInfo,
    pub mmio_phys_base: u64,
    pub mmio_virt_base: *mut u8,
    pub mmio_size: u64,
}

impl PciGpuInfo {
    pub const fn zeroed() -> Self {
        Self {
            present: 0,
            device: PciDeviceInfo::zeroed(),
            mmio_phys_base: 0,
            mmio_virt_base: ptr::null_mut(),
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

// SAFETY: PciDriver stores raw pointers for static driver metadata and an
// optional context pointer. The driver registry is populated during
// single-threaded boot and then treated as immutable; any context data is
// either immutable or otherwise synchronized by the driver itself. Under these
// rules, sharing PciDriver references across threads is safe.
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
    if ptr.is_null() {
        return "<null>";
    }
    unsafe { CStr::from_ptr(ptr as *const c_char) }
        .to_str()
        .unwrap_or("<invalid utf-8>")
}

#[inline(always)]
unsafe fn outl(port: u16, value: u32) {
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline(always)]
unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        asm!(
            "in eax, dx",
            out("eax") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Build PCI configuration space address for port 0xCF8
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
        outl(PCI_CONFIG_ADDRESS, pci_config_addr(bus, device, function, offset));
        inl(PCI_CONFIG_DATA)
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
        outl(PCI_CONFIG_ADDRESS, pci_config_addr(bus, device, function, offset));
        outl(PCI_CONFIG_DATA, value);
    }
}

pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let current = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x2) * 8;
    let mask = !(0xFFFFu32 << shift);
    let new_value = (current & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_value);
}

pub fn pci_config_write8(bus: u8, device: u8, function: u8, offset: u8, value: u8) {
    let current = pci_config_read32(bus, device, function, offset);
    let shift = (offset & 0x3) * 8;
    let mask = !(0xFFu32 << shift);
    let new_value = (current & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset, new_value);
}

fn pci_probe_bar_size(bus: u8, device: u8, function: u8, offset: u8, original_value: u32) -> u64 {
    if original_value == 0 {
        return 0;
    }

    if (original_value & PCI_BAR_IO_SPACE) != 0 {
        pci_config_write32(bus, device, function, offset, 0xFFFF_FFFF);
        let size_mask = pci_config_read32(bus, device, function, offset);
        pci_config_write32(bus, device, function, offset, original_value);
        let masked = size_mask & PCI_BAR_IO_ADDRESS_MASK;
        if masked == 0 {
            return 0;
        }
        let size = (!masked + 1) & 0xFFFF_FFFF;
        return size as u64;
    }

    let bar_mem_type = original_value & PCI_BAR_MEM_TYPE_MASK;
    let is_64bit = bar_mem_type == PCI_BAR_MEM_TYPE_64;

    pci_config_write32(bus, device, function, offset, 0xFFFF_FFFF);
    let size_low = pci_config_read32(bus, device, function, offset);
    pci_config_write32(bus, device, function, offset, original_value);

    let mut mask: u64 = (size_low & PCI_BAR_MEM_ADDRESS_MASK) as u64;
    let mut size_value: u64 = (!mask + 1) as u64;

    if is_64bit {
        let original_high = pci_config_read32(bus, device, function, offset + 4);
        pci_config_write32(bus, device, function, offset + 4, 0xFFFF_FFFF);
        let size_high = pci_config_read32(bus, device, function, offset + 4);
        pci_config_write32(bus, device, function, offset + 4, original_high);

        mask |= (size_high as u64) << 32;
        size_value = (!mask).wrapping_add(1);
    }
    size_value
}

fn pci_log_device_header(info: &PciDeviceInfo) {
    klog_info!(
        "PCI: [Bus {} Dev {} Func {}] VID=0x{:04x} DID=0x{:04x} Class=0x{:02x}:{:02x} ProgIF=0x{:02x} Rev=0x{:02x}",
        info.bus,
        info.device,
        info.function,
        info.vendor_id,
        info.device_id,
        info.class_code,
        info.subclass,
        info.prog_if,
        info.revision
    );
}

fn pci_log_bar(bar: &PciBarInfo, index: u8) {
    if bar.is_io != 0 {
        if bar.size != 0 {
            klog_info!(
                "    BAR{}: IO base=0x{:x} size={}",
                index,
                bar.base,
                bar.size
            );
        } else {
            klog_info!("    BAR{}: IO base=0x{:x}", index, bar.base);
        }
    } else {
        let prefetch = if bar.prefetchable != 0 {
            "prefetch"
        } else {
            "non-prefetch"
        };
        let width = if bar.is_64bit != 0 { "64bit" } else { "32bit" };
        if bar.size != 0 {
            klog_info!(
                "    BAR{}: MMIO base=0x{:x} size=0x{:x} {} {}",
                index,
                bar.base,
                bar.size,
                prefetch,
                width
            );
        } else {
            klog_info!(
                "    BAR{}: MMIO base=0x{:x} {} {}",
                index,
                bar.base,
                prefetch,
                width
            );
        }
    }
}

use slopos_abi::addr::PhysAddr;
use slopos_mm::phys_virt::mm_map_mmio_region;

fn pci_consider_gpu_candidate(info: &PciDeviceInfo) {
    let virtio_candidate = pci_is_virtio_gpu(info);

    unsafe {
        if PRIMARY_GPU.present != 0 {
            return;
        }
    }

    if !pci_is_gpu_candidate(info) {
        return;
    }

    for i in 0..info.bar_count as usize {
        let bar = &info.bars[i];
        if bar.is_io != 0 || bar.base == 0 {
            continue;
        }

        unsafe {
            PRIMARY_GPU.present = 1;
            PRIMARY_GPU.device = *info;
            PRIMARY_GPU.mmio_phys_base = bar.base;
            PRIMARY_GPU.mmio_size = if bar.size != 0 { bar.size } else { 0x1000 };
            PRIMARY_GPU.mmio_virt_base = mm_map_mmio_region(
                PhysAddr::new(PRIMARY_GPU.mmio_phys_base),
                PRIMARY_GPU.mmio_size as usize,
            ) as *mut u8;
        }

        let gpu_kind = if virtio_candidate {
            "virtio"
        } else {
            "display-class"
        };
        let virt = unsafe { PRIMARY_GPU.mmio_virt_base };
        let size = unsafe { PRIMARY_GPU.mmio_size };
        if !virt.is_null() {
            klog_info!(
                "PCI: Selected {} GPU candidate at MMIO phys=0x{:x} size=0x{:x} virt=0x{:x}",
                gpu_kind,
                bar.base,
                size,
                virt as u64
            );
            wl_currency::award_win();
        } else {
            klog_info!(
                "PCI: Selected {} GPU candidate at MMIO phys=0x{:x} size=0x{:x} (mapping failed)",
                gpu_kind,
                bar.base,
                size
            );
            wl_currency::award_loss();
        }
        klog_info!("PCI: GPU acceleration groundwork ready (MMIO mapped)");
        if unsafe { PRIMARY_GPU.mmio_virt_base.is_null() } {
            klog_info!("PCI: WARNING GPU MMIO not accessible; check paging support");
        }
        return;
    }
}

fn pci_is_virtio_gpu(info: &PciDeviceInfo) -> bool {
    info.vendor_id == PCI_VENDOR_ID_VIRTIO
        && (info.device_id == PCI_DEVICE_ID_VIRTIO_GPU
            || info.device_id == PCI_DEVICE_ID_VIRTIO_GPU_TRANS)
}

fn pci_is_gpu_candidate(info: &PciDeviceInfo) -> bool {
    if info.class_code == PCI_CLASS_DISPLAY {
        return true;
    }
    pci_is_virtio_gpu(info)
}

fn pci_collect_bars(info: &mut PciDeviceInfo) {
    info.bar_count = 0;

    let header_type = info.header_type & PCI_HEADER_TYPE_MASK;
    let max_bars = if header_type == PCI_HEADER_TYPE_DEVICE {
        6
    } else if header_type == PCI_HEADER_TYPE_BRIDGE {
        2
    } else {
        0
    };

    let mut bar_index = 0;
    while bar_index < max_bars && (info.bar_count as usize) < PCI_MAX_BARS {
        let offset = PCI_BAR0_OFFSET + (bar_index as u8 * 4);
        let raw = pci_config_read32(info.bus, info.device, info.function, offset);
        if raw == 0 {
            bar_index += 1;
            continue;
        }

        let bar = &mut info.bars[info.bar_count as usize];
        bar.size = pci_probe_bar_size(info.bus, info.device, info.function, offset, raw);

        if (raw & PCI_BAR_IO_SPACE) != 0 {
            bar.is_io = 1;
            bar.is_64bit = 0;
            bar.prefetchable = 0;
            bar.base = (raw & PCI_BAR_IO_ADDRESS_MASK) as u64;
        } else {
            let bar_type = (raw & PCI_BAR_MEM_TYPE_MASK) >> 1;
            bar.is_io = 0;
            bar.prefetchable = if (raw & PCI_BAR_MEM_PREFETCHABLE) != 0 {
                1
            } else {
                0
            };
            bar.is_64bit = if bar_type == 0x2 { 1 } else { 0 };
            let mut base = (raw & PCI_BAR_MEM_ADDRESS_MASK) as u64;
            if bar.is_64bit != 0 && bar_index + 1 < max_bars {
                let upper =
                    pci_config_read32(info.bus, info.device, info.function, offset + 4) as u64;
                base |= upper << 32;
                bar_index += 1; // skip high dword
            }
            bar.base = base;
        }

        pci_log_bar(bar, info.bar_count);
        info.bar_count += 1;
        bar_index += 1;
    }
}

fn pci_notify_drivers(info: &PciDeviceInfo) {
    unsafe {
        for i in 0..PCI_REGISTERED_DRIVER_COUNT {
            let driver_ptr = PCI_REGISTERED_DRIVERS[i];
            if driver_ptr.is_null() {
                continue;
            }
            let driver = &*driver_ptr;
            if let Some(match_fn) = driver.match_fn {
                if !match_fn(info, driver.context) {
                    continue;
                }
            } else {
                continue;
            }

            if let Some(probe) = driver.probe {
                if probe(info, driver.context) != 0 {
                    klog_debug!(
                        "PCI: Driver {} probe failed for bus {} dev {} func {}",
                        cstr_or_placeholder(driver.name),
                        info.bus,
                        info.device,
                        info.function
                    );
                }
            }
        }
    }
}

fn pci_scan_function(bus: u8, device: u8, function: u8) {
    let vendor_id = pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET);
    if vendor_id == 0xFFFF {
        return;
    }

    let device_id = pci_config_read16(bus, device, function, PCI_DEVICE_ID_OFFSET);
    let class_code = pci_config_read8(bus, device, function, PCI_CLASS_CODE_OFFSET);
    let subclass = pci_config_read8(bus, device, function, PCI_SUBCLASS_OFFSET);
    let prog_if = pci_config_read8(bus, device, function, PCI_PROG_IF_OFFSET);
    let revision = pci_config_read8(bus, device, function, PCI_REVISION_ID_OFFSET);
    let header_type = pci_config_read8(bus, device, function, PCI_HEADER_TYPE_OFFSET);
    let irq_line = pci_config_read8(bus, device, function, PCI_INTERRUPT_LINE_OFFSET);
    let irq_pin = pci_config_read8(bus, device, function, PCI_INTERRUPT_PIN_OFFSET);

    unsafe {
        if DEVICE_COUNT == PCI_MAX_DEVICES {
            klog_info!("PCI: Device buffer full, additional devices will not be tracked");
        }
        if DEVICE_COUNT >= PCI_MAX_DEVICES {
            return;
        }
    }

    let mut info = PciDeviceInfo {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        class_code,
        subclass,
        prog_if,
        revision,
        header_type,
        irq_line,
        irq_pin,
        bar_count: 0,
        bars: [PciBarInfo::default(); PCI_MAX_BARS],
    };

    pci_log_device_header(&info);
    pci_collect_bars(&mut info);
    pci_consider_gpu_candidate(&info);
    pci_notify_drivers(&info);

    unsafe {
        DEVICES[DEVICE_COUNT] = info;
        DEVICE_COUNT += 1;
    }

    if (header_type & PCI_HEADER_TYPE_MASK) == PCI_HEADER_TYPE_BRIDGE {
        let secondary_bus = pci_config_read8(bus, device, function, 0x19);
        unsafe {
            if secondary_bus != 0 && BUS_VISITED[secondary_bus as usize] == 0 {
                klog_info!("PCI: Traversing to secondary bus {}", secondary_bus);
                BUS_VISITED[secondary_bus as usize] = 1;
                for dev in 0..32u8 {
                    let sec_vendor = pci_config_read16(secondary_bus, dev, 0, PCI_VENDOR_ID_OFFSET);
                    if sec_vendor == 0xFFFF {
                        continue;
                    }
                    pci_scan_function(secondary_bus, dev, 0);
                    let sec_header =
                        pci_config_read8(secondary_bus, dev, 0, PCI_HEADER_TYPE_OFFSET);
                    if (sec_header & PCI_HEADER_TYPE_MULTI_FUNCTION) != 0 {
                        for func in 1..8u8 {
                            let sec_vendor =
                                pci_config_read16(secondary_bus, dev, func, PCI_VENDOR_ID_OFFSET);
                            if sec_vendor == 0xFFFF {
                                continue;
                            }
                            pci_scan_function(secondary_bus, dev, func);
                        }
                    }
                }
            }
        }
    }
}

fn pci_scan_device(bus: u8, device: u8) {
    let vendor_id = pci_config_read16(bus, device, 0, PCI_VENDOR_ID_OFFSET);
    if vendor_id == 0xFFFF {
        return;
    }

    pci_scan_function(bus, device, 0);
    let header_type = pci_config_read8(bus, device, 0, PCI_HEADER_TYPE_OFFSET);
    if (header_type & PCI_HEADER_TYPE_MULTI_FUNCTION) != 0 {
        for function in 1..8u8 {
            if pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET) != 0xFFFF {
                pci_scan_function(bus, device, function);
            }
        }
    }
}

fn pci_enumerate_bus(bus: u8) {
    unsafe {
        if BUS_VISITED[bus as usize] != 0 {
            return;
        }
        BUS_VISITED[bus as usize] = 1;
    }

    for device in 0..32u8 {
        pci_scan_device(bus, device);
    }
}
pub fn pci_init() -> c_int {
    unsafe {
        if PCI_INITIALIZED != 0 {
            return 0;
        }
    }

    unsafe {
        DEVICE_COUNT = 0;
        PRIMARY_GPU.present = 0;
        PRIMARY_GPU.mmio_phys_base = 0;
        PRIMARY_GPU.mmio_size = 0;
        PRIMARY_GPU.mmio_virt_base = ptr::null_mut();
        for b in BUS_VISITED.iter_mut() {
            *b = 0;
        }
    }
    klog_info!("PCI: Initializing PCI subsystem");

    pci_enumerate_bus(0);

    unsafe {
        if PRIMARY_GPU.present == 0 {
            klog_info!("PCI: No GPU-class device detected on primary bus");
        }
        klog_info!(
            "PCI: Enumeration complete. Devices discovered: {}",
            DEVICE_COUNT
        );
        PCI_INITIALIZED = 1;
    }
    0
}
pub fn pci_get_device_count() -> usize {
    unsafe { DEVICE_COUNT }
}
pub fn pci_get_devices() -> *const PciDeviceInfo {
    unsafe { DEVICES.as_ptr() }
}
pub fn pci_get_primary_gpu() -> *const PciGpuInfo {
    unsafe {
        if PRIMARY_GPU.present != 0 {
            &PRIMARY_GPU as *const PciGpuInfo
        } else {
            ptr::null()
        }
    }
}
pub fn pci_get_registered_driver_count() -> usize {
    unsafe { PCI_REGISTERED_DRIVER_COUNT }
}
pub fn pci_get_registered_driver(index: usize) -> *const PciDriver {
    unsafe {
        if index >= PCI_REGISTERED_DRIVER_COUNT {
            ptr::null()
        } else {
            PCI_REGISTERED_DRIVERS[index]
        }
    }
}
pub fn pci_register_driver(driver: *const PciDriver) -> c_int {
    if driver.is_null() {
        klog_info!("PCI: Attempted to register invalid driver");
        return -1;
    }

    unsafe {
        if PCI_REGISTERED_DRIVER_COUNT >= PCI_DRIVER_MAX {
            klog_info!("PCI: Driver registration queue is full");
            return -1;
        }
        PCI_REGISTERED_DRIVERS[PCI_REGISTERED_DRIVER_COUNT] = driver;
        PCI_REGISTERED_DRIVER_COUNT += 1;
        klog_debug!(
            "PCI: Registered driver {}",
            cstr_or_placeholder((*driver).name)
        );
    }
    0
}
