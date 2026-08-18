//! Non-PCI **platform (ACPI) device-driver bus**, mirroring `pci.rs`: a
//! link-section driver registry (the [`platform_driver!`] macro), an
//! ACPI-namespace enumerator that finds each registered driver's device by
//! `_HID`/`_CID` and decodes its `_CRS` resources, and a priority-ordered
//! matchmaker that binds exactly one driver per device with devres-managed
//! resource ownership.
//!
//! [`matchmake`] is decoupled from the live ACPI enumeration and the global
//! claim table so it is exercisable over synthetic devices + drivers in-QEMU
//! (see `tests/platform_binding.rs`).

use slopos_acpi::aml::{self, AcpiPlatformDevice, HhdmHost};
use slopos_acpi::fadt::Fadt;
use slopos_acpi::tables::AcpiTables;
use slopos_ostd::dev::Devres;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{AllocError, KVec, klog_info, klog_warn};

use crate::driver_core::platform_bound::BoundPlatformDevice;
pub use crate::pci::ProbeOutcome;

/// Maximum I/O-port windows recorded per platform device (the keyboard uses 2).
pub const MAX_PLATFORM_IO: usize = 4;
pub const MAX_PLATFORM_DEVICES: usize = 32;

/// One I/O-port window from a device's `_CRS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlatformIoWindow {
    pub base: u16,
    pub len: u8,
}

/// A device's legacy interrupt line from its `_CRS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformIrq {
    pub line: u8,
    pub edge: bool,
    pub active_low: bool,
}

/// Enumeration record for one discovered ACPI platform device.
///
/// `Copy` so a probe snapshots it freely.
#[derive(Clone, Copy)]
pub struct PlatformDeviceInfo {
    /// The `_HID`/`_CID` id this device matched (a `'static` driver id).
    pub matched_id: &'static [u8],
    /// `_CRS` I/O-port windows (`io[..io_count]` are valid).
    pub io: [PlatformIoWindow; MAX_PLATFORM_IO],
    pub io_count: u8,
    /// First `_CRS` IRQ, if any.
    pub irq: Option<PlatformIrq>,
    /// `_STA` presence (`None` if unevaluable / EC-gated — not "absent").
    pub present: Option<bool>,
}

impl PlatformDeviceInfo {
    pub fn io_ports(&self) -> &[PlatformIoWindow] {
        &self.io[..self.io_count as usize]
    }

    pub fn has_io_port(&self, port: u16) -> bool {
        self.io_ports().iter().any(|w| {
            let end = w.base as u32 + (w.len.max(1)) as u32;
            (port as u32) >= w.base as u32 && (port as u32) < end
        })
    }
}

/// A declarative match rule: the device's `_HID` or `_CID` equals this id.
#[derive(Clone, Copy)]
pub enum PlatformMatch {
    /// Match an EISA/PNP id like `b"PNP0303"`.
    HidCid(&'static [u8]),
}

impl PlatformMatch {
    pub fn matches(&self, dev: &PlatformDeviceInfo) -> bool {
        match self {
            PlatformMatch::HidCid(id) => *id == dev.matched_id,
        }
    }

    pub fn id(&self) -> &'static [u8] {
        match self {
            PlatformMatch::HidCid(id) => id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformProbeError {
    /// Matched, but post-inspection rules rejected it.
    Mismatch,
    OutOfMemory,
    DeviceFault,
    Unsupported,
}

/// Static, link-section-resident platform-driver descriptor.
#[repr(C)]
pub struct PlatformDriverEntry {
    pub name: &'static str,
    /// Declarative match rules; the driver matches when any rule matches.
    pub match_table: &'static [PlatformMatch],
    /// Bind order, ascending (lower binds first). Default 128 via the macro.
    pub priority: u8,
    /// Consulted when ACPI enumeration finds no device for any of
    /// `match_table`'s ids, to synthesize one from fixed/architectural
    /// resources. The argument is whether the FADT advertises an 8042
    /// (`IAPC_BOOT_ARCH` bit 1); `None` binds ACPI-discovered devices only.
    pub fallback: Option<fn(bool) -> Option<PlatformDeviceInfo>>,
    /// Probe the matched device, acquiring resources through the capability.
    pub probe: fn(&mut BoundPlatformDevice<'_>) -> Result<ProbeOutcome, PlatformProbeError>,
}

impl PlatformDriverEntry {
    fn entry_matches(&self, dev: &PlatformDeviceInfo) -> bool {
        self.match_table.iter().any(|m| m.matches(dev))
    }
}

impl slopos_ostd::ffi::registry::RegistryEntry for PlatformDriverEntry {
    const REGISTRIES: &'static [slopos_ostd::ffi::registry::RegistryId] =
        &[slopos_ostd::ffi::registry::RegistryId::PlatformDrivers];
}

pub fn driver_registry_iter() -> impl Iterator<Item = &'static PlatformDriverEntry> {
    slopos_ostd::ffi::registry::registry_slice::<PlatformDriverEntry>(
        slopos_ostd::ffi::registry::RegistryId::PlatformDrivers,
    )
    .iter()
}

/// Pick a supplied optional macro field or fall back to its default.
#[macro_export]
#[doc(hidden)]
macro_rules! __platform_driver_opt {
    (, $default:expr) => {
        $default
    };
    ($val:expr, $default:expr) => {
        $val
    };
}

/// Emit a [`PlatformDriverEntry`] into the `.platform_driver_registry` link
/// section. `match_table` is required; `priority` defaults to 128.
///
/// ```ignore
/// platform_driver! {
///     pub static I8042_KBD = {
///         name: "i8042-kbd",
///         match_table: &[PlatformMatch::HidCid(b"PNP0303")],
///         probe: i8042_probe,
///     };
/// }
/// ```
#[macro_export]
macro_rules! platform_driver {
    (
        $(#[$attr:meta])*
        $vis:vis static $name:ident = {
            name: $drv_name:expr,
            match_table: $match_table:expr,
            $(priority: $priority:expr,)?
            $(fallback: $fallback:expr,)?
            probe: $probe:path $(,)?
        };
    ) => {
        slopos_ostd::registry_entry! {
            platform_drivers,
            $(#[$attr])*
            $vis static $name: $crate::platform_bus::PlatformDriverEntry =
                $crate::platform_bus::PlatformDriverEntry {
                    name: $drv_name,
                    match_table: $match_table,
                    priority: $crate::__platform_driver_opt!($($priority)?, 128),
                    fallback: $crate::__platform_driver_opt!($($fallback)?, None),
                    probe: $probe,
                };
        }
    };
}

fn build_info(matched_id: &'static [u8], dev: &AcpiPlatformDevice) -> PlatformDeviceInfo {
    let mut io = [PlatformIoWindow::default(); MAX_PLATFORM_IO];
    let mut io_count = 0u8;
    for w in dev.io_ports.iter() {
        if (io_count as usize) < MAX_PLATFORM_IO {
            io[io_count as usize] = PlatformIoWindow {
                base: w.base,
                len: w.len,
            };
            io_count += 1;
        }
    }
    let irq = dev.irqs.iter().find_map(|r| {
        r.first_line().map(|line| PlatformIrq {
            line,
            edge: r.edge,
            active_low: r.active_low,
        })
    });
    PlatformDeviceInfo {
        matched_id,
        io,
        io_count,
        irq,
        present: dev.present,
    }
}

/// Discover the ACPI device for each registered driver's match ids, deduped by
/// matched id. A driver that matches no ACPI device but supplies a `fallback`
/// gets a synthesized record instead.
fn enumerate(tables: &AcpiTables, debug: bool) -> Result<KVec<PlatformDeviceInfo>, AllocError> {
    let host = HhdmHost;
    let has_8042 = tables
        .find_table(b"FACP")
        .and_then(|f| Fadt::parse(f.raw()))
        .map(|f| f.has_8042())
        .unwrap_or(false);
    let mut devices: KVec<PlatformDeviceInfo> = KVec::new();
    for drv in driver_registry_iter() {
        let mut matched = false;
        for m in drv.match_table {
            let id = m.id();
            // Already discovered for an earlier driver; the matchmaker offers
            // one record to both, so skip the redundant namespace walk.
            if devices.iter().any(|d| d.matched_id == id) {
                matched = true;
                continue;
            }
            if let Some(found) = aml::find_acpi_platform_device(tables, &host, &[id], debug) {
                devices.push(build_info(id, &found))?;
                matched = true;
            }
        }
        if !matched {
            if let Some(fb) = drv.fallback {
                if let Some(dev) = fb(has_8042) {
                    if !devices.iter().any(|d| d.matched_id == dev.matched_id) {
                        devices.push(dev)?;
                    }
                }
            }
        }
    }
    Ok(devices)
}

/// Whether the FADT advertises an i8042 controller (IAPC_BOOT_ARCH bit 1).
pub fn acpi_has_8042(rsdp_phys: u64) -> bool {
    let Some(tables) = AcpiTables::from_phys(rsdp_phys) else {
        return false;
    };
    let Some(facp) = tables.find_table(b"FACP") else {
        return false;
    };
    Fadt::parse(facp.raw())
        .map(|f| f.has_8042())
        .unwrap_or(false)
}

enum ClaimSlot {
    Unclaimed,
    Claimed {
        #[allow(dead_code)]
        name: &'static str,
        // Held for its `Drop`: keeps the device's resources alive for the
        // binding's lifetime.
        #[allow(dead_code)]
        devres: Devres,
    },
}

struct ClaimTable {
    slots: [ClaimSlot; MAX_PLATFORM_DEVICES],
}

impl ClaimTable {
    const fn new() -> Self {
        Self {
            slots: [const { ClaimSlot::Unclaimed }; MAX_PLATFORM_DEVICES],
        }
    }

    fn is_claimed(&self, dev_idx: usize) -> bool {
        matches!(self.slots.get(dev_idx), Some(ClaimSlot::Claimed { .. }))
    }

    fn claim(&mut self, dev_idx: usize, name: &'static str, devres: Devres) {
        if dev_idx < self.slots.len() {
            self.slots[dev_idx] = ClaimSlot::Claimed { name, devres };
        }
    }
}

static CLAIMED_BY: SpinLock<ClaimTable> = SpinLock::new(
    ClaimTable::new(),
    lock_class!("platform.CLAIMED_BY", LOCK_LEVEL_RESOURCE),
);

/// Abstracts the live `CLAIMED_BY` static (boot) from a heap-backed sink (tests).
pub(crate) trait PlatformClaimSink {
    fn is_claimed(&self, dev_idx: usize) -> bool;
    fn record(&self, dev_idx: usize, name: &'static str, devres: Devres);
}

struct GlobalClaims;

impl PlatformClaimSink for GlobalClaims {
    fn is_claimed(&self, dev_idx: usize) -> bool {
        CLAIMED_BY.lock().is_claimed(dev_idx)
    }
    fn record(&self, dev_idx: usize, name: &'static str, devres: Devres) {
        CLAIMED_BY.lock().claim(dev_idx, name, devres);
    }
}

/// Offer each device to its candidate drivers in priority order, binding the
/// first that returns `Bound`. Probe runs with no lock held; the resource bag
/// drops on `Declined`/`Err`, or moves into the claim slot on `Bound`.
pub(crate) fn matchmake(
    devices: &[PlatformDeviceInfo],
    drivers: &[&'static PlatformDriverEntry],
    claims: &dyn PlatformClaimSink,
) -> Result<(), AllocError> {
    let mut cands: KVec<usize> = KVec::new();
    for (dev_idx, dev) in devices.iter().enumerate() {
        if claims.is_claimed(dev_idx) {
            continue;
        }
        cands.clear();
        for (di, drv) in drivers.iter().enumerate() {
            if drv.entry_matches(dev) {
                cands.push(di)?;
            }
        }
        cands.sort_unstable_by(|&a, &b| {
            drivers[a]
                .priority
                .cmp(&drivers[b].priority)
                .then(a.cmp(&b))
        });
        for k in 0..cands.len() {
            let drv = drivers[cands[k]];
            let mut devres = Devres::new();
            let mut bound = BoundPlatformDevice::new(dev, &mut devres);
            match (drv.probe)(&mut bound) {
                Ok(ProbeOutcome::Bound) => {
                    drop(bound);
                    claims.record(dev_idx, drv.name, devres);
                    break;
                }
                Ok(ProbeOutcome::Declined) => continue,
                Err(e) => {
                    klog_warn!(
                        "platform: {} declined device {}: {:?}",
                        drv.name,
                        dev_idx,
                        e
                    );
                    continue;
                }
            }
        }
    }
    Ok(())
}

/// Runs once at boot on the BSP, after `pci_probe_drivers`.
pub fn probe_drivers(rsdp_phys: u64, debug: bool) {
    let Some(tables) = AcpiTables::from_phys(rsdp_phys) else {
        klog_warn!("platform: ACPI tables unavailable; skipping platform-device probe");
        return;
    };

    let devices = match enumerate(&tables, debug) {
        Ok(d) => d,
        Err(_) => {
            klog_warn!("platform: enumeration out of memory");
            return;
        }
    };

    for (i, d) in devices.iter().enumerate() {
        klog_info!(
            "platform: device #{} id={} io_windows={} irq={:?} present={:?}",
            i,
            core::str::from_utf8(d.matched_id).unwrap_or("?"),
            d.io_count,
            d.irq.map(|q| q.line),
            d.present,
        );
    }

    let mut drivers: KVec<&'static PlatformDriverEntry> = KVec::new();
    for e in driver_registry_iter() {
        if drivers.push(e).is_err() {
            klog_warn!("platform: driver list out of memory");
            return;
        }
    }

    klog_info!(
        "platform: {} driver(s), {} device(s) discovered",
        drivers.len(),
        devices.len()
    );

    if matchmake(devices.as_slice(), drivers.as_slice(), &GlobalClaims).is_err() {
        klog_warn!("platform: matchmake out of memory");
    }
}
