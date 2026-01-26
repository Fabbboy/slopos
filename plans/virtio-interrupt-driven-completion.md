# VirtIO Interrupt-Driven Completion Migration Plan

## Overview

Migrate VirtIO block driver from polling-based completion to interrupt-driven completion. This is a **breaking change** that removes all legacy polling code and introduces production-quality Rust patterns for device driver synchronization.

## Current Problems

1. **Polling Deadlock**: `IrqMutex` held across `poll_used()` spin loop disables interrupts, blocking scheduler
2. **CPU Waste**: Spin-polling up to 1M iterations burns cycles during I/O
3. **Static Mut**: Unsafe `static mut` used to avoid lock-across-poll issue
4. **No Concurrency**: Single request at a time, fully serialized I/O

## Target Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     VirtioBlkDevice                             │
├─────────────────────────────────────────────────────────────────┤
│  inner: IrqMutex<DeviceInner>                                   │
│    ├── queue: Virtqueue                                         │
│    ├── caps: VirtioMmioCaps                                     │
│    ├── free_slots: BitArray<N>                                  │
│    └── ready: bool                                              │
├─────────────────────────────────────────────────────────────────┤
│  slots: [RequestSlot; N]  (N = 32, outside IrqMutex)            │
│    ├── state: AtomicU8 (FREE/SUBMITTED/DONE/ERROR)              │
│    ├── waiter: AtomicPtr<Task>                                  │
│    ├── status: AtomicU8                                         │
│    └── buffers: UnsafeCell<Option<RequestBuffers>>              │
├─────────────────────────────────────────────────────────────────┤
│  irq_line: u8                                                   │
│  vector: u8                                                     │
└─────────────────────────────────────────────────────────────────┘
```

## Design Decisions

### 1. Synchronization: Request Slots + Virtqueue Lock

**Pattern**: `IrqMutex<Inner>` for queue ops, per-slot atomics for completion

- `IrqMutex` held only during: allocate slot, write descriptors, submit, pop used ring
- Never held during wait
- Per-slot `AtomicU8` state allows IRQ handler to mark completion without big lock

### 2. Interrupt Allocation: Legacy INTx via IOAPIC

**Pattern**: Use PCI `irq_line` field, route through existing IOAPIC infrastructure

- VirtIO PCI devices report interrupt line in config space
- Map `irq_line` to GSI via IOAPIC
- Allocate vector in 0x30+ range (above legacy IRQs)
- Fallback: if `irq_line` not in 0-15, use GSI directly with higher vector

### 3. Completion Notification: Atomic Flag + Direct Wake

**Pattern**: IRQ sets atomic state, wakes blocked task directly

```rust
// IRQ Handler (simplified):
fn virtio_blk_irq_handler() {
    ack_isr();
    let mut inner = DEVICE.inner.lock();
    while let Some((slot_id, _len)) = inner.queue.pop_used() {
        let slot = &DEVICE.slots[slot_id];
        slot.state.store(DONE, Release);
    }
    drop(inner);  // Release lock before waking
    
    for slot in completed_slots {
        wake_task(slot.waiter.load(Acquire));
    }
    send_eoi();
}
```

### 4. Request Tracking: Descriptor ID = Slot ID

**Pattern**: VirtIO used ring returns descriptor head ID, which maps 1:1 to slot index

- Slot index is the descriptor chain head
- Used ring entry contains `(id, len)` - `id` is our slot index
- No searching, O(1) completion lookup

### 5. DMA Lifetime: Slot Owns Buffers Until Completion

**Pattern**: `RequestBuffers` lives inside slot, task reclaims after wake

- Slot owns `OwnedPageFrame` instances
- IRQ never touches buffer ownership
- Task `take()`s buffers after completion confirmed

---

## Files to Create

### `drivers/src/virtio/request.rs` (NEW)

Request slot management and synchronization primitives.

```rust
use core::sync::atomic::{AtomicU8, AtomicPtr, Ordering};
use core::cell::UnsafeCell;
use slopos_mm::page_alloc::OwnedPageFrame;

pub const SLOT_STATE_FREE: u8 = 0;
pub const SLOT_STATE_SUBMITTED: u8 = 1;
pub const SLOT_STATE_DONE: u8 = 2;
pub const SLOT_STATE_ERROR: u8 = 3;

pub const MAX_SLOTS: usize = 32;

pub struct RequestBuffers {
    pub req_page: OwnedPageFrame,
    pub data_page: OwnedPageFrame,
}

pub struct RequestSlot {
    state: AtomicU8,
    waiter: AtomicPtr<Task>,
    status: AtomicU8,
    buffers: UnsafeCell<Option<RequestBuffers>>,
}

// SAFETY: Slot access is synchronized via atomic state machine
unsafe impl Sync for RequestSlot {}

impl RequestSlot {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(SLOT_STATE_FREE),
            waiter: AtomicPtr::new(core::ptr::null_mut()),
            status: AtomicU8::new(0),
            buffers: UnsafeCell::new(None),
        }
    }
    
    pub fn try_acquire(&self) -> bool {
        self.state.compare_exchange(
            SLOT_STATE_FREE, 
            SLOT_STATE_SUBMITTED,
            Ordering::AcqRel,
            Ordering::Relaxed
        ).is_ok()
    }
    
    pub fn set_buffers(&self, buffers: RequestBuffers) {
        // SAFETY: Only called by task that acquired the slot
        unsafe { *self.buffers.get() = Some(buffers); }
    }
    
    pub fn take_buffers(&self) -> Option<RequestBuffers> {
        // SAFETY: Only called by task after completion confirmed
        unsafe { (*self.buffers.get()).take() }
    }
    
    pub fn set_waiter(&self, task: *mut Task) {
        self.waiter.store(task, Ordering::Release);
    }
    
    pub fn mark_done(&self, status: u8) {
        self.status.store(status, Ordering::Relaxed);
        self.state.store(SLOT_STATE_DONE, Ordering::Release);
    }
    
    pub fn mark_error(&self) {
        self.state.store(SLOT_STATE_ERROR, Ordering::Release);
    }
    
    pub fn is_done(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        state == SLOT_STATE_DONE || state == SLOT_STATE_ERROR
    }
    
    pub fn get_status(&self) -> u8 {
        self.status.load(Ordering::Acquire)
    }
    
    pub fn get_waiter(&self) -> *mut Task {
        self.waiter.load(Ordering::Acquire)
    }
    
    pub fn release(&self) {
        self.waiter.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.status.store(0, Ordering::Relaxed);
        self.state.store(SLOT_STATE_FREE, Ordering::Release);
    }
}

pub struct SlotArray {
    slots: [RequestSlot; MAX_SLOTS],
}

impl SlotArray {
    pub const fn new() -> Self {
        Self {
            slots: [const { RequestSlot::new() }; MAX_SLOTS],
        }
    }
    
    pub fn allocate(&self) -> Option<usize> {
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.try_acquire() {
                return Some(i);
            }
        }
        None
    }
    
    pub fn get(&self, index: usize) -> Option<&RequestSlot> {
        self.slots.get(index)
    }
}
```

### `drivers/src/virtio/wait.rs` (NEW)

Task blocking/waking primitives for I/O completion.

```rust
use core::ptr;
use slopos_core::scheduler::{
    scheduler_get_current_task, block_current_task, unblock_task, scheduler_is_enabled
};

pub fn wait_for_completion(slot: &RequestSlot) {
    if scheduler_is_enabled() == 0 {
        // Fallback: busy-wait if scheduler not ready
        while !slot.is_done() {
            core::hint::spin_loop();
        }
        return;
    }
    
    let current = scheduler_get_current_task() as *mut Task;
    slot.set_waiter(current);
    
    // Memory barrier: ensure waiter is visible before we check state
    core::sync::atomic::fence(Ordering::SeqCst);
    
    // Check if already done (race with IRQ)
    if slot.is_done() {
        slot.set_waiter(ptr::null_mut());
        return;
    }
    
    // Block until IRQ wakes us
    block_current_task();
    
    // Clear waiter on return
    slot.set_waiter(ptr::null_mut());
}

pub fn wake_slot_waiter(slot: &RequestSlot) {
    let waiter = slot.get_waiter();
    if !waiter.is_null() {
        unblock_task(waiter);
    }
}
```

---

## Files to Modify

### `drivers/src/virtio/queue.rs`

**Remove**:
- `poll_used()` function entirely
- All polling-related constants (`REQUEST_TIMEOUT_SPINS` if defined here)

**Add**:
- `pop_used(&mut self) -> Option<(u16, u32)>` - Returns `(descriptor_id, bytes_written)` or None

```rust
pub fn pop_used(&mut self) -> Option<(u16, u32)> {
    virtio_rmb();
    let used_idx = self.read_used_idx();
    if used_idx == self.last_used_idx {
        return None;
    }
    
    let ring_idx = (self.last_used_idx % self.size) as usize;
    let entry = self.read_used_entry(ring_idx);
    self.last_used_idx = self.last_used_idx.wrapping_add(1);
    
    Some((entry.id as u16, entry.len))
}

fn read_used_entry(&self, index: usize) -> VirtqUsedElem {
    // Read from used ring at index
    let ptr = unsafe { 
        self.used_virt.add(4 + index * 8) as *const VirtqUsedElem 
    };
    unsafe { ptr::read_volatile(ptr) }
}
```

### `drivers/src/virtio/mod.rs`

**Add**:
```rust
pub mod request;
pub mod wait;
```

### `drivers/src/virtio_blk.rs` (REWRITE)

Complete rewrite with interrupt-driven architecture:

```rust
use core::ffi::c_int;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_lib::{klog_info, IrqMutex, InitFlag};
use slopos_core::irq;
use slopos_abi::arch::x86_64::InterruptFrame;

use crate::pci::{pci_register_driver, PciDeviceInfo, PciDriver};
use crate::virtio::{
    self,
    pci::{enable_bus_master, negotiate_features, parse_capabilities, set_driver_ok, VIRTIO_VENDOR_ID},
    queue::{self, VirtqDesc, Virtqueue, DEFAULT_QUEUE_SIZE},
    request::{RequestBuffers, RequestSlot, SlotArray, MAX_SLOTS, SLOT_STATE_DONE},
    wait::{wait_for_completion, wake_slot_waiter},
    VirtioMmioCaps, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use crate::ioapic;

use slopos_mm::page_alloc::OwnedPageFrame;

const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: u64 = 512;

const VIRTIO_BLK_VECTOR: u8 = 0x30;  // First vector above legacy IRQs

#[repr(C)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

struct DeviceInner {
    queue: Virtqueue,
    caps: VirtioMmioCaps,
    capacity_sectors: u64,
    ready: bool,
}

pub struct VirtioBlkDevice {
    inner: IrqMutex<DeviceInner>,
    slots: SlotArray,
    irq_line: u8,
}

impl VirtioBlkDevice {
    const fn new() -> Self {
        Self {
            inner: IrqMutex::new(DeviceInner {
                queue: Virtqueue::new(),
                caps: VirtioMmioCaps::empty(),
                capacity_sectors: 0,
                ready: false,
            }),
            slots: SlotArray::new(),
            irq_line: 0,
        }
    }
}

// SAFETY: Device access synchronized via IrqMutex and atomic slot states
unsafe impl Sync for VirtioBlkDevice {}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static DEVICE: VirtioBlkDevice = VirtioBlkDevice::new();

extern "C" fn virtio_blk_irq_handler(_irq: u8, _frame: *mut InterruptFrame, _ctx: *mut core::ffi::c_void) {
    // Read and acknowledge ISR status
    {
        let inner = DEVICE.inner.lock();
        if inner.caps.isr_cfg.is_mapped() {
            let _isr = inner.caps.isr_cfg.read_u8(0);  // Auto-clears on read
        }
    }
    
    // Collect completed slots while holding lock
    let mut completed = [0u16; MAX_SLOTS];
    let mut completed_count = 0usize;
    
    {
        let mut inner = DEVICE.inner.lock();
        while let Some((slot_id, _len)) = inner.queue.pop_used() {
            if let Some(slot) = DEVICE.slots.get(slot_id as usize) {
                slot.mark_done(VIRTIO_BLK_S_OK);
                completed[completed_count] = slot_id;
                completed_count += 1;
            }
        }
    }
    
    // Wake waiters outside of lock
    for i in 0..completed_count {
        if let Some(slot) = DEVICE.slots.get(completed[i] as usize) {
            wake_slot_waiter(slot);
        }
    }
}

fn setup_interrupt(irq_line: u8) {
    let gsi = irq_line as u32;
    let lapic_id = crate::apic::get_id() as u8;
    
    ioapic::config_irq(
        gsi,
        VIRTIO_BLK_VECTOR,
        lapic_id,
        ioapic::IOAPIC_FLAG_DELIVERY_FIXED | ioapic::IOAPIC_FLAG_DEST_PHYSICAL,
    );
    
    irq::register_handler(
        irq_line,
        Some(virtio_blk_irq_handler),
        ptr::null_mut(),
        b"virtio-blk\0".as_ptr() as *const i8,
    );
    
    ioapic::unmask_gsi(gsi);
}

fn do_request(sector: u64, buffer: *mut u8, len: usize, write: bool) -> bool {
    // Allocate a request slot
    let slot_id = match DEVICE.slots.allocate() {
        Some(id) => id,
        None => {
            klog_info!("virtio-blk: no free slots");
            return false;
        }
    };
    
    let slot = DEVICE.slots.get(slot_id).unwrap();
    
    // Allocate DMA buffers
    let req_page = match OwnedPageFrame::alloc_zeroed() {
        Some(p) => p,
        None => {
            slot.release();
            return false;
        }
    };
    let data_page = match OwnedPageFrame::alloc_zeroed() {
        Some(p) => p,
        None => {
            slot.release();
            return false;
        }
    };
    
    // Setup request header
    let req_virt = req_page.as_mut_ptr::<u8>();
    let req_phys = req_page.phys_u64();
    let header = req_virt as *mut VirtioBlkReqHeader;
    let status_offset = size_of::<VirtioBlkReqHeader>();
    let status_phys = req_phys + status_offset as u64;
    
    let data_virt = data_page.as_mut_ptr::<u8>();
    let data_phys = data_page.phys_u64();
    
    if write {
        unsafe { ptr::copy_nonoverlapping(buffer, data_virt, len); }
    }
    
    unsafe {
        (*header).type_ = if write { VIRTIO_BLK_T_OUT } else { VIRTIO_BLK_T_IN };
        (*header).reserved = 0;
        (*header).sector = sector;
        *req_virt.add(status_offset) = 0xFF;
    }
    
    // Store buffers in slot before submitting
    slot.set_buffers(RequestBuffers { req_page, data_page });
    
    // Submit to virtqueue (brief lock)
    {
        let mut inner = DEVICE.inner.lock();
        if !inner.queue.is_ready() {
            slot.take_buffers();
            slot.release();
            return false;
        }
        
        // Write descriptors with slot_id as the head
        inner.queue.write_desc(slot_id as u16, VirtqDesc {
            addr: req_phys,
            len: size_of::<VirtioBlkReqHeader>() as u32,
            flags: VIRTQ_DESC_F_NEXT,
            next: (slot_id as u16).wrapping_add(1) % MAX_SLOTS as u16,
        });
        
        // ... (descriptor chain setup - data buffer + status byte)
        
        inner.queue.submit(slot_id as u16);
        queue::notify_queue(&inner.caps.notify_cfg, inner.caps.notify_off_multiplier, &inner.queue, 0);
    }
    // Lock released here - interrupts enabled
    
    // Wait for completion (no lock held)
    wait_for_completion(slot);
    
    // Reclaim buffers and check status
    let buffers = slot.take_buffers().unwrap();
    let status = unsafe { *buffers.req_page.as_ptr::<u8>().add(status_offset) };
    let success = status == VIRTIO_BLK_S_OK;
    
    if success && !write {
        unsafe { ptr::copy_nonoverlapping(buffers.data_page.as_ptr::<u8>(), buffer, len); }
    }
    
    slot.release();
    success
}

fn virtio_blk_probe(info: &PciDeviceInfo) -> c_int {
    if !DEVICE_CLAIMED.claim() {
        return -1;
    }
    
    klog_info!("virtio-blk: probing {:04x}:{:04x}", info.vendor_id, info.device_id);
    
    enable_bus_master(info);
    let caps = parse_capabilities(info);
    
    if !caps.has_common_cfg() {
        DEVICE_CLAIMED.reset();
        return -1;
    }
    
    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, 0);
    if !feat_result.success {
        DEVICE_CLAIMED.reset();
        return -1;
    }
    
    let queue = match queue::setup_queue(&caps.common_cfg, 0, DEFAULT_QUEUE_SIZE) {
        Some(q) => q,
        None => {
            DEVICE_CLAIMED.reset();
            return -1;
        }
    };
    
    set_driver_ok(&caps);
    
    let capacity = read_capacity(&caps);
    
    // Initialize device state
    {
        let mut inner = DEVICE.inner.lock();
        inner.queue = queue;
        inner.caps = caps;
        inner.capacity_sectors = capacity;
        inner.ready = true;
    }
    
    // Setup interrupt handling
    setup_interrupt(info.irq_line);
    
    klog_info!("virtio-blk: ready, capacity {} MB, IRQ {}", 
        (capacity * SECTOR_SIZE) / (1024 * 1024), info.irq_line);
    
    0
}

// ... rest of public API (virtio_blk_read, virtio_blk_write, etc.)
```

### `core/src/irq.rs`

**Extend** to support vectors beyond legacy IRQ range:

```rust
// Add support for higher vectors (0x30+) used by device interrupts
pub fn register_device_handler(
    vector: u8,
    handler: Option<IrqHandler>,
    context: *mut c_void,
    name: *const c_char,
) -> i32
```

### `drivers/src/ioapic.rs`

**Ensure** `config_irq` works for GSI > 23 if needed (may require multi-IOAPIC support later).

---

## Migration Steps

### Phase 1: Infrastructure (No Breaking Changes Yet)

1. Create `drivers/src/virtio/request.rs` with `RequestSlot` and `SlotArray`
2. Create `drivers/src/virtio/wait.rs` with blocking primitives
3. Add `pop_used()` to `Virtqueue` in `queue.rs`
4. Extend `core/src/irq.rs` for device vectors

### Phase 2: Interrupt Plumbing

1. Add `setup_interrupt()` function in `virtio_blk.rs`
2. Create `virtio_blk_irq_handler()` 
3. Verify IOAPIC routing works for device IRQ line
4. Test: interrupt fires on VirtIO activity

### Phase 3: Request Flow Migration

1. Rewrite `do_request()` to use slots + interrupt completion
2. Remove `poll_used()` from `queue.rs`
3. Remove all `#![allow(static_mut_refs)]`
4. Remove `REQUEST_TIMEOUT_SPINS` constant

### Phase 4: Cleanup

1. Remove any dead code flagged by compiler
2. Run `cargo clippy` and fix all warnings
3. Verify no `unsafe` blocks without `// SAFETY:` comments
4. Run full test suite

---

## Testing Strategy

### Unit Tests

- Slot allocation/release cycle
- Atomic state transitions
- Waiter registration/wakeup

### Integration Tests

- Single block read (verify data integrity)
- Single block write (verify persistence)
- Multiple sequential requests
- Concurrent requests (if supporting >1 in-flight)

### Stress Tests

- Rapid sequential I/O
- Large file read/write
- Interrupt storm handling

### Boot Test

- Full boot with ext2 mount
- Roulette → compositor transition (the original bug)

---

## Rollback Plan

If interrupt-driven approach fails:

1. Git revert to pre-migration commit
2. Keep `OwnedPageFrame` RAII (proven working)
3. Keep `static mut` for device state (known working)
4. Document why interrupt approach failed for future reference

---

## Success Criteria

- [ ] No `static mut` in VirtIO drivers
- [ ] No polling loops in I/O path
- [ ] `IrqMutex` never held during wait
- [ ] Interrupts fire and complete requests
- [ ] Boot test passes (roulette → compositor)
- [ ] All 360 existing tests pass
- [ ] No compiler warnings
- [ ] No `#[allow(dead_code)]` or similar suppressions

---

## Estimated Effort

| Phase | Effort | Risk |
|-------|--------|------|
| Infrastructure | 2-3 hours | Low |
| Interrupt Plumbing | 3-4 hours | Medium |
| Request Flow | 4-6 hours | High |
| Cleanup & Testing | 2-3 hours | Low |
| **Total** | **11-16 hours** | Medium |

---

## References

- [VirtIO Specification v1.3](https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.pdf)
- [Linux virtio_ring.c](https://github.com/torvalds/linux/blob/master/drivers/virtio/virtio_ring.c)
- [Linux virtio_blk.c](https://github.com/torvalds/linux/blob/master/drivers/block/virtio_blk.c)
- Redox OS WaitQueue pattern
- Theseus OS IrqSafeMutex pattern
