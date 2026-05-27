use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, BlockDeviceIndex};
use slopos_ostd::KArc;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{klog_debug, klog_info, write_field, write_init_field};

use crate::pci::{PciDeviceInfo, PciProbeError};
use crate::virtio::{
    self, CompletionEvent, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
    VirtioMmioCaps, VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok, setup_interrupts,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

use slopos_mm::page_alloc::OwnedPageFrame;

pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: u64 = 512;
const REQUEST_TIMEOUT_MS: u32 = 5000;

#[repr(C)]
#[derive(Clone, Copy, slopos_ostd::Pod)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
struct VirtioBlkDevice {
    queue: Virtqueue,
    capacity_sectors: u64,
    ready: bool,
}

impl VirtioBlkDevice {
    const fn new() -> Self {
        Self {
            queue: Virtqueue::new(),
            capacity_sectors: 0,
            ready: false,
        }
    }
}

/// Combined device + MMIO caps + interrupt state under a single lock,
/// ensuring ownership/claim state and the request path share one coherent
/// synchronization model.
struct VirtioBlkState {
    device: VirtioBlkDevice,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
}

impl VirtioBlkState {
    /// In-place recipe for the empty (pre-probe) state. Written field by
    /// field into the heap slot so the ~280-byte aggregate never lands on
    /// the prober's stack (the 2 KiB frame gate).
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(|slot: SlotPtr<Self>| -> Result<(), AllocError> {
            write_field!(slot, device, VirtioBlkDevice::new());
            write_field!(slot, caps, VirtioMmioCaps::empty());
            write_field!(slot, msix_state, None);
            Ok(())
        })
    }
}

/// Owned per-device state for one claimed virtio-blk device.
///
/// Lives on the heap inside a [`KArc`] so its address is stable: the
/// per-device IRQ closure and the registry both hold clones, and the
/// closure signals `queue_event` from interrupt context. Replacing the
/// former global statics (`VIRTIO_BLK_STATE` / `BLK_QUEUE_EVENT` /
/// `BLK_REQUEST_IN_FLIGHT`) with per-device fields is what makes
/// multi-device ownership — and thus exclusive write claims — possible.
struct VirtioBlkInner {
    /// Device + caps + MSI-X state. `LOCK_LEVEL_RESOURCE`.
    state: SpinLock<VirtioBlkState>,
    /// Single-request serialization (one in-flight request per device).
    request_in_flight: AtomicBool,
    /// Completion signalled by this device's own IRQ handler.
    queue_event: CompletionEvent,
}

impl VirtioBlkInner {
    /// In-place recipe for a fresh, empty device. Built via
    /// [`KArc::try_init`] so neither the `VirtioBlkState` nor the
    /// surrounding `KArc` inner ever materialises on the caller's stack.
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(|slot: SlotPtr<Self>| -> Result<(), AllocError> {
            write_init_field!(
                slot,
                state,
                SpinLock::init_with(LOCK_LEVEL_RESOURCE, VirtioBlkState::init_empty())
            )?;
            write_field!(slot, request_in_flight, AtomicBool::new(false));
            write_field!(slot, queue_event, CompletionEvent::new());
            Ok(())
        })
    }

    fn is_ready(&self) -> bool {
        self.state.lock().device.ready
    }

    fn capacity_bytes(&self) -> u64 {
        self.state.lock().device.capacity_sectors * SECTOR_SIZE
    }

    #[cfg(feature = "test-hooks")]
    fn msix_state(&self) -> Option<VirtioMsixState> {
        self.state.lock().msix_state.clone()
    }

    fn do_request(&self, sector: u64, buffer: &mut [u8], write: bool) -> bool {
        let _request_guard = RequestGuard::acquire(&self.request_in_flight);

        {
            let state = self.state.lock();
            if !state.device.queue.is_ready() {
                return false;
            }
        }

        let buffers = match RequestBuffers::allocate() {
            Some(b) => b,
            None => return false,
        };

        let req_phys = buffers.req_page.phys_u64();
        let status_offset = size_of::<VirtioBlkReqHeader>();
        let status_phys = req_phys + status_offset as u64;
        let bounce_phys = buffers.bounce_page.phys_u64();
        let len = buffer.len();

        if write && !buffers.bounce_page.write_slice(0, buffer) {
            return false;
        }

        let header = VirtioBlkReqHeader {
            type_: if write {
                VIRTIO_BLK_T_OUT
            } else {
                VIRTIO_BLK_T_IN
            },
            reserved: 0,
            sector,
        };
        if !buffers.req_page.write_at::<VirtioBlkReqHeader>(0, &header) {
            return false;
        }
        if !buffers
            .req_page
            .write_volatile_at::<u8>(status_offset, 0xFF)
        {
            return false;
        }

        {
            let mut state = self.state.lock();
            self.queue_event.reset();

            state.device.queue.write_desc(
                0,
                VirtqDesc {
                    addr: req_phys,
                    len: size_of::<VirtioBlkReqHeader>() as u32,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: 1,
                },
            );

            state.device.queue.write_desc(
                1,
                VirtqDesc {
                    addr: bounce_phys,
                    len: len as u32,
                    flags: if write {
                        VIRTQ_DESC_F_NEXT
                    } else {
                        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
                    },
                    next: 2,
                },
            );

            state.device.queue.write_desc(
                2,
                VirtqDesc {
                    addr: status_phys,
                    len: 1,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );

            state.device.queue.submit(0);
            queue::notify_queue(
                &state.caps.notify_cfg,
                state.caps.notify_off_multiplier,
                &state.device.queue,
                0,
            );
        }

        if !self.queue_event.wait_timeout_ms(REQUEST_TIMEOUT_MS) {
            klog_info!("virtio-blk: request timeout");
            return false;
        }

        {
            let mut state = self.state.lock();
            if !state.device.queue.advance_used() {
                klog_info!("virtio-blk: signaled without used completion");
                return false;
            }
        }

        let status = buffers
            .req_page
            .read_volatile_at::<u8>(status_offset)
            .unwrap_or(0xFF);
        let success = status == VIRTIO_BLK_S_OK;

        if success && !write && !buffers.bounce_page.read_slice(0, buffer) {
            return false;
        }

        success
    }

    fn read_offset(&self, offset: u64, buffer: &mut [u8]) -> bool {
        if buffer.is_empty() {
            return true;
        }
        if !self.is_ready() {
            return false;
        }

        let start_sector = offset / SECTOR_SIZE;
        let sector_offset = (offset % SECTOR_SIZE) as usize;

        let mut sector_buf = [0u8; 512];
        let sectors_needed = (sector_offset + buffer.len() + 511) / 512;

        let mut buf_pos = 0usize;
        for i in 0..sectors_needed {
            let sector = start_sector + i as u64;
            if !self.do_request(sector, &mut sector_buf, false) {
                return false;
            }

            let src_start = if i == 0 { sector_offset } else { 0 };
            let src_end = 512.min(src_start + (buffer.len() - buf_pos));
            let copy_len = src_end - src_start;

            buffer[buf_pos..buf_pos + copy_len].copy_from_slice(&sector_buf[src_start..src_end]);
            buf_pos += copy_len;

            if buf_pos >= buffer.len() {
                break;
            }
        }

        true
    }

    fn write_offset(&self, offset: u64, buffer: &[u8]) -> bool {
        if buffer.is_empty() {
            return true;
        }
        if !self.is_ready() {
            return false;
        }

        let start_sector = offset / SECTOR_SIZE;
        let sector_offset = (offset % SECTOR_SIZE) as usize;

        let mut sector_buf = [0u8; 512];
        let sectors_needed = (sector_offset + buffer.len() + 511) / 512;

        let mut buf_pos = 0usize;
        for i in 0..sectors_needed {
            let sector = start_sector + i as u64;

            let dst_start = if i == 0 { sector_offset } else { 0 };
            let dst_end = 512.min(dst_start + (buffer.len() - buf_pos));
            let copy_len = dst_end - dst_start;

            // Read-modify-write for partial sectors so we never clobber the
            // bytes outside [dst_start, dst_end).
            if (dst_start != 0 || dst_end != 512)
                && !self.do_request(sector, &mut sector_buf, false)
            {
                return false;
            }

            sector_buf[dst_start..dst_end].copy_from_slice(&buffer[buf_pos..buf_pos + copy_len]);

            if !self.do_request(sector, &mut sector_buf, true) {
                return false;
            }

            buf_pos += copy_len;
            if buf_pos >= buffer.len() {
                break;
            }
        }

        true
    }
}

struct RequestGuard<'a> {
    flag: &'a AtomicBool,
}

impl<'a> RequestGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Self {
        while flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Self { flag }
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

struct RequestBuffers {
    req_page: OwnedPageFrame,
    bounce_page: OwnedPageFrame,
}

impl RequestBuffers {
    fn allocate() -> Option<Self> {
        let req_page = OwnedPageFrame::alloc_zeroed()?;
        let bounce_page = OwnedPageFrame::alloc_zeroed()?;
        Some(Self {
            req_page,
            bounce_page,
        })
    }
}

// ============================================================================
// Device registry + capability handles
//
// The registry owns every claimed virtio-blk device and is the ONLY way to
// reach one: there is no ambient "write any LBA" free function. Code obtains
// a device by index, then either reads through a borrowed handle or acquires
// an EXCLUSIVE write capability ([`open_writer`]) — modelled on Linux's
// `bd_writers` / FreeBSD GEOM exclusive-access counts. A second writer claim
// is rejected, so a buggy caller (or a test) cannot silently write a device
// the filesystem already owns.
// ============================================================================

const MAX_BLK_DEVICES: usize = 8;
const SLOT_BITS: u32 = 8;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

/// Opaque, unforgeable handle to a registered virtio-blk device. Encodes a
/// slot index plus a generation counter (mirroring `MemfdHandle`) so a stale
/// handle fails validation instead of aliasing a different device. The only
/// constructor is private to this module.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct DevHandle(u32);

impl DevHandle {
    fn new(slot: usize, generation: u32) -> Self {
        DevHandle((generation << SLOT_BITS) | (slot as u32 & SLOT_MASK))
    }
    fn slot(self) -> usize {
        (self.0 & SLOT_MASK) as usize
    }
    fn generation(self) -> u32 {
        self.0 >> SLOT_BITS
    }
}

/// Error from acquiring an exclusive write capability on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkClaimError {
    /// The handle does not refer to a live device (freed/never existed).
    Stale,
    /// Another holder already owns the exclusive write capability.
    AlreadyClaimed,
}

struct DevSlot {
    inner: Option<KArc<VirtioBlkInner>>,
    generation: u32,
    index: u16,
    write_claimed: bool,
}

impl DevSlot {
    const fn empty() -> Self {
        Self {
            inner: None,
            generation: 0,
            index: 0,
            write_claimed: false,
        }
    }
}

struct BlkRegistry {
    slots: [DevSlot; MAX_BLK_DEVICES],
    next_generation: u32,
    probe_order_next: u16,
}

impl BlkRegistry {
    const fn new() -> Self {
        const EMPTY: DevSlot = DevSlot::empty();
        Self {
            slots: [EMPTY; MAX_BLK_DEVICES],
            // Generation 0 is reserved as "none" so a zeroed handle is invalid.
            next_generation: 1,
            probe_order_next: 0,
        }
    }
}

static BLK_REGISTRY: SpinLock<BlkRegistry> = SpinLock::new(BlkRegistry::new(), LOCK_LEVEL_REGISTRY);

/// Validate a handle against the live registry, returning its slot index.
fn validate(reg: &BlkRegistry, handle: DevHandle) -> Option<usize> {
    let slot = handle.slot();
    if slot >= MAX_BLK_DEVICES || handle.generation() == 0 {
        return None;
    }
    let s = &reg.slots[slot];
    if s.inner.is_some() && s.generation == handle.generation() {
        Some(slot)
    } else {
        None
    }
}

/// Register a freshly-probed device. Assigns the next probe-order index
/// (disk0, disk1, …) and a fresh generation, returning the index. Called
/// once per device from `virtio_blk_probe` while the PCI `ENUM_STATE` lock
/// is held; the only nesting is `ENUM_STATE -> BLK_REGISTRY` (never the
/// reverse), so no cycle. Callers reach the device afterwards via
/// [`blk_device_by_index`].
fn register_device(inner: KArc<VirtioBlkInner>) -> Option<BlockDeviceIndex> {
    let mut reg = BLK_REGISTRY.lock();
    let free = (0..MAX_BLK_DEVICES).find(|&s| reg.slots[s].inner.is_none())?;
    let generation = reg.next_generation;
    let index = reg.probe_order_next;
    reg.next_generation = reg.next_generation.wrapping_add(1);
    if reg.next_generation == 0 {
        reg.next_generation = 1;
    }
    reg.probe_order_next = reg.probe_order_next.wrapping_add(1);
    reg.slots[free] = DevSlot {
        inner: Some(inner),
        generation,
        index,
        write_claimed: false,
    };
    Some(BlockDeviceIndex(index))
}

/// Clone the device's owned state out from under the registry lock, so the
/// (potentially blocking) I/O path never runs while holding the registry.
fn clone_inner(handle: DevHandle) -> Option<KArc<VirtioBlkInner>> {
    let reg = BLK_REGISTRY.lock();
    let slot = validate(&reg, handle)?;
    reg.slots[slot].inner.clone()
}

/// Number of claimed virtio-blk devices.
pub fn blk_device_count() -> usize {
    let reg = BLK_REGISTRY.lock();
    reg.slots.iter().filter(|s| s.inner.is_some()).count()
}

/// Look a device up by stable probe-order index (disk0 = first probed).
pub fn blk_device_by_index(index: BlockDeviceIndex) -> Option<DevHandle> {
    let reg = BLK_REGISTRY.lock();
    (0..MAX_BLK_DEVICES).find_map(|slot| {
        let s = &reg.slots[slot];
        if s.inner.is_some() && s.index == index.0 {
            Some(DevHandle::new(slot, s.generation))
        } else {
            None
        }
    })
}

/// Read-only block access through a borrowed handle (no exclusivity needed).
pub fn blk_read(handle: DevHandle, offset: u64, buffer: &mut [u8]) -> bool {
    clone_inner(handle).is_some_and(|inner| inner.read_offset(offset, buffer))
}

/// Whether the device is probed and ready.
pub fn blk_is_ready(handle: DevHandle) -> bool {
    clone_inner(handle).is_some_and(|inner| inner.is_ready())
}

/// Device capacity in bytes (0 if the handle is stale).
pub fn blk_capacity(handle: DevHandle) -> u64 {
    clone_inner(handle).map_or(0, |inner| inner.capacity_bytes())
}

#[cfg(feature = "test-hooks")]
pub fn blk_msix_state(handle: DevHandle) -> Option<VirtioMsixState> {
    clone_inner(handle).and_then(|inner| inner.msix_state())
}

/// Acquire the EXCLUSIVE write capability for a device. Returns an owned
/// [`BlockWriteToken`]; a second `open_writer` on the same device returns
/// [`BlkClaimError::AlreadyClaimed`] until the first token is dropped.
pub fn open_writer(handle: DevHandle) -> Result<BlockWriteToken, BlkClaimError> {
    let mut reg = BLK_REGISTRY.lock();
    let slot = validate(&reg, handle).ok_or(BlkClaimError::Stale)?;
    if reg.slots[slot].write_claimed {
        return Err(BlkClaimError::AlreadyClaimed);
    }
    // `validate` already proved `inner.is_some()` under this same guard, so
    // this is provably `Some`; match defensively rather than panic on a
    // registry path.
    let Some(inner) = reg.slots[slot].inner.clone() else {
        return Err(BlkClaimError::Stale);
    };
    reg.slots[slot].write_claimed = true;
    Ok(BlockWriteToken { handle, inner })
}

/// Owned, exclusive read+write capability for one block device. Implements
/// [`BlockDevice`] so the filesystem holds it as its sole writable handle.
/// Dropping it releases the exclusive write claim.
pub struct BlockWriteToken {
    handle: DevHandle,
    inner: KArc<VirtioBlkInner>,
}

impl BlockDevice for BlockWriteToken {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if self.inner.read_offset(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if self.inner.write_offset(offset, buffer) {
            Ok(())
        } else {
            Err(BlockDeviceError::InvalidBuffer)
        }
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity_bytes()
    }
}

impl Drop for BlockWriteToken {
    fn drop(&mut self) {
        let mut reg = BLK_REGISTRY.lock();
        if let Some(slot) = validate(&reg, self.handle) {
            reg.slots[slot].write_claimed = false;
        }
    }
}

fn virtio_blk_matches(info: &PciDeviceInfo) -> bool {
    if info.vendor_id != PCI_VENDOR_ID_VIRTIO {
        return false;
    }
    info.device_id == VIRTIO_BLK_DEVICE_ID_LEGACY || info.device_id == VIRTIO_BLK_DEVICE_ID_MODERN
}

fn read_capacity(caps: &VirtioMmioCaps) -> u64 {
    if !caps.has_device_cfg() {
        return 0;
    }
    let lo = caps.device_cfg.read::<u32>(0) as u64;
    let hi = caps.device_cfg.read::<u32>(4) as u64;
    lo | (hi << 32)
}

fn virtio_blk_probe(info: &PciDeviceInfo) -> Result<(), PciProbeError> {
    klog_info!(
        "virtio-blk: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(info);

    let caps = parse_capabilities(info);

    klog_debug!(
        "virtio-blk: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        return Err(PciProbeError::Unsupported);
    }

    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, 0);
    if !feat_result.success {
        klog_info!("virtio-blk: features negotiation failed");
        return Err(PciProbeError::DeviceFault);
    }

    // Allocate the per-device state up front (empty) so the IRQ closure can
    // capture a KArc clone and signal THIS device's completion event.
    let inner = match KArc::try_init(VirtioBlkInner::init_empty()) {
        Ok(i) => i,
        Err(_) => return Err(PciProbeError::OutOfMemory),
    };

    // --- MSI-X / MSI interrupt setup ---
    // VirtIO modern on q35 always has MSI-X; MSI is the minimum fallback.
    // The handler is a per-device closure that signals this device's own
    // CompletionEvent — no global IRQ sink.
    let inner_for_irq = inner.clone();
    let (irq_mode, msix_state) = setup_interrupts(info, &caps, 1, move |_q: u8| {
        inner_for_irq.queue_event.signal();
    })
    .unwrap_or_else(|msg| {
        panic!(
            "virtio-blk: {}:{}.{} {}",
            info.bus, info.device, info.function, msg
        )
    });
    let q0_msix_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));

    let queue = match queue::setup_queue(&caps.common_cfg, 0, DEFAULT_QUEUE_SIZE, q0_msix_entry) {
        Some(q) => q,
        None => {
            klog_info!("virtio-blk: queue setup failed");
            return Err(PciProbeError::OutOfMemory);
        }
    };

    set_driver_ok(&caps);

    let capacity_sectors = read_capacity(&caps);

    {
        let mut state = inner.state.lock();
        state.device = VirtioBlkDevice {
            queue,
            capacity_sectors,
            ready: true,
        };
        state.caps = caps;
        state.msix_state = msix_state;
    }

    let index = match register_device(inner) {
        Some(i) => i,
        None => {
            klog_info!("virtio-blk: device registry full");
            return Err(PciProbeError::OutOfMemory);
        }
    };

    klog_info!(
        "virtio-blk: disk{} ready, capacity {} sectors ({} MB), irq {:?}",
        index.0,
        capacity_sectors,
        (capacity_sectors * SECTOR_SIZE) / (1024 * 1024),
        irq_mode,
    );

    Ok(())
}

crate::pci_driver! {
    pub static VIRTIO_BLK_DRIVER = {
        name: "virtio-blk",
        matches: virtio_blk_matches,
        probe: virtio_blk_probe,
    };
}

// All block access now flows through the capability registry above
// (`blk_device_by_index` + `open_writer`/`blk_read`/`blk_is_ready` /
// `blk_capacity` / `blk_msix_state`). There is no ambient "read/write any
// LBA on the global device" free function: that surface was the root of the
// io_capture on-disk corruption and has been removed.
