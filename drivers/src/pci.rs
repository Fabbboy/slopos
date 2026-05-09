use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_abi::PhysAddr;
use slopos_acpi::mcfg::{Mcfg, McfgEntry};
use slopos_acpi::tables::AcpiTables;
use slopos_kernel_services::platform;
use slopos_mm::hhdm;
use slopos_mm::mmio::{MmioRegion, MmioRegionExt};
use slopos_ostd::dev::FromRawPtr;
use slopos_ostd::pci::{Bdf, EcamConfigSpace};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_REGISTRY, OnceLock, SpinLock};
use slopos_ostd::{KVec, Pod};
use slopos_utils::klog_info;
use slopos_utils::string::cstr_to_str_lossy;

pub use crate::pci_defs::*;

const PCI_SECONDARY_BUS_OFFSET: u16 = 0x19;

#[repr(C)]
#[derive(Clone)]
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

impl Default for PciGpuInfo {
    fn default() -> Self {
        Self::zeroed()
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

struct PciEnumState {
    bus_visited: [u8; PCI_MAX_BUSES],
    devices: [PciDeviceInfo; PCI_MAX_DEVICES],
    device_count: usize,
    primary_gpu: PciGpuInfo,
}

impl PciEnumState {
    const fn new() -> Self {
        Self {
            bus_visited: [0; PCI_MAX_BUSES],
            devices: [PciDeviceInfo::zeroed(); PCI_MAX_DEVICES],
            device_count: 0,
            primary_gpu: PciGpuInfo::zeroed(),
        }
    }
}

struct PciDriverRegistry {
    drivers: [*const PciDriver; PCI_DRIVER_MAX],
    count: usize,
}

impl PciDriverRegistry {
    const fn new() -> Self {
        Self {
            drivers: [ptr::null(); PCI_DRIVER_MAX],
            count: 0,
        }
    }
}

// SAFETY: PciDriverRegistry only stores pointers to 'static PciDrivers
unsafe impl Send for PciDriverRegistry {}

static PCI_INIT: InitFlag = InitFlag::new();
static ENUM_STATE: SpinLock<PciEnumState> = SpinLock::new(PciEnumState::new(), LOCK_LEVEL_REGISTRY);
static DRIVER_REGISTRY: SpinLock<PciDriverRegistry> =
    SpinLock::new(PciDriverRegistry::new(), LOCK_LEVEL_REGISTRY);
static DEVICE_COUNT_CACHE: AtomicUsize = AtomicUsize::new(0);

// =============================================================================
// MCFG / ECAM State
// =============================================================================

/// Cached ECAM segments. Built once during `pci_discover_mcfg()`; read via the
/// public `pci_ecam_*` accessors.  The primary segment (segment 0) is laid
/// out first so the hot path skips an array search.
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

/// PCIe extended configuration space size per function (4 KiB).
const ECAM_FUNCTION_SIZE: u16 = 4096;

// =============================================================================
// PCI Configuration Access
//
// ECAM MMIO is the sole configuration space backend.
// Legacy port I/O (0xCF8/0xCFC) has been removed from the active path.
// ECAM MMIO is mapped during pci_discover_mcfg() and is a hard boot
// requirement — pci_init() panics if MCFG is absent or mapping fails.
// =============================================================================

fn cstr_or_placeholder(s: *const u8) -> &'static str {
    cstr_to_str_lossy(s as *const core::ffi::c_char)
}

// =============================================================================
// ECAM MMIO Implementation
// =============================================================================

/// Resolve `(bus, device, function)` to a typed [`Bdf`] and locate the
/// `EcamConfigSpace` covering that bus.
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

/// Read a 32-bit value from PCI configuration space via ECAM MMIO.
///
/// Supports the full 4096-byte PCIe extended config space (offset 0x000–0xFFC).
/// Returns `None` if ECAM is unavailable, the BDF is out of range, or the
/// offset is misaligned / out of bounds.
pub fn pci_ecam_read32(bus: u8, device: u8, function: u8, offset: u16) -> Option<u32> {
    if offset & 0x3 != 0 {
        return None;
    }
    ecam_read::<u32>(bus, device, function, offset)
}

/// Read a 16-bit value from PCI configuration space via ECAM MMIO.
pub fn pci_ecam_read16(bus: u8, device: u8, function: u8, offset: u16) -> Option<u16> {
    if offset & 0x1 != 0 {
        return None;
    }
    ecam_read::<u16>(bus, device, function, offset)
}

/// Read an 8-bit value from PCI configuration space via ECAM MMIO.
pub fn pci_ecam_read8(bus: u8, device: u8, function: u8, offset: u16) -> Option<u8> {
    ecam_read::<u8>(bus, device, function, offset)
}

/// Write a 32-bit value to PCI configuration space via ECAM MMIO.
///
/// Returns `None` if ECAM is unavailable, the BDF is out of range, or the
/// offset is misaligned / out of bounds.
pub fn pci_ecam_write32(bus: u8, device: u8, function: u8, offset: u16, value: u32) -> Option<()> {
    if offset & 0x3 != 0 {
        return None;
    }
    ecam_write::<u32>(bus, device, function, offset, value)
}

/// Write a 16-bit value to PCI configuration space via ECAM MMIO.
pub fn pci_ecam_write16(bus: u8, device: u8, function: u8, offset: u16, value: u16) -> Option<()> {
    if offset & 0x1 != 0 {
        return None;
    }
    ecam_write::<u16>(bus, device, function, offset, value)
}

/// Write an 8-bit value to PCI configuration space via ECAM MMIO.
pub fn pci_ecam_write8(bus: u8, device: u8, function: u8, offset: u16, value: u8) -> Option<()> {
    ecam_write::<u8>(bus, device, function, offset, value)
}

// =============================================================================
// Public PCI Configuration Access (ECAM-only)
//
// All config space reads/writes go through ECAM MMIO.  The offset parameter
// is u16, supporting the full 4096-byte PCIe extended config space.
//
// These functions panic on ECAM read failure (which indicates a bug — the ECAM
// region is validated at boot).  Use the `pci_ecam_read*` variants directly
// if you need fallible access.
// =============================================================================

/// Read a 32-bit value from PCI configuration space via ECAM MMIO.
///
/// Supports the full 4096-byte PCIe config space (offset 0x000–0xFFC).
/// Panics if the ECAM read fails (offset misaligned or out of range).
#[inline]
pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    pci_ecam_read32(bus, device, function, offset).expect("pci_config_read32: ECAM read failed")
}

/// Read a 16-bit value from PCI configuration space via ECAM MMIO.
#[inline]
pub fn pci_config_read16(bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    pci_ecam_read16(bus, device, function, offset).expect("pci_config_read16: ECAM read failed")
}

/// Read an 8-bit value from PCI configuration space via ECAM MMIO.
#[inline]
pub fn pci_config_read8(bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    pci_ecam_read8(bus, device, function, offset).expect("pci_config_read8: ECAM read failed")
}

/// Write a 32-bit value to PCI configuration space via ECAM MMIO.
#[inline]
pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u16, value: u32) {
    pci_ecam_write32(bus, device, function, offset, value)
        .expect("pci_config_write32: ECAM write failed");
}

/// Write a 16-bit value to PCI configuration space via ECAM MMIO.
#[inline]
pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u16, value: u16) {
    pci_ecam_write16(bus, device, function, offset, value)
        .expect("pci_config_write16: ECAM write failed");
}

/// Write an 8-bit value to PCI configuration space via ECAM MMIO.
#[inline]
pub fn pci_config_write8(bus: u8, device: u8, function: u8, offset: u16, value: u8) {
    pci_ecam_write8(bus, device, function, offset, value)
        .expect("pci_config_write8: ECAM write failed");
}

// =============================================================================
// PCI Capability List Walking
// =============================================================================

/// Iterator over PCI capabilities in a device's configuration space.
///
/// Walks the capability linked list starting from the Capabilities Pointer
/// (offset 0x34). Each capability header contains an 8-bit ID and a pointer
/// to the next capability.
///
/// # Infinite-loop protection
///
/// A guard counter limits traversal to [`Self::MAX_CAPS`] entries to protect
/// against malformed capability lists on buggy hardware.
pub struct PciCapabilityIter {
    bus: u8,
    device: u8,
    function: u8,
    next_ptr: u16,
    /// Remaining entries before we give up (infinite-loop guard).
    remaining: u8,
}

impl PciCapabilityIter {
    /// Maximum capabilities to visit before assuming a malformed list.
    ///
    /// The standard 256-byte config space can fit at most ~60 entries
    /// (4 bytes minimum per capability, starting around offset 0x40).
    /// 48 is a generous upper bound matching Linux's `PCI_FIND_CAP_TTL`.
    const MAX_CAPS: u8 = 48;

    /// Create a capability iterator for the specified PCI function.
    ///
    /// Returns an empty iterator if the device's Status register does not
    /// advertise a capabilities list (bit 4 of Status).
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

    /// Create a capability iterator for a known [`PciDeviceInfo`].
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

// =============================================================================
// PCIe Extended Capability List Walking (offset 0x100+, ECAM-only)
// =============================================================================

/// Iterator over PCIe extended capabilities in a device's configuration space.
///
/// Extended capabilities occupy offsets 0x100–0xFFF and are only accessible via
/// ECAM MMIO (4096-byte config space).  Each header is a 32-bit DWORD:
///
/// ```text
///   bits [15:0]  — Capability ID (16-bit)
///   bits [19:16] — Capability Version (4-bit)
///   bits [31:20] — Next Capability Offset (12-bit, 0 = end of list)
/// ```
///
/// A header value of `0x0000_0000` or `0xFFFF_FFFF` at the first extended
/// capability offset (0x100) indicates the device has no extended capabilities.
///
/// # Infinite-loop protection
///
/// A guard counter limits traversal to [`Self::MAX_EXT_CAPS`] entries.
pub struct PciExtCapabilityIter {
    bus: u8,
    device: u8,
    function: u8,
    next_offset: u16,
    /// Remaining entries before we give up (infinite-loop guard).
    remaining: u8,
}

impl PciExtCapabilityIter {
    /// Maximum extended capabilities to visit before assuming a malformed list.
    ///
    /// The extended config space (0x100–0xFFF = 3840 bytes) can fit at most ~240
    /// 16-byte entries.  48 matches the standard capability guard in
    /// [`PciCapabilityIter`] and Linux's `PCI_FIND_CAP_TTL`.
    const MAX_EXT_CAPS: u8 = 48;

    /// Create an extended capability iterator for the specified PCI function.
    ///
    /// Returns an empty iterator (yielding no items) if:
    /// - ECAM MMIO is not active (extended config space inaccessible)
    /// - The first extended capability header is absent (`0x0000_0000` or
    ///   `0xFFFF_FFFF` at offset 0x100)
    pub fn new(bus: u8, device: u8, function: u8) -> Self {
        let first_offset = if pci_ecam_available() {
            match pci_ecam_read32(bus, device, function, PCI_EXT_CAP_START) {
                // No extended capabilities or device not present.
                Some(0x0000_0000) | Some(0xFFFF_FFFF) | None => 0,
                // Valid header — start iteration at 0x100.
                Some(_) => PCI_EXT_CAP_START,
            }
        } else {
            // Extended config space requires ECAM.
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

    /// Create an extended capability iterator for a known [`PciDeviceInfo`].
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

        // A zero or all-ones header terminates the list (shouldn't normally happen
        // mid-list, but guard against malformed hardware).
        if header == 0 || header == 0xFFFF_FFFF {
            self.next_offset = 0;
            return None;
        }

        let id = (header & 0xFFFF) as u16;
        let version = ((header >> 16) & 0xF) as u8;
        let next = ((header >> 20) & 0xFFF) as u16;

        // PCIe spec: next offset must be either 0 (end) or ≥ 0x100 and
        // DWORD-aligned.  Reject anything that points below 0x100 or is
        // unaligned.
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

/// Find the first PCI capability with the given ID.
///
/// Returns the config-space byte offset of the capability header,
/// or `None` if the device doesn't advertise that capability.
pub fn pci_find_capability(bus: u8, device: u8, function: u8, cap_id: u8) -> Option<u16> {
    PciCapabilityIter::new(bus, device, function)
        .find(|cap| cap.id == cap_id)
        .map(|cap| cap.offset)
}

/// Find the first PCIe extended capability with the given ID.
///
/// Returns the config-space byte offset of the extended capability header,
/// or `None` if the device has no extended capabilities or the requested
/// capability is absent.  Requires ECAM MMIO to be active.
pub fn pci_find_ext_capability(bus: u8, device: u8, function: u8, cap_id: u16) -> Option<u16> {
    PciExtCapabilityIter::new(bus, device, function)
        .find(|cap| cap.id == cap_id)
        .map(|cap| cap.offset)
}

/// Convenience methods for PCI capability queries on a known device.
impl PciDeviceInfo {
    /// Find the first standard capability with the given ID for this device.
    pub fn find_capability(&self, cap_id: u8) -> Option<u16> {
        pci_find_capability(self.bus, self.device, self.function, cap_id)
    }

    /// Iterate over all standard PCI capabilities of this device.
    pub fn capabilities(&self) -> PciCapabilityIter {
        PciCapabilityIter::for_device(self)
    }

    /// Find the first PCIe extended capability with the given ID for this device.
    ///
    /// Returns `None` if ECAM is not active or the capability is absent.
    pub fn find_ext_capability(&self, cap_id: u16) -> Option<u16> {
        pci_find_ext_capability(self.bus, self.device, self.function, cap_id)
    }

    /// Iterate over all PCIe extended capabilities of this device.
    ///
    /// Yields no items if ECAM MMIO is not active.
    pub fn ext_capabilities(&self) -> PciExtCapabilityIter {
        PciExtCapabilityIter::for_device(self)
    }
}

/// Human-readable name for a PCI capability ID (for boot log output).
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

/// Human-readable name for a PCIe extended capability ID (for boot log output).
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

/// Enumerate BARs for a non-bridge function and return the `[PciBarInfo;
/// 6]` array plus the populated-entry count. `#[inline(never)]` so the
/// 144 B BAR array lives in this helper's frame, not the caller's.
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

/// Walk the capability list once, extracting MSI and MSI-X offsets.
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

/// Log summary + capability + BAR lines for one device. `#[inline(never)]`
/// so each `klog_info!`'s `format_args!` scratch stays in this helper's
/// frame, keeping `pci_probe_device` below the 1 KiB stack gate.
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

    if class == 0x03 && subclass == 0x00 {
        for bar in &bars {
            if bar.is_io == 0 && bar.base != 0 && bar.size != 0 {
                if state.primary_gpu.present == 0 {
                    state.primary_gpu.present = 1;
                    state.primary_gpu.device = info;
                    state.primary_gpu.mmio_phys_base = bar.base;
                    state.primary_gpu.mmio_size = bar.size;

                    let phys = PhysAddr::new(bar.base);
                    state.primary_gpu.mmio_region =
                        MmioRegion::map(phys, bar.size as usize).unwrap_or_else(MmioRegion::empty);
                    klog_info!(
                        "PCI: Selected display-class GPU candidate at MMIO phys=0x{:x} size=0x{:x} virt=0x{:x}",
                        bar.base,
                        bar.size,
                        state.primary_gpu.mmio_region.virt_base()
                    );
                }
                break;
            }
        }
    }

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

// =============================================================================
// MCFG / ECAM Discovery + MMIO Mapping
// =============================================================================

/// Discover and cache MCFG (PCIe ECAM) entries from ACPI tables, then map
/// each entry's configuration space into virtual memory.
///
/// Called during [`pci_init`] before bus enumeration. ECAM is mandatory and
/// this function panics if MCFG is absent, empty, or the
/// primary segment's MMIO region cannot be mapped.
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

// =============================================================================
// Public ECAM Accessors
// =============================================================================

/// Check whether ECAM configuration space is available.
///
/// Returns `true` after [`pci_init`] has successfully mapped ECAM MMIO.
#[inline]
pub fn pci_ecam_available() -> bool {
    ECAM.get().is_some()
}

/// Return the physical base address of the primary ECAM region (segment 0).
///
/// Returns `0` if MCFG was not found or does not cover segment 0.
#[inline]
pub fn pci_ecam_base() -> u64 {
    ECAM.get().map(|r| r.primary_entry.base_phys).unwrap_or(0)
}

/// Return the number of cached ECAM entries.
#[inline]
pub fn pci_ecam_entry_count() -> u8 {
    ECAM.get().map(|r| r.entry_count() as u8).unwrap_or(0)
}

/// Retrieve a specific ECAM entry by index.
pub fn pci_ecam_entry(index: usize) -> Option<McfgEntry> {
    ECAM.get()?.entry(index)
}

/// Find the ECAM entry that covers a given segment and bus.
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

/// Retrieve the mapped MMIO region for a given ECAM entry index.
///
/// Returns `None` if the index is out of range or the region was not mapped.
pub fn pci_ecam_mapped_region(index: usize) -> Option<MmioRegion> {
    ECAM.get()?
        .config_space(index)
        .map(|cs| cs.region().clone())
}

/// Return the virtual base address of the primary ECAM MMIO mapping.
///
/// Returns `0` if the primary segment was not mapped.
#[inline]
pub fn pci_ecam_primary_virt() -> u64 {
    ECAM.get()
        .map(|r| r.primary.region().virt_base())
        .unwrap_or(0)
}

// =============================================================================
// Initialization
// =============================================================================

pub fn pci_init() {
    if !PCI_INIT.init_once() {
        return;
    }

    klog_info!("PCI: Initializing PCI subsystem");

    pci_discover_mcfg();

    let mut state = ENUM_STATE.lock();
    state.device_count = 0;
    state.bus_visited = [0; PCI_MAX_BUSES];
    state.primary_gpu = PciGpuInfo::zeroed();

    pci_scan_bus_inner(&mut state, 0);

    let header_type = pci_read_header_type(0, 0, 0);
    if (header_type & 0x80) != 0 {
        for function in 1..8u8 {
            if pci_read_vendor_id(0, 0, function) != 0xFFFF {
                pci_scan_bus_inner(&mut state, function);
            }
        }
    }

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

pub fn pci_get_primary_gpu() -> PciGpuInfo {
    ENUM_STATE.lock().primary_gpu.clone()
}

pub fn pci_register_driver(driver: &'static PciDriver) -> c_int {
    let mut registry = DRIVER_REGISTRY.lock();
    let idx = registry.count;
    if idx >= PCI_DRIVER_MAX {
        return -1;
    }
    let name = cstr_or_placeholder(driver.name);
    klog_info!("PCI: Registered driver {}", name);
    registry.drivers[idx] = driver;
    registry.count = idx + 1;
    0
}

pub fn pci_probe_drivers() {
    let registry = DRIVER_REGISTRY.lock();
    let state = ENUM_STATE.lock();

    for drv_idx in 0..registry.count {
        let Some(drv) = PciDriver::from_ptr(registry.drivers[drv_idx]) else {
            continue;
        };
        for dev_idx in 0..state.device_count {
            let dev = &state.devices[dev_idx];
            if let Some(mf) = drv.match_fn {
                if mf(dev, drv.context) {
                    if let Some(probe) = drv.probe {
                        let _ = probe(dev, drv.context);
                    }
                }
            }
        }
    }
}

/// Retrieve all devices that advertise MSI or MSI-X capability.
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
