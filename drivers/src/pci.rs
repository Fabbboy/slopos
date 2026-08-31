use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::PhysAddr;
use slopos_acpi::mcfg::{Mcfg, McfgEntry};
use slopos_acpi::tables::AcpiTables;
use slopos_kernel_services::platform;
use slopos_mm::hhdm;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::dev::Devres;
use slopos_ostd::pci::{Bdf, EcamConfigSpace};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, OnceLock, SpinLock};
use slopos_ostd::{AllocError, KBTreeMap, KVec, Pod, klog_info, klog_warn};

use crate::driver_core::bus::{
    Binding, Bus, ClaimSink, ClaimTable, DriverIndex, ProbeError, probe_bus, push_unique,
    sort_candidates,
};

/// The PCI [`BoundDevice`](crate::driver_core::bus::BoundDevice) instance.
pub type BoundDevice<'d> = crate::driver_core::bus::BoundDevice<'d, PciBus>;

/// Reason a PCI probe rejected a candidate device. The variant set is shared
/// across buses; the alias is what keeps a PCI driver's signatures reading in
/// its own vocabulary.
pub type PciProbeError = ProbeError;

pub use crate::driver_core::bus::ProbeOutcome;

pub use crate::pci_defs::*;

const PCI_SECONDARY_BUS_OFFSET: u16 = 0x19;

/// One declarative match rule in a driver's `match_table`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciMatch {
    VendorDevice { vendor: u16, device: u16 },
    VendorClass { vendor: u16, class: u8 },
    ClassSubclass { class: u8, subclass: u8 },
    ClassOnly { class: u8 },
}

impl PciMatch {
    pub const fn matches(&self, d: &PciDeviceInfo) -> bool {
        match *self {
            PciMatch::VendorDevice { vendor, device } => {
                d.vendor_id == vendor && d.device_id == device
            }
            PciMatch::VendorClass { vendor, class } => {
                d.vendor_id == vendor && d.class_code == class
            }
            PciMatch::ClassSubclass { class, subclass } => {
                d.class_code == class && d.subclass == subclass
            }
            PciMatch::ClassOnly { class } => d.class_code == class,
        }
    }

    const fn vd_key(&self) -> Option<u32> {
        match *self {
            PciMatch::VendorDevice { vendor, device } => {
                Some(((vendor as u32) << 16) | device as u32)
            }
            _ => None,
        }
    }

    /// The index only narrows candidates; [`PciMatch::matches`] still verifies
    /// the full predicate.
    const fn cs_key(&self) -> Option<u16> {
        match *self {
            PciMatch::VendorClass { class, .. }
            | PciMatch::ClassSubclass { class, .. }
            | PciMatch::ClassOnly { class } => Some(class as u16),
            PciMatch::VendorDevice { .. } => None,
        }
    }
}

/// Every field is `'static`-constructible so the struct fits into a `static`
/// placed in the `.driver_registry` link section by [`crate::pci_driver!`].
#[repr(C)]
pub struct PciDriverEntry {
    pub name: &'static str,
    /// The driver matches when **any** rule matches. Indexed at boot for O(1)
    /// candidate lookup.
    pub match_table: &'static [PciMatch],
    /// Imperative escape hatch for predicates a `match_table` cannot express:
    /// a driver matches when the table matches **or** the fallback returns
    /// `true`.
    pub fallback: Option<fn(&PciDeviceInfo) -> bool>,
    /// Bind order, ascending: a lower value binds first, so a specific driver
    /// can beat a generic one for the same device.
    pub priority: u8,
    /// The driver acquires every resource through the [`BoundDevice`]
    /// capability, so a failed probe releases them automatically. `Deferred` is
    /// retried once.
    pub probe: fn(&mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError>,
}

impl PciDriverEntry {
    fn entry_matches(&self, dev: &PciDeviceInfo) -> bool {
        self.match_table.iter().any(|m| m.matches(dev)) || self.fallback.map_or(false, |f| f(dev))
    }
}

/// The PCI bus's device/driver model.
pub struct PciBus;

impl Bus for PciBus {
    type Device = PciDeviceInfo;
    type DriverEntry = PciDriverEntry;

    const NAME: &'static str = "PCI";

    fn entry_name(entry: &PciDriverEntry) -> &'static str {
        entry.name
    }

    fn priority(entry: &PciDriverEntry) -> u8 {
        entry.priority
    }

    fn matches(entry: &PciDriverEntry, dev: &PciDeviceInfo) -> bool {
        entry.entry_matches(dev)
    }

    fn probe(
        entry: &PciDriverEntry,
        bound: &mut BoundDevice<'_>,
    ) -> Result<ProbeOutcome, ProbeError> {
        (entry.probe)(bound)
    }
}

struct PciEnumState {
    bus_visited: [u8; PCI_MAX_BUSES],
    devices: [PciDeviceInfo; PCI_MAX_DEVICES],
    device_count: usize,
}

impl PciEnumState {
    const fn new() -> Self {
        Self {
            bus_visited: [0; PCI_MAX_BUSES],
            devices: [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES],
            device_count: 0,
        }
    }
}

static PCI_INIT: InitFlag = InitFlag::new();
static ENUM_STATE: SpinLock<PciEnumState> = SpinLock::new(
    PciEnumState::new(),
    lock_class!("ENUM_STATE", LOCK_LEVEL_REGISTRY),
);
static DEVICE_COUNT_CACHE: AtomicUsize = AtomicUsize::new(0);

/// Anchored at enumeration to the top of the highest firmware-assigned MMIO
/// BAR; `0` = no anchor available.
static MMIO_ALLOC_CURSOR: AtomicU64 = AtomicU64::new(0);

/// Allocate a free physical MMIO region of `size` bytes (size-aligned) for
/// a PCI BAR the firmware left unassigned (`base == 0`).
///
/// The region is placed just above the highest MMIO BAR the firmware *did*
/// assign, so it lands inside the same host-bridge MMIO aperture and the device
/// decodes it. Returns `None` if no anchor exists.
pub fn pci_alloc_mmio(size: u64) -> Option<u64> {
    if size == 0 {
        return None;
    }
    let align = size.max(0x1000);
    loop {
        let cur = MMIO_ALLOC_CURSOR.load(Ordering::Acquire);
        if cur == 0 {
            return None;
        }
        let base = (cur.checked_add(align - 1)?) & !(align - 1);
        let next = base.checked_add(size)?;
        if MMIO_ALLOC_CURSOR
            .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(base);
        }
    }
}

impl slopos_ostd::ffi::registry::RegistryEntry for PciDriverEntry {
    const REGISTRIES: &'static [slopos_ostd::ffi::registry::RegistryId] =
        &[slopos_ostd::ffi::registry::RegistryId::PciDrivers];
}

/// Borrow the linker-built `[PciDriverEntry]` slice.
pub fn driver_registry_iter() -> impl Iterator<Item = &'static PciDriverEntry> {
    slopos_ostd::ffi::registry::registry_slice::<PciDriverEntry>(
        slopos_ostd::ffi::registry::RegistryId::PciDrivers,
    )
    .iter()
}

#[macro_export]
#[doc(hidden)]
macro_rules! __pci_driver_opt {
    (, $default:expr) => {
        $default
    };
    ($val:expr, $default:expr) => {
        $val
    };
}

/// Declarative wrapper around `registry_entry!` for emitting a
/// [`PciDriverEntry`] into the `.driver_registry` link section.
///
/// `match_table` is required; `fallback` (default `None`) and `priority`
/// (default `128` — lower binds first) are optional:
///
/// ```ignore
/// pci_driver! {
///     pub static FOO_DRIVER = {
///         name: "foo",
///         match_table: &[PciMatch::VendorDevice { vendor: 0x1af4, device: 0x1042 }],
///         probe: foo_probe,
///     };
/// }
/// ```
#[macro_export]
macro_rules! pci_driver {
    (
        $(#[$attr:meta])*
        $vis:vis static $name:ident = {
            name: $drv_name:expr,
            match_table: $match_table:expr,
            $(fallback: $fallback:expr,)?
            $(priority: $priority:expr,)?
            probe: $probe:path $(,)?
        };
    ) => {
        slopos_ostd::registry_entry! {
            pci_drivers,
            $(#[$attr])*
            $vis static $name: $crate::pci::PciDriverEntry = $crate::pci::PciDriverEntry {
                name: $drv_name,
                match_table: $match_table,
                fallback: $crate::__pci_driver_opt!($($fallback)?, None),
                priority: $crate::__pci_driver_opt!($($priority)?, 128),
                probe: $probe,
            };
        }
    };
}

/// Cached ECAM segments, built once during `pci_discover_mcfg()`. The primary
/// segment (segment 0) is laid out first so the hot path skips an array search.
struct EcamRegistry {
    primary: EcamConfigSpace,
    primary_entry: McfgEntry,
    extras: KVec<EcamConfigSpace>,
    extra_entries: KVec<McfgEntry>,
}

impl EcamRegistry {
    fn find(&self, bdf: Bdf) -> Option<&EcamConfigSpace> {
        if self.primary.contains(bdf) {
            return Some(&self.primary);
        }
        self.extras.iter().find(|cs| cs.contains(bdf))
    }

    fn entry_count(&self) -> usize {
        1 + self.extras.len()
    }

    fn entry(&self, idx: usize) -> Option<McfgEntry> {
        if idx == 0 {
            Some(self.primary_entry)
        } else {
            self.extra_entries.get(idx - 1).copied()
        }
    }

    fn config_space(&self, idx: usize) -> Option<&EcamConfigSpace> {
        if idx == 0 {
            Some(&self.primary)
        } else {
            self.extras.get(idx - 1)
        }
    }
}

static ECAM: OnceLock<EcamRegistry> = OnceLock::new();

const ECAM_FUNCTION_SIZE: u16 = 4096;

fn ecam_for(bus: u8, device: u8, function: u8) -> Option<(&'static EcamConfigSpace, Bdf)> {
    let bdf = Bdf::new(bus, device, function)?;
    let space = ECAM.get()?.find(bdf)?;
    Some((space, bdf))
}

fn validate_offset(offset: u16, access_size: u16) -> Option<()> {
    if offset.checked_add(access_size)? > ECAM_FUNCTION_SIZE {
        None
    } else {
        Some(())
    }
}

fn ecam_read<T: Pod>(bus: u8, device: u8, function: u8, offset: u16) -> Option<T> {
    validate_offset(offset, core::mem::size_of::<T>() as u16)?;
    let (space, bdf) = ecam_for(bus, device, function)?;
    space.read::<T>(bdf, offset)
}

fn ecam_write<T: Pod>(bus: u8, device: u8, function: u8, offset: u16, value: T) -> Option<()> {
    validate_offset(offset, core::mem::size_of::<T>() as u16)?;
    let (space, bdf) = ecam_for(bus, device, function)?;
    space.write::<T>(bdf, offset, value)
}

/// Covers the full 4096-byte PCIe extended config space (0x000–0xFFC). `None`
/// if ECAM is unavailable, the BDF is out of range, or the offset is misaligned
/// or out of bounds — as for every `pci_ecam_*` accessor.
pub fn pci_ecam_read32(bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
    if offset & 0x3 != 0 {
        return None;
    }
    ecam_read::<u32>(bus, device, function, offset)
}

pub fn pci_ecam_read16(bus: u8, device: u8, function: u8, offset: u16) -> Option<u16> {
    if offset & 0x1 != 0 {
        return None;
    }
    ecam_read::<u16>(bus, device, function, offset)
}

pub fn pci_ecam_read8(bus: u8, device: u8, function: u8, offset: u16) -> Option<u8> {
    ecam_read::<u8>(bus, device, function, offset)
}

pub fn pci_ecam_write32(bus: u8, device: u8, function: u8, offset: u16, value: u32) -> Option<()> {
    if offset & 0x3 != 0 {
        return None;
    }
    ecam_write::<u32>(bus, device, function, offset, value)
}

pub fn pci_ecam_write16(bus: u8, device: u8, function: u8, offset: u16, value: u16) -> Option<()> {
    if offset & 0x1 != 0 {
        return None;
    }
    ecam_write::<u16>(bus, device, function, offset, value)
}

pub fn pci_ecam_write8(bus: u8, device: u8, function: u8, offset: u16, value: u8) -> Option<()> {
    ecam_write::<u8>(bus, device, function, offset, value)
}

/// Panics if the ECAM access fails (misaligned or out-of-range offset), as does
/// every `pci_config_*` wrapper; use the `pci_ecam_*` variants for fallible
/// access.
#[inline]
pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    pci_ecam_read32(bus, device, function, offset).expect("pci_config_read32: ECAM read failed")
}

#[inline]
pub fn pci_config_read16(bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    pci_ecam_read16(bus, device, function, offset).expect("pci_config_read16: ECAM read failed")
}

#[inline]
pub fn pci_config_read8(bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    pci_ecam_read8(bus, device, function, offset).expect("pci_config_read8: ECAM read failed")
}

#[inline]
pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u16, value: u32) {
    pci_ecam_write32(bus, device, function, offset, value)
        .expect("pci_config_write32: ECAM write failed");
}

#[inline]
pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u16, value: u16) {
    pci_ecam_write16(bus, device, function, offset, value)
        .expect("pci_config_write16: ECAM write failed");
}

#[inline]
pub fn pci_config_write8(bus: u8, device: u8, function: u8, offset: u16, value: u8) {
    pci_ecam_write8(bus, device, function, offset, value)
        .expect("pci_config_write8: ECAM write failed");
}

/// Iterator over PCI capabilities: walks the linked list from the Capabilities
/// Pointer (offset 0x34), giving up after [`Self::MAX_CAPS`] entries so a
/// malformed list on buggy hardware cannot loop forever.
pub struct PciCapabilityIter {
    bus: u8,
    device: u8,
    function: u8,
    next_ptr: u16,
    remaining: u8,
}

impl PciCapabilityIter {
    /// The 256-byte config space fits at most ~60 entries; 48 is a generous
    /// bound.
    const MAX_CAPS: u8 = 48;

    /// Empty if the device's Status register does not advertise a capabilities
    /// list (bit 4).
    pub fn new(bus: u8, device: u8, function: u8) -> Self {
        let status = pci_config_read16(bus, device, function, PCI_STATUS_OFFSET);
        let first_ptr = if (status & PCI_STATUS_CAP_LIST) != 0 {
            // PCI spec: bottom 2 bits of the Capabilities Pointer are reserved.
            (pci_config_read8(bus, device, function, PCI_CAP_PTR_OFFSET) & 0xFC) as u16
        } else {
            0
        };

        Self {
            bus,
            device,
            function,
            next_ptr: first_ptr,
            remaining: Self::MAX_CAPS,
        }
    }

    pub fn for_device(info: &PciDeviceInfo) -> Self {
        Self::new(info.bus, info.device, info.function)
    }
}

impl Iterator for PciCapabilityIter {
    type Item = PciCapability;

    fn next(&mut self) -> Option<PciCapability> {
        if self.next_ptr == 0 || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let offset = self.next_ptr;
        let id = pci_config_read8(self.bus, self.device, self.function, offset);
        // PCI spec: bottom 2 bits of the Next Pointer are reserved.
        let next =
            (pci_config_read8(self.bus, self.device, self.function, offset + 1) & 0xFC) as u16;

        self.next_ptr = next;
        Some(PciCapability { offset, id })
    }
}

/// Iterator over PCIe extended capabilities: offsets 0x100–0xFFF, reachable
/// only via ECAM MMIO. Each header is a 32-bit DWORD:
///
/// ```text
///   bits [15:0]  — Capability ID (16-bit)
///   bits [19:16] — Capability Version (4-bit)
///   bits [31:20] — Next Capability Offset (12-bit, 0 = end of list)
/// ```
///
/// A header of `0x0000_0000` or `0xFFFF_FFFF` at 0x100 means the device has
/// none. Traversal gives up after [`Self::MAX_EXT_CAPS`] entries.
pub struct PciExtCapabilityIter {
    bus: u8,
    device: u8,
    function: u8,
    next_offset: u16,
    remaining: u8,
}

impl PciExtCapabilityIter {
    /// Matches the standard-capability guard in [`PciCapabilityIter`].
    const MAX_EXT_CAPS: u8 = 48;

    /// Empty if ECAM MMIO is inactive or the first extended header is absent.
    pub fn new(bus: u8, device: u8, function: u8) -> Self {
        let first_offset = if pci_ecam_available() {
            match pci_ecam_read32(bus, device, function, PCI_EXT_CAP_START) {
                Some(0x0000_0000) | Some(0xFFFF_FFFF) | None => 0,
                Some(_) => PCI_EXT_CAP_START,
            }
        } else {
            0
        };

        Self {
            bus,
            device,
            function,
            next_offset: first_offset,
            remaining: Self::MAX_EXT_CAPS,
        }
    }

    pub fn for_device(info: &PciDeviceInfo) -> Self {
        Self::new(info.bus, info.device, info.function)
    }
}

impl Iterator for PciExtCapabilityIter {
    type Item = PciExtCapability;

    fn next(&mut self) -> Option<PciExtCapability> {
        if self.next_offset < PCI_EXT_CAP_START || self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let offset = self.next_offset;
        let header = pci_ecam_read32(self.bus, self.device, self.function, offset)?;

        if header == 0 || header == 0xFFFF_FFFF {
            self.next_offset = 0;
            return None;
        }

        let id = (header & 0xFFFF) as u16;
        let version = ((header >> 16) & 0xF) as u8;
        let next = ((header >> 20) & 0xFFF) as u16;

        // PCIe spec: next offset is either 0 (end) or ≥ 0x100 and DWORD-aligned.
        self.next_offset = if next == 0 || next < PCI_EXT_CAP_START || (next & 0x3) != 0 {
            0
        } else {
            next
        };

        Some(PciExtCapability {
            offset,
            id,
            version,
        })
    }
}

/// Config-space byte offset of the first capability header with `cap_id`.
pub fn pci_find_capability(bus: u8, device: u8, function: u8, cap_id: u8) -> Option<u16> {
    PciCapabilityIter::new(bus, device, function)
        .find(|cap| cap.id == cap_id)
        .map(|cap| cap.offset)
}

/// Config-space byte offset of the first extended capability header with
/// `cap_id`. Requires ECAM MMIO to be active.
pub fn pci_find_ext_capability(bus: u8, device: u8, function: u8, cap_id: u16) -> Option<u16> {
    PciExtCapabilityIter::new(bus, device, function)
        .find(|cap| cap.id == cap_id)
        .map(|cap| cap.offset)
}

impl PciDeviceInfo {
    pub fn find_capability(&self, cap_id: u8) -> Option<u16> {
        pci_find_capability(self.bus, self.device, self.function, cap_id)
    }

    pub fn capabilities(&self) -> PciCapabilityIter {
        PciCapabilityIter::for_device(self)
    }

    /// `None` if ECAM is not active or the capability is absent.
    pub fn find_ext_capability(&self, cap_id: u16) -> Option<u16> {
        pci_find_ext_capability(self.bus, self.device, self.function, cap_id)
    }

    /// Yields no items if ECAM MMIO is not active.
    pub fn ext_capabilities(&self) -> PciExtCapabilityIter {
        PciExtCapabilityIter::for_device(self)
    }
}

fn pci_cap_id_name(id: u8) -> &'static str {
    match id {
        0x01 => "PM",
        0x02 => "AGP",
        0x03 => "VPD",
        0x04 => "SlotID",
        PCI_CAP_ID_MSI => "MSI",
        0x06 => "CompactPCI",
        0x07 => "PCI-X",
        0x08 => "HyperTransport",
        PCI_CAP_ID_VNDR => "Vendor",
        0x0A => "DebugPort",
        0x0B => "CompactPCI-CRC",
        0x0D => "Bridge-SubVID",
        PCI_CAP_ID_PCIE => "PCIe",
        PCI_CAP_ID_MSIX => "MSI-X",
        0x12 => "SATA",
        0x13 => "AF",
        _ => "Unknown",
    }
}

fn pci_ext_cap_id_name(id: u16) -> &'static str {
    match id {
        PCI_EXT_CAP_ID_AER => "AER",
        PCI_EXT_CAP_ID_VC => "VC",
        PCI_EXT_CAP_ID_DSN => "DSN",
        PCI_EXT_CAP_ID_PWR_BUDGET => "PwrBudget",
        PCI_EXT_CAP_ID_VNDR => "VendorExt",
        PCI_EXT_CAP_ID_ACS => "ACS",
        PCI_EXT_CAP_ID_ARI => "ARI",
        PCI_EXT_CAP_ID_ATS => "ATS",
        PCI_EXT_CAP_ID_SRIOV => "SR-IOV",
        PCI_EXT_CAP_ID_LTR => "LTR",
        PCI_EXT_CAP_ID_SEC_PCIE => "SecPCIe",
        PCI_EXT_CAP_ID_L1SS => "L1SS",
        PCI_EXT_CAP_ID_DVSEC => "DVSEC",
        PCI_EXT_CAP_ID_DLF => "DLF",
        PCI_EXT_CAP_ID_PL16G => "PL16G",
        _ => "Unknown",
    }
}

fn pci_read_vendor_id(bus: u8, device: u8, function: u8) -> u16 {
    pci_config_read16(bus, device, function, PCI_VENDOR_ID_OFFSET)
}

fn pci_read_header_type(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_HEADER_TYPE_OFFSET)
}

fn pci_is_multifunction(bus: u8, device: u8) -> bool {
    (pci_read_header_type(bus, device, 0) & 0x80) != 0
}

fn pci_get_secondary_bus(bus: u8, device: u8, function: u8) -> u8 {
    pci_config_read8(bus, device, function, PCI_SECONDARY_BUS_OFFSET)
}

fn pci_probe_bar(bus: u8, device: u8, function: u8, bar_idx: u8) -> PciBarInfo {
    let bar_offset = PCI_BAR0_OFFSET + (bar_idx as u16) * 4;
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
        let size = (!(size_mask & !0x3)).wrapping_add(1) as u32 as u64;
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
        let original_high = if is_64bit && bar_idx < 5 {
            pci_config_read32(bus, device, function, bar_offset + 4)
        } else {
            0
        };
        let size_mask_high = if is_64bit && bar_idx < 5 {
            pci_config_write32(bus, device, function, bar_offset, 0xFFFF_FFFF);
            pci_config_write32(bus, device, function, bar_offset + 4, 0xFFFF_FFFF);
            let high = pci_config_read32(bus, device, function, bar_offset + 4);
            pci_config_write32(bus, device, function, bar_offset + 4, original_high);
            pci_config_write32(bus, device, function, bar_offset, original);
            high
        } else {
            0
        };
        let base_low = (original & !0xF) as u64;
        let base_high = original_high as u64;
        let base = base_low | (base_high << 32);
        let size_mask = (size_mask & !0xF) as u64 | ((size_mask_high as u64) << 32);
        let size_bits = if is_64bit && bar_idx < 5 {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        let size = (!size_mask).wrapping_add(1) & size_bits;
        PciBarInfo {
            base,
            size,
            is_io: 0,
            is_64bit: is_64bit as u8,
            prefetchable: is_prefetchable as u8,
        }
    }
}

/// `#[inline(never)]` so the 144 B BAR array lives in this helper's frame, not
/// the caller's.
#[inline(never)]
fn pci_enumerate_bars(
    bus: u8,
    device: u8,
    function: u8,
    header_type: u8,
) -> ([PciBarInfo; PCI_MAX_BARS], u8) {
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
    (bars, bar_count)
}

#[inline(never)]
fn pci_find_msi_caps(bus: u8, device: u8, function: u8) -> (Option<u16>, Option<u16>) {
    let mut msi_cap_offset: Option<u16> = None;
    let mut msix_cap_offset: Option<u16> = None;
    for cap in PciCapabilityIter::new(bus, device, function) {
        match cap.id {
            PCI_CAP_ID_MSI if msi_cap_offset.is_none() => msi_cap_offset = Some(cap.offset),
            PCI_CAP_ID_MSIX if msix_cap_offset.is_none() => msix_cap_offset = Some(cap.offset),
            _ => {}
        }
    }
    (msi_cap_offset, msix_cap_offset)
}

/// `#[inline(never)]` so each `klog_info!`'s `format_args!` scratch stays in
/// this helper's frame, keeping `pci_probe_device` below the stack gate.
#[inline(never)]
fn pci_log_device_summary(info: &PciDeviceInfo) {
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

    for cap in info.capabilities() {
        klog_info!(
            "    CAP: 0x{:02x} ({}) at offset 0x{:02x}",
            cap.id,
            pci_cap_id_name(cap.id),
            cap.offset
        );
    }

    for ext_cap in info.ext_capabilities() {
        klog_info!(
            "    EXT_CAP: 0x{:04x} ({}) v{} at offset 0x{:03x}",
            ext_cap.id,
            pci_ext_cap_id_name(ext_cap.id),
            ext_cap.version,
            ext_cap.offset
        );
    }

    for (i, bar) in info.bars.iter().enumerate() {
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
}

fn pci_probe_device(state: &mut PciEnumState, bus: u8, device: u8, function: u8) {
    let vendor = pci_read_vendor_id(bus, device, function);
    if vendor == 0xFFFF {
        return;
    }

    let device_id = pci_config_read16(bus, device, function, PCI_DEVICE_ID_OFFSET);
    let class = pci_config_read8(bus, device, function, PCI_CLASS_CODE_OFFSET);
    let subclass = pci_config_read8(bus, device, function, PCI_SUBCLASS_OFFSET);
    let prog_if = pci_config_read8(bus, device, function, PCI_PROG_IF_OFFSET);
    let revision = pci_config_read8(bus, device, function, PCI_REVISION_ID_OFFSET);
    let header_type = pci_read_header_type(bus, device, function) & 0x7F;
    let interrupt_line = pci_config_read8(bus, device, function, PCI_INTERRUPT_LINE_OFFSET);
    let interrupt_pin = pci_config_read8(bus, device, function, PCI_INTERRUPT_PIN_OFFSET);

    let (bars, bar_count) = pci_enumerate_bars(bus, device, function, header_type);
    let (msi_cap_offset, msix_cap_offset) = pci_find_msi_caps(bus, device, function);

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
        msi_cap_offset,
        msix_cap_offset,
    };

    if state.device_count < PCI_MAX_DEVICES {
        state.devices[state.device_count] = info;
        state.device_count += 1;
    }

    pci_log_device_summary(&info);

    if header_type == 1 {
        let secondary = pci_get_secondary_bus(bus, device, function);
        pci_scan_bus_inner(state, secondary);
    }
}

fn pci_scan_bus_inner(state: &mut PciEnumState, bus: u8) {
    if state.bus_visited[bus as usize] != 0 {
        return;
    }
    state.bus_visited[bus as usize] = 1;

    for device in 0..32u8 {
        let vendor = pci_read_vendor_id(bus, device, 0);
        if vendor == 0xFFFF {
            continue;
        }

        pci_probe_device(state, bus, device, 0);

        if pci_is_multifunction(bus, device) {
            for function in 1..8u8 {
                if pci_read_vendor_id(bus, device, function) != 0xFFFF {
                    pci_probe_device(state, bus, device, function);
                }
            }
        }
    }
}

/// ECAM is mandatory, so this panics if MCFG is absent, empty, or the primary
/// segment cannot be mapped.
fn pci_discover_mcfg() {
    if !hhdm::is_available() {
        panic!("PCI: ECAM requires HHDM — cannot initialize PCI subsystem");
    }
    if !platform::is_rsdp_available() {
        panic!("PCI: ECAM requires ACPI RSDP — cannot initialize PCI subsystem");
    }

    let tables = AcpiTables::from_phys(platform::get_rsdp_phys())
        .expect("PCI: ACPI tables validation failed — ECAM requires valid ACPI");

    let mcfg = Mcfg::from_tables(&tables).expect("PCI: No MCFG table — ECAM MMIO is mandatory");

    let count = mcfg.count();
    if count == 0 {
        panic!("PCI: MCFG table present but empty — at least one ECAM entry required");
    }

    let mut primary_pair: Option<(EcamConfigSpace, McfgEntry)> = None;
    let mut extras: KVec<EcamConfigSpace> = KVec::new();
    let mut extra_entries: KVec<McfgEntry> = KVec::new();

    for entry in mcfg.entries() {
        let entry = *entry;
        let region_size = entry.region_size() as usize;
        klog_info!(
            "PCI: ECAM segment {} buses {}..{} at phys 0x{:x} ({}MB)",
            entry.segment,
            entry.bus_start,
            entry.bus_end,
            entry.base_phys,
            region_size / (1024 * 1024),
        );

        let phys = PhysAddr::new(entry.base_phys);
        let region = MmioRegion::map(phys, region_size).unwrap_or_else(|| {
            panic!(
                "PCI: ECAM segment {} MMIO mapping failed ({}MB) — cannot continue",
                entry.segment,
                region_size / (1024 * 1024),
            )
        });
        let region_virt = region.virt_base();

        klog_info!(
            "PCI: ECAM segment {} mapped at virt 0x{:x} ({}MB)",
            entry.segment,
            region_virt,
            region_size / (1024 * 1024),
        );

        let space =
            EcamConfigSpace::new(region, entry.bus_start, entry.bus_end).unwrap_or_else(|| {
                panic!(
                    "PCI: ECAM segment {} bus range invalid for region size",
                    entry.segment,
                )
            });

        if entry.segment == 0 && primary_pair.is_none() {
            primary_pair = Some((space, entry));
        } else {
            extras.push(space).expect("PCI: ECAM extras alloc");
            extra_entries.push(entry).expect("PCI: ECAM extras alloc");
        }
    }

    let (primary, primary_entry) =
        primary_pair.unwrap_or_else(|| panic!("PCI: No primary ECAM segment (segment 0) mapped"));

    ECAM.call_once(|| EcamRegistry {
        primary,
        primary_entry,
        extras,
        extra_entries,
    });

    klog_info!(
        "PCI: ECAM MMIO active — config access via memory-mapped PCIe (4096B per function), {} entry(s) cached",
        count,
    );
}

/// `true` once [`pci_init`] has mapped ECAM MMIO.
#[inline]
pub fn pci_ecam_available() -> bool {
    ECAM.get().is_some()
}

/// Physical base of the primary ECAM region (segment 0), or `0` if MCFG was not
/// found or does not cover segment 0.
#[inline]
pub fn pci_ecam_base() -> u64 {
    ECAM.get().map(|r| r.primary_entry.base_phys).unwrap_or(0)
}

#[inline]
pub fn pci_ecam_entry_count() -> u8 {
    ECAM.get().map(|r| r.entry_count() as u8).unwrap_or(0)
}

pub fn pci_ecam_entry(index: usize) -> Option<McfgEntry> {
    ECAM.get()?.entry(index)
}

pub fn pci_ecam_find_entry(segment: u16, bus: u8) -> Option<McfgEntry> {
    let registry = ECAM.get()?;
    let mut idx = 0;
    while let Some(entry) = registry.entry(idx) {
        if entry.segment == segment
            && bus >= entry.bus_start
            && bus <= entry.bus_end
            && entry.base_phys != 0
        {
            return Some(entry);
        }
        idx += 1;
    }
    None
}

/// `None` if the index is out of range or the region was not mapped.
pub fn pci_ecam_mapped_region(index: usize) -> Option<MmioRegion> {
    ECAM.get()?
        .config_space(index)
        .map(|cs| cs.region().clone())
}

/// Virtual base of the primary ECAM MMIO mapping, or `0` if it was not mapped.
#[inline]
pub fn pci_ecam_primary_virt() -> u64 {
    ECAM.get()
        .map(|r| r.primary.region().virt_base())
        .unwrap_or(0)
}

pub fn pci_init() {
    if !PCI_INIT.init_once() {
        return;
    }

    klog_info!("PCI: Initializing PCI subsystem");

    pci_discover_mcfg();

    let mut state = ENUM_STATE.lock();
    state.device_count = 0;
    state.bus_visited = [0; PCI_MAX_BUSES];

    pci_scan_bus_inner(&mut state, 0);

    let header_type = pci_read_header_type(0, 0, 0);
    if (header_type & 0x80) != 0 {
        for function in 1..8u8 {
            if pci_read_vendor_id(0, 0, function) != 0xFFFF {
                pci_scan_bus_inner(&mut state, function);
            }
        }
    }

    let mut mmio_top = 0u64;
    for dev in &state.devices[..state.device_count] {
        for bar in &dev.bars {
            if bar.is_io == 0 && bar.base != 0 {
                mmio_top = mmio_top.max(bar.base.saturating_add(bar.size));
            }
        }
    }
    MMIO_ALLOC_CURSOR.store(mmio_top, Ordering::Release);

    let count = state.device_count;
    DEVICE_COUNT_CACHE.store(count, Ordering::Release);
    klog_info!("PCI: Enumeration complete. Devices discovered: {}", count);
}

pub fn pci_get_device_count() -> usize {
    DEVICE_COUNT_CACHE.load(Ordering::Acquire)
}

pub fn pci_get_device(index: usize) -> Option<PciDeviceInfo> {
    let state = ENUM_STATE.lock();
    if index < state.device_count {
        Some(state.devices[index])
    } else {
        None
    }
}

/// The name of the driver that has claimed device `dev_idx`, if any.
pub fn pci_device_owner(dev_idx: usize) -> Option<&'static str> {
    CLAIMED_BY.lock().owner(dev_idx)
}

const fn vd_key_of(dev: &PciDeviceInfo) -> u32 {
    ((dev.vendor_id as u32) << 16) | dev.device_id as u32
}

/// Boot-built index over the driver registry giving O(1) candidate lookup.
/// `entries` is the flattened driver set; the buckets hold link indices into it.
pub(crate) struct MatchIndex {
    entries: KVec<&'static PciDriverEntry>,
    by_vd: KBTreeMap<u32, KVec<u16>>,
    by_cs: KBTreeMap<u16, KVec<u16>>,
    catch_all: KVec<u16>,
}

impl MatchIndex {
    /// Build over the live link-section registry.
    fn build() -> Result<Self, AllocError> {
        let mut entries = KVec::new();
        for e in driver_registry_iter() {
            entries.push(e)?;
        }
        Self::build_from(entries)
    }

    /// Build over an explicit driver set; the in-QEMU unit tests pass synthetic
    /// drivers here.
    pub(crate) fn build_from(entries: KVec<&'static PciDriverEntry>) -> Result<Self, AllocError> {
        let mut idx = MatchIndex {
            entries,
            by_vd: KBTreeMap::new(),
            by_cs: KBTreeMap::new(),
            catch_all: KVec::new(),
        };
        for i in 0..idx.entries.len() {
            let e = idx.entries[i];
            let li = i as u16;
            for m in e.match_table {
                if let Some(k) = m.vd_key() {
                    idx.by_vd.entry(k).or_default().push(li)?;
                }
                if let Some(k) = m.cs_key() {
                    idx.by_cs.entry(k).or_default().push(li)?;
                }
            }
            // A fallback is an arbitrary predicate: its driver must be offered
            // every device.
            if e.fallback.is_some() {
                idx.catch_all.push(li)?;
            }
        }
        Ok(idx)
    }
}

impl DriverIndex<PciBus> for MatchIndex {
    fn entry(&self, li: u16) -> &'static PciDriverEntry {
        self.entries[li as usize]
    }

    /// Collect the candidate driver indices for `dev` into `out`, deduplicated
    /// and sorted by (priority, link-index) ascending — specific beats generic.
    fn candidates_for(&self, dev: &PciDeviceInfo, out: &mut KVec<u16>) -> Result<(), AllocError> {
        out.clear();
        if let Some(bucket) = self.by_vd.get(&vd_key_of(dev)) {
            for &li in bucket.iter() {
                push_unique(out, li)?;
            }
        }
        if let Some(bucket) = self.by_cs.get(&(dev.class_code as u16)) {
            for &li in bucket.iter() {
                push_unique(out, li)?;
            }
        }
        for &li in self.catch_all.iter() {
            push_unique(out, li)?;
        }
        sort_candidates::<PciBus>(self.entries.as_slice(), out);
        Ok(())
    }
}

// RESOURCE(1) < REGISTRY(2): never nested with `ENUM_STATE`, so the lock graph
// stays acyclic regardless of which numeric level is larger.
static CLAIMED_BY: SpinLock<ClaimTable<PCI_MAX_DEVICES>> = SpinLock::new(
    ClaimTable::new(),
    lock_class!("pci.CLAIMED_BY", LOCK_LEVEL_RESOURCE),
);

struct GlobalClaims;

impl ClaimSink for GlobalClaims {
    fn is_claimed(&self, dev_idx: usize) -> bool {
        CLAIMED_BY.lock().is_claimed(dev_idx)
    }

    fn record(&self, dev_idx: usize, name: &'static str, devres: Devres) {
        CLAIMED_BY.lock().claim(dev_idx, Binding::new(name), devres);
    }
}

/// Match every enumerated PCI device to a driver and bind exactly one per
/// device, in priority order. Runs once at boot on the BSP.
pub fn pci_probe_drivers() {
    let idx = match MatchIndex::build() {
        Ok(idx) => idx,
        Err(_) => {
            klog_warn!("PCI: match index build failed (OOM); using linear fallback");
            pci_probe_drivers_fallback();
            return;
        }
    };
    let device_count = pci_get_device_count();
    if probe_bus::<PciBus>(&idx, device_count, &|i| pci_get_device(i), &GlobalClaims).is_err() {
        klog_warn!("PCI: matchmaker ran out of memory; some devices may be unbound");
    }
}

/// Allocation-free probe used only when [`MatchIndex::build`] cannot allocate.
/// Drivers are tried in link order, so binding is correct but unordered.
///
/// Deliberately not generic: PCI is the only bus whose devices live in a fixed
/// array, so it is the only one with an allocation-free path to enumerate.
fn pci_probe_drivers_fallback() {
    let device_count = pci_get_device_count();
    for dev_idx in 0..device_count {
        if GlobalClaims.is_claimed(dev_idx) {
            continue;
        }
        let Some(dev) = pci_get_device(dev_idx) else {
            continue;
        };
        for e in driver_registry_iter() {
            if !e.entry_matches(&dev) {
                continue;
            }
            let mut devres = Devres::new();
            let mut bound = BoundDevice::new(&dev, &mut devres);
            match (e.probe)(&mut bound) {
                Ok(ProbeOutcome::Bound) => {
                    drop(bound);
                    GlobalClaims.record(dev_idx, e.name, devres);
                    break;
                }
                Ok(ProbeOutcome::Declined) => continue,
                Err(err) => {
                    klog_info!("PCI: {} declined device {}: {:?}", e.name, dev_idx, err);
                    continue;
                }
            }
        }
    }
}

pub fn pci_get_msi_capable_devices() -> ([PciDeviceInfo; PCI_MAX_DEVICES], usize) {
    let state = ENUM_STATE.lock();
    let mut result = [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES];
    let mut count = 0;
    for i in 0..state.device_count {
        let dev = &state.devices[i];
        if dev.has_msi() || dev.has_msix() {
            result[count] = *dev;
            count += 1;
        }
    }
    (result, count)
}
