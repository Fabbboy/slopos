//! PCI bind glue for the Intel LPSS / DesignWare I²C controllers.
//!
//! The LPSS I²C controllers live at PCI device `0x15` (functions 0–3) and
//! `0x19`, class `0x0c80` ("serial bus, other"). Those slots are matched
//! specifically rather than all of class `0x0c80`, which also covers the
//! PCH SPI-flash controller (a different register layout that must not be
//! poked). [`DesignWareI2c::init`] re-verifies the `IC_COMP_TYPE` magic.

use slopos_abi::addr::PhysAddr;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::KArc;
use slopos_ostd::{klog_info, klog_warn};

use super::designware::{DesignWareI2c, I2cError};
use super::{I2cBus, register_bus};
use crate::driver_core::bound::BoundDevice;
use crate::hpet;
use crate::pci::{
    PciProbeError, ProbeOutcome, pci_alloc_mmio, pci_config_read16, pci_config_read32,
    pci_config_write16, pci_config_write32, pci_find_capability,
};
use crate::pci_defs::PciDeviceInfo;
use crate::pci_defs::{PCI_BAR0_OFFSET, PCI_COMMAND_MEMORY_SPACE, PCI_COMMAND_OFFSET};

/// PCI Power Management capability ID.
const PCI_CAP_ID_PM: u8 = 0x01;
/// PMCSR (power management control/status) offset within the PM capability.
const PCI_PM_CTRL: u16 = 0x04;
/// Power-state field within PMCSR (`0` = D0, `3` = D3hot).
const PCI_PM_STATE_MASK: u16 = 0x03;

/// Transition the function to D0 (full power). LPSS controllers can come up
/// in D3hot, where their MMIO BARs do not decode, so this must run before
/// any MMIO access. Clears the PMCSR power-state field and waits the
/// mandatory 10 ms D3hot→D0 settle.
fn set_power_d0(info: &PciDeviceInfo) {
    let Some(pm_cap) = pci_find_capability(info.bus, info.device, info.function, PCI_CAP_ID_PM)
    else {
        return; // no PM capability → the function is always in D0
    };
    let pmcsr = pci_config_read16(info.bus, info.device, info.function, pm_cap + PCI_PM_CTRL);
    if pmcsr & PCI_PM_STATE_MASK != 0 {
        pci_config_write16(
            info.bus,
            info.device,
            info.function,
            pm_cap + PCI_PM_CTRL,
            pmcsr & !PCI_PM_STATE_MASK,
        );
        hpet::delay_ms(10);
    }
}

/// Program a (possibly 64-bit) MMIO BAR0 with `base`, preserving the BAR's
/// read-only low type bits.
fn program_bar0(info: &PciDeviceInfo, base: u64, is_64bit: bool) {
    let orig_lo = pci_config_read32(info.bus, info.device, info.function, PCI_BAR0_OFFSET);
    let lo = ((base & 0xffff_ffff) as u32 & 0xffff_fff0) | (orig_lo & 0x0f);
    pci_config_write32(info.bus, info.device, info.function, PCI_BAR0_OFFSET, lo);
    if is_64bit {
        pci_config_write32(
            info.bus,
            info.device,
            info.function,
            PCI_BAR0_OFFSET + 4,
            (base >> 32) as u32,
        );
    }
}

const PCI_VENDOR_INTEL: u16 = 0x8086;
const PCI_CLASS_SERIAL_BUS: u8 = 0x0c;
const PCI_SUBCLASS_SERIAL_OTHER: u8 = 0x80;
/// Intel client-PCH PCI device numbers hosting the LPSS I²C functions.
const LPSS_I2C_PCI_DEVICES: [u8; 2] = [0x15, 0x19];

fn lpss_i2c_matches(info: &PciDeviceInfo) -> bool {
    if super::lpss_disabled() {
        return false;
    }
    info.vendor_id == PCI_VENDOR_INTEL
        && info.class_code == PCI_CLASS_SERIAL_BUS
        && info.subclass == PCI_SUBCLASS_SERIAL_OTHER
        && LPSS_I2C_PCI_DEVICES.contains(&info.device)
}

fn lpss_i2c_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    let bar = info.bars[0];
    if bar.is_io != 0 || bar.size == 0 {
        return Err(PciProbeError::Unsupported);
    }

    // Power the function up to D0 BEFORE touching its BAR — LPSS boots in
    // D3hot and won't decode MMIO otherwise.
    set_power_d0(&info);

    // Firmware may leave the LPSS BAR unassigned (base == 0); assign a free
    // MMIO region and program the BAR.
    let base = if bar.base != 0 {
        bar.base
    } else {
        let assigned = pci_alloc_mmio(bar.size).ok_or(PciProbeError::Unsupported)?;
        program_bar0(&info, assigned, bar.is_64bit != 0);
        klog_info!(
            "i2c-lpss: assigned BAR0 {:#x} (size {:#x}) at {:02x}:{:02x}.{}",
            assigned,
            bar.size,
            info.bus,
            info.device,
            info.function
        );
        assigned
    };

    let cmd = pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let new_cmd = cmd | PCI_COMMAND_MEMORY_SPACE;
    if new_cmd != cmd {
        pci_config_write16(
            info.bus,
            info.device,
            info.function,
            PCI_COMMAND_OFFSET,
            new_cmd,
        );
    }

    let phys = PhysAddr::new(base);
    let mmio = MmioRegion::map(phys, bar.size as usize).ok_or(PciProbeError::DeviceFault)?;

    let mut ctrl = DesignWareI2c::new(mmio);
    ctrl.lpss_bringup(base);
    match ctrl.init() {
        Ok(()) => {}
        Err(I2cError::NotDesignWare) => {
            // Matched the slot but the core isn't a DesignWare I²C — leave
            // it alone and let other drivers have a crack.
            return Err(PciProbeError::Unsupported);
        }
        Err(e) => {
            klog_warn!(
                "i2c-lpss: init failed at {:02x}:{:02x}.{}: {:?}",
                info.bus,
                info.device,
                info.function,
                e
            );
            return Err(PciProbeError::DeviceFault);
        }
    }

    let bus = KArc::try_new(I2cBus::new(info.bus, info.device, info.function, ctrl))
        .map_err(|_| PciProbeError::OutOfMemory)?;
    register_bus(bus);

    klog_info!(
        "i2c-lpss: DesignWare I²C bound at {:02x}:{:02x}.{} ({:04x}:{:04x})",
        info.bus,
        info.device,
        info.function,
        info.vendor_id,
        info.device_id
    );
    Ok(ProbeOutcome::Bound)
}

crate::pci_driver! {
    pub static LPSS_I2C_DRIVER = {
        name: "i2c-lpss-designware",
        // The match needs a cmdline gate and a device-slot list that no
        // declarative rule can express, so the whole predicate lives in the
        // imperative fallback.
        match_table: &[],
        fallback: Some(lpss_i2c_matches),
        probe: lpss_i2c_probe,
    };
}
