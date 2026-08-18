use core::ffi::c_int;
use slopos_ostd::lock_class;

use slopos_ostd::KVec;
use slopos_ostd::handle::{Handle, HandleError, PROCESS_VM_SLOT_BITS};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};
use slopos_ostd::panic::AbortOnUnwind;
use slopos_ostd::process::{Process, ProcessId};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{align_down, align_up, klog_debug, klog_info};

use crate::aslr;
use crate::elf::{ElfError, ElfValidator, MAX_LOAD_SEGMENTS, PF_W, ValidatedSegment};
use crate::hhdm::PhysAddrHhdm;
use crate::memory_layout_defs::DEFAULT_PROCESS_LAYOUT;
use crate::memory_layout_defs::{KERNEL_VIRTUAL_BASE, MAX_PROCESSES};
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;
use crate::tlb::TlbProcessKey;
use crate::user_mappings::{
    ostd_get_pte_flags_4kb, ostd_map_4kb_user, ostd_mark_cow_4kb, ostd_mark_range_user_4kb,
    ostd_protect_range_4kb, ostd_unmap_4kb_user, ostd_virt_to_phys_4kb,
};
use crate::vma_region::{Protection, RegionBacking, RegionPurpose, VmaMap, VmaRegion};
use slopos_abi::task::INVALID_PROCESS_ID;

/// Per-process VM slot, protected by the per-slot lock in `PROCESS_VMS`.
///
/// Exposed as an opaque marker so other crates can name [`Handle<ProcessVm>`].
pub struct ProcessVm {
    /// Owning, so a bound slot keeps its process resolvable; `None` exactly
    /// when the slot is free.
    process: Option<KArc<Process>>,
    /// Display copy of the process id, a plain scalar so the lock-free slot
    /// peek can read it. Never an identity — the generation decides that.
    process_id: u32,
    /// Generation half of the slot's [`Handle`], copied from the bound
    /// process so a handle minted for a previous occupant fails to resolve.
    generation: u64,
    /// `None` only between `reset()` and the next `create_process_vm`
    /// re-init; every live process has one.
    vm_space: Option<KArc<VmSpace>>,
    vma_map: VmaMap,
    code_start: u64,
    data_start: u64,
    heap_start: u64,
    /// Mapped heap extent end: always `heap_break` rounded up to a page.
    heap_end: u64,
    /// Byte-granular program break, in Linux `brk` semantics.
    heap_break: u64,
    stack_start: u64,
    stack_end: u64,
    flags: u32,
}

impl ProcessVm {
    const fn new() -> Self {
        Self {
            process: None,
            process_id: INVALID_PROCESS_ID,
            generation: 0,
            vm_space: None,
            vma_map: VmaMap::new(),
            code_start: 0,
            data_start: 0,
            heap_start: 0,
            heap_end: 0,
            heap_break: 0,
            stack_start: 0,
            stack_end: 0,
            flags: 0,
        }
    }

    fn reset(&mut self) {
        self.process = None;
        self.process_id = INVALID_PROCESS_ID;
        self.generation = 0;
        self.vm_space = None;
        self.vma_map.clear();
        self.code_start = 0;
        self.data_start = 0;
        self.heap_start = 0;
        self.heap_end = 0;
        self.heap_break = 0;
        self.stack_start = 0;
        self.stack_end = 0;
        self.flags = 0;
    }
}

/// The slot index is not allocated here — it *is* the process's registry slot,
/// so `Handle<ProcessVm>` and `Handle<Process>` agree by construction.
struct VmReservation {
    slot: usize,
    process_id: u32,
    generation: u64,
    process: KArc<Process>,
}

impl VmReservation {
    /// `None` when the process carries no handle or its slot is already bound;
    /// the latter is a caller bug — a process gets exactly one address space.
    fn claim(process: KArc<Process>) -> Option<Self> {
        let handle = process.handle()?;
        let slot = handle.slot() as usize;
        if slot >= MAX_PROCESSES {
            return None;
        }
        {
            let guard = PROCESS_VMS[slot].lock();
            if guard.process_id != INVALID_PROCESS_ID {
                klog_info!(
                    "process_vm: slot {} is already bound to process {}",
                    slot,
                    guard.process_id
                );
                return None;
            }
        }
        Some(Self {
            slot,
            process_id: process.id(),
            generation: handle.generation(),
            process,
        })
    }
}

fn count_bound_slots() -> u32 {
    (0..MAX_PROCESSES)
        .filter(|&i| slot_pid_lock_free(&PROCESS_VMS[i]) != INVALID_PROCESS_ID)
        .count() as u32
}

/// A live process address space, named both ways: `process_id` is what the
/// rest of the kernel keys on; `handle` resolves it without a pid scan.
#[derive(Clone, Copy)]
pub struct ProcessVmRef {
    pub process_id: u32,
    pub handle: Handle<ProcessVm>,
}

/// Independently lockable, so unrelated processes never contend.
static PROCESS_VMS: [SpinLock<ProcessVm>; MAX_PROCESSES] = {
    const INIT: SpinLock<ProcessVm> = SpinLock::new(
        ProcessVm::new(),
        lock_class!("PROCESS_VMS", LOCK_LEVEL_RESOURCE),
    );
    [INIT; MAX_PROCESSES]
};

fn vma_range_valid(start: u64, end: u64) -> bool {
    start < end && (start & (PAGE_SIZE_4KB - 1)) == 0 && (end & (PAGE_SIZE_4KB - 1)) == 0
}

/// Maps `[start_addr, end_addr)`. On failure the range is rolled back, so
/// `Err` always means nothing was left mapped.
fn map_user_range(
    vm_space: &mut KArc<VmSpace>,
    start_addr: u64,
    end_addr: u64,
    map_flags: u64,
) -> Result<u32, c_int> {
    if (start_addr & (PAGE_SIZE_4KB - 1)) != 0
        || (end_addr & (PAGE_SIZE_4KB - 1)) != 0
        || end_addr <= start_addr
    {
        klog_info!("map_user_range: Unaligned or invalid range");
        return Err(-1);
    }

    let mut current = start_addr;
    let mut mapped: u32 = 0;

    while current < end_addr {
        let phys = alloc_kernel_page();
        if phys.is_null() {
            klog_info!("map_user_range: Physical allocation failed");
            if let Err(err) = rollback_range(vm_space, current, start_addr, &mut mapped) {
                klog_info!("map_user_range: rollback failed: {:?}", err);
            }
            return Err(-1);
        }
        if let Err(err) = ostd_map_4kb_user(vm_space, VirtAddr::new(current), phys, map_flags) {
            klog_info!("map_user_range: OSTD cursor map failed: {:?}", err);
            free_page_frame(phys);
            if let Err(rollback_err) = rollback_range(vm_space, current, start_addr, &mut mapped) {
                klog_info!("map_user_range: rollback failed: {:?}", rollback_err);
            }
            return Err(-1);
        }
        mapped += 1;
        current += PAGE_SIZE_4KB;
    }

    Ok(mapped)
}

/// Caller contract: `virt` is a fresh resolution of a 4 KiB user-mapped
/// frame's physical address, and the user-space `VmSpace` cursor pins the
/// underlying page for the duration of the call.
#[inline]
fn hhdm_write_bytes(virt: VirtAddr, offset: usize, src: &[u8]) -> bool {
    slopos_ostd::mm::hhdm_bytes::write_bytes(virt, offset, src)
}

/// Same caller contract as [`hhdm_write_bytes`].
#[inline]
fn hhdm_read_bytes(virt: VirtAddr, offset: usize, dst: &mut [u8]) -> bool {
    slopos_ostd::mm::hhdm_bytes::read_bytes(virt, offset, dst)
}

/// Same caller contract as [`hhdm_write_bytes`].
#[inline]
fn hhdm_fill_bytes(virt: VirtAddr, offset: usize, len: usize, value: u8) -> bool {
    slopos_ostd::mm::hhdm_bytes::fill_bytes(virt, offset, len, value)
}

fn rollback_range(
    vm_space: &mut KArc<VmSpace>,
    mut current: u64,
    start_addr: u64,
    mapped: &mut u32,
) -> Result<(), MapError> {
    while *mapped > 0 {
        current -= PAGE_SIZE_4KB;
        ostd_unmap_4kb_user(vm_space, VirtAddr::new(current))?;
        *mapped -= 1;
    }
    let _ = start_addr;
    Ok(())
}

fn unmap_user_range(
    vm_space: &mut KArc<VmSpace>,
    start_addr: u64,
    end_addr: u64,
) -> Result<u32, MapError> {
    if end_addr <= start_addr {
        return Ok(0);
    }
    let mut addr = start_addr;
    let mut unmapped = 0u32;
    while addr < end_addr {
        if ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr))? {
            unmapped += 1;
        }
        addr += PAGE_SIZE_4KB;
    }
    Ok(unmapped)
}

/// The field must be naturally aligned, per `SpinLock::read_atomic_field`.
#[inline]
fn slot_read_lock_free<R>(slot: &SpinLock<ProcessVm>, f: impl FnOnce(&ProcessVm) -> R) -> R {
    slot.read_atomic_field(f)
}

#[inline]
fn slot_pid_lock_free(slot: &SpinLock<ProcessVm>) -> u32 {
    slot_read_lock_free(slot, |inner| inner.process_id)
}

fn find_slot_for_pid(process: ProcessId) -> Option<usize> {
    slot_for_handle(process.handle())
}

/// The slot `handle` names, or `None` if it has been rebound since — never a
/// stranger's address space.
fn slot_for_handle(handle: Handle<Process>) -> Option<usize> {
    let slot = handle.slot() as usize;
    if slot >= MAX_PROCESSES {
        return None;
    }
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id == INVALID_PROCESS_ID || guard.generation != handle.generation() {
        return None;
    }
    Some(slot)
}

/// Every slot index comes from the table, so the conversion cannot fail.
#[inline]
fn slot_tlb_key(slot: usize) -> TlbProcessKey {
    TlbProcessKey::from_slot(slot as u32).expect("a VM slot index is a valid shootdown key")
}

/// The generation-checked handle for `process`'s VM slot, if bound.
pub fn process_vm_handle(process: ProcessId) -> Option<Handle<ProcessVm>> {
    let slot = find_slot_for_pid(process)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return None;
    }
    Some(Handle::from_parts(slot as u32, guard.generation))
}

/// A rebound slot resolves to [`HandleError::Stale`]; an unbound slot to
/// [`HandleError::NoEntry`]; an out-of-range slot to
/// [`HandleError::OutOfBounds`].
pub fn process_vm_with_handle<R>(
    handle: Handle<ProcessVm>,
    f: impl FnOnce(&mut ProcessVm) -> R,
) -> Result<R, HandleError> {
    let slot = handle.slot() as usize;
    if slot >= MAX_PROCESSES {
        return Err(HandleError::OutOfBounds);
    }
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id == INVALID_PROCESS_ID {
        return Err(HandleError::NoEntry);
    }
    if guard.generation != handle.generation() {
        return Err(HandleError::Stale);
    }
    Ok(f(&mut guard))
}

/// The shape every caller should reach for: a process whose slot has been
/// rebound answers `Stale` instead of handing back a stranger's page tables.
pub fn process_vm_with_process<R>(
    process: Handle<Process>,
    f: impl FnOnce(&mut ProcessVm) -> R,
) -> Result<R, HandleError> {
    let slot = slot_for_handle(process).ok_or(HandleError::Stale)?;
    let mut guard = PROCESS_VMS[slot].lock();
    // Re-checked under the lock: `slot_for_handle` released it between the
    // resolution and this acquisition, and a teardown can land in that window.
    if guard.process_id == INVALID_PROCESS_ID {
        return Err(HandleError::NoEntry);
    }
    if guard.generation != process.generation() {
        return Err(HandleError::Stale);
    }
    Ok(f(&mut guard))
}

/// The address-space handle for `process`, if its slot is still bound to it.
/// Both handles name the same slot at the same generation.
pub fn process_vm_handle_for(process: Handle<Process>) -> Option<Handle<ProcessVm>> {
    let slot = slot_for_handle(process)?;
    Some(Handle::from_parts(slot as u32, process.generation()))
}

/// Pack a process-VM handle into the single word a task carries. The slot is
/// stored **biased by one**, so slot 0 at generation 0 does not collide with
/// "no address space". Matches `slopos_ostd::process::pack_process_handle`.
pub fn pack_process_vm_handle(handle: Handle<ProcessVm>) -> u64 {
    Handle::<ProcessVm>::from_parts(handle.slot() + 1, handle.generation())
        .pack(PROCESS_VM_SLOT_BITS) as u64
}

/// Inverse of [`pack_process_vm_handle`]. Zero is "no address space".
pub fn unpack_process_vm_handle(packed: u64) -> Option<Handle<ProcessVm>> {
    if packed == 0 {
        return None;
    }
    let biased = Handle::<ProcessVm>::unpack(packed as usize, PROCESS_VM_SLOT_BITS);
    // A packed word whose slot field is 0 did not come from the packer, which
    // biases every real slot; refuse it rather than unbias to `u32::MAX`.
    let slot = biased.slot().checked_sub(1)?;
    Some(Handle::from_parts(slot, biased.generation()))
}

/// Install the address space named by `handle` as the current CPU's CR3.
/// `Ok(false)` means the slot holds no address space; the caller falls back
/// to the kernel master.
pub fn process_vm_activate_by_handle(handle: Handle<ProcessVm>) -> Result<bool, HandleError> {
    process_vm_with_handle(handle, |proc| {
        let Some(vm_space) = proc.vm_space.as_ref() else {
            return false;
        };
        vm_space.activate_at_context_switch();
        true
    })
}

/// The PML4 physical address of the address space named by `handle`, or
/// `Ok(0)` if the slot holds none.
pub fn process_vm_get_cr3_phys_by_handle(handle: Handle<ProcessVm>) -> Result<u64, HandleError> {
    process_vm_with_handle(handle, |proc| {
        proc.vm_space
            .as_ref()
            .map_or(0, |vm_space| vm_space.pml4_paddr().as_u64())
    })
}

/// Runs `f` under the per-process lock; a rebound slot resolves to
/// [`HandleError::Stale`] rather than to its current occupant.
pub fn process_vm_with_vm_space_by_handle<R>(
    handle: Handle<ProcessVm>,
    f: impl FnOnce(&mut KArc<VmSpace>) -> R,
) -> Result<R, HandleError> {
    process_vm_with_handle(handle, |proc| proc.vm_space.as_mut().map(f))?
        .ok_or(HandleError::NoEntry)
}

/// Like [`process_vm_with_vm_space_by_handle`] but also resolves the
/// covering [`VmaRegion`] for `fault_addr` under the same lock, so the
/// demand-fault path decides and acts in one acquisition.
pub fn process_vm_with_vm_space_and_region_by_handle<R>(
    handle: Handle<ProcessVm>,
    fault_addr: u64,
    f: impl FnOnce(&mut KArc<VmSpace>, VmaRegion) -> R,
) -> Result<R, HandleError> {
    process_vm_with_handle(handle, |proc| {
        let region = {
            let (_rs, _re, region_ref) = proc.vma_map.find_containing(fault_addr)?;
            region_ref.clone()
        };
        let vm_space = proc.vm_space.as_mut()?;
        Some(f(vm_space, region))
    })?
    .ok_or(HandleError::NoEntry)
}

/// Translate a user virtual address to its backing physical address for
/// `process`. Returns 0 if the slot is unbound, `vm_space` is missing, or no
/// 4 KiB leaf is mapped; the returned paddr includes `va`'s page offset.
pub fn process_vm_user_va_to_paddr(process: ProcessId, va: u64) -> u64 {
    let Some(slot) = find_slot_for_pid(process) else {
        return 0;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return 0;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return 0;
    };
    crate::user_mappings::ostd_virt_to_phys_4kb(vm_space, slopos_abi::addr::VirtAddr::new(va))
        .as_u64()
}

/// A clone rather than a borrow through the per-slot lock, so `user_copy`'s
/// walk runs with that lock released — otherwise it orders against the
/// page-fault recovery path. `None` if the slot is unbound.
pub fn process_vm_get_vm_space(
    process: ProcessId,
) -> Option<slopos_ostd::KArc<slopos_ostd::mm::vm_space::VmSpace>> {
    let slot = find_slot_for_pid(process)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return None;
    }
    guard.vm_space.as_ref().cloned()
}

/// Is `va` mapped AND user-accessible in `process`'s address space?
/// Kernel-half pages return `false`.
pub fn process_vm_user_va_is_user_accessible(process: ProcessId, va: u64) -> bool {
    let Some(slot) = find_slot_for_pid(process) else {
        return false;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return false;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return false;
    };
    crate::user_mappings::ostd_is_user_accessible_4kb(vm_space, slopos_abi::addr::VirtAddr::new(va))
}

/// Read the `VmSpace`'s PML4 paddr for `process`; 0 means "no VM". Once
/// [`VmSpace::activate`] has written CR3 this matches the hardware CR3, which
/// is what the user-fault dispatcher compares against.
pub fn process_vm_get_ostd_pml4_paddr(process: ProcessId) -> u64 {
    let Some(slot) = find_slot_for_pid(process) else {
        return 0;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return 0;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return 0;
    };
    vm_space.pml4_paddr().as_u64()
}

/// Install `process`'s `VmSpace` as the current CPU's CR3. `false` if the slot
/// is unbound or has no `vm_space` — the caller falls back to
/// `kernel_vm_space().lock().activate()`.
///
/// The scheduler upholds `VmSpace::activate`'s context-switch contract (IRQs
/// disabled, on this CPU, kernel half preserved).
pub fn process_vm_activate(process: ProcessId) -> bool {
    let Some(slot) = find_slot_for_pid(process) else {
        return false;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return false;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return false;
    };
    vm_space.activate_at_context_switch();
    true
}

/// Run `f` under the per-process lock with mutable access to `process`'s
/// `KArc<VmSpace>`. `None` if the slot is unbound or has no `vm_space`.
/// The closure runs with the lock held — keep the body fast.
pub fn process_vm_with_vm_space<R>(
    process: ProcessId,
    f: impl FnOnce(&mut KArc<VmSpace>) -> R,
) -> Option<R> {
    let slot = find_slot_for_pid(process)?;
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return None;
    }
    let vm_space = guard.vm_space.as_mut()?;
    Some(f(vm_space))
}

/// Like [`process_vm_with_vm_space`] but also resolves the covering
/// [`VmaRegion`] for `fault_addr` under the same lock: dropping and
/// re-acquiring it would deadlock the recursive demand-fault path.
pub fn process_vm_with_vm_space_and_region<R>(
    process: ProcessId,
    fault_addr: u64,
    f: impl FnOnce(&mut KArc<VmSpace>, VmaRegion) -> R,
) -> Option<R> {
    let slot = find_slot_for_pid(process)?;
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return None;
    }
    let region = {
        let (_rs, _re, region_ref) = guard.vma_map.find_containing(fault_addr)?;
        region_ref.clone()
    };
    let vm_space = guard.vm_space.as_mut()?;
    Some(f(vm_space, region))
}

/// Read the PML4 physical address for a process. `0` means "no VM", on which
/// the scheduler refuses to dispatch.
pub fn process_vm_get_cr3_phys(process: ProcessId) -> u64 {
    process_vm_get_ostd_pml4_paddr(process)
}

/// The stable 64-bit `MmContextId` for this process, or
/// `MmContextId::INVALID` if the slot is freed or has no address space. The
/// scheduler keys the per-CPU ASID cache on it, so PCID reuse survives id
/// recycling.
pub fn process_vm_get_mm_ctx_id(process: ProcessId) -> crate::mmu::MmContextId {
    let Some(slot) = find_slot_for_pid(process) else {
        return crate::mmu::MmContextId::INVALID;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return crate::mmu::MmContextId::INVALID;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return crate::mmu::MmContextId::INVALID;
    };
    crate::mmu::MmContextId::from_raw(vm_space.mm_ctx_handle())
}

pub fn process_vm_find_pid_by_cr3(cr3: u64) -> u32 {
    let cr3_phys = cr3 & !0xFFF;
    if cr3_phys == 0 {
        return INVALID_PROCESS_ID;
    }

    for i in 0..MAX_PROCESSES {
        let pid = slot_pid_lock_free(&PROCESS_VMS[i]);
        if pid == INVALID_PROCESS_ID {
            continue;
        }
        let guard = PROCESS_VMS[i].lock();
        if guard.process_id != pid {
            continue;
        }
        if let Some(vm_space) = guard.vm_space.as_ref() {
            if vm_space.pml4_paddr().as_u64() == cr3_phys {
                return pid;
            }
        }
    }

    INVALID_PROCESS_ID
}

/// Caller guarantees the range does not overlap an existing VMA — for
/// non-MAP_FIXED mmaps the gap finder provides that by construction.
fn add_vma_to_inner(inner: &mut ProcessVm, start: u64, end: u64, region: VmaRegion) -> c_int {
    if !vma_range_valid(start, end) {
        return -1;
    }
    if inner.vma_map.insert(start, end, region).is_err() {
        return -1;
    }
    0
}

fn prot_to_region(prot: u64) -> VmaRegion {
    use slopos_abi::syscall::{PROT_EXEC, PROT_READ, PROT_WRITE};
    VmaRegion {
        protection: Protection {
            read: prot & PROT_READ != 0,
            write: prot & PROT_WRITE != 0,
            exec: prot & PROT_EXEC != 0,
        },
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: true,
        purpose: RegionPurpose::General,
    }
}

fn unmap_and_free_range_inner(
    inner: &mut ProcessVm,
    start: u64,
    end: u64,
) -> Result<u32, MapError> {
    if !vma_range_valid(start, end) {
        return Ok(0);
    }
    let vm_space = inner
        .vm_space
        .as_mut()
        .expect("unmap_and_free_range_inner: vm_space present for live process");
    let mut freed = 0u32;
    let mut addr = start;
    while addr < end {
        if ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr))? {
            freed += 1;
        }
        addr += PAGE_SIZE_4KB;
    }
    Ok(freed)
}

/// The caller drops the slot's `KArc<VmSpace>`; the shootdown issued here is
/// what makes the frames that drop releases safe to reuse.
fn teardown_inner_mappings(inner: &mut ProcessVm, key: TlbProcessKey) {
    tlb::flush_all_for_process(key);
    inner.vma_map.drain(|start, end, region| {
        dec_removed_shared_mapcount(start, end, region);
    });
    inner.heap_end = inner.heap_start;
    inner.heap_break = inner.heap_start;
}

/// Unmap a range; each unmapped `UFrame` returns its buddy frame on drop.
fn unmap_and_free_range_dir(
    vm_space: &mut KArc<VmSpace>,
    start: u64,
    end: u64,
) -> Result<u64, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(start);
    }
    let mut addr = start;
    while addr < end {
        match ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) | Ok(false) => {}
            Err(err) => return Err(unmap_region_error(err, addr)),
        }
        addr += PAGE_SIZE_4KB;
    }
    Ok(end)
}

/// Unmap a SlopRing mapping range. Each page's PTE holds its own ref on the
/// `RingMeta` frame, so dropping it here leaves the frame alive for as long as
/// the ring object still holds its own — neither a free nor a nofree unmap.
fn unmap_ring_range_dir(
    vm_space: &mut KArc<VmSpace>,
    key: TlbProcessKey,
    start: u64,
    end: u64,
) -> Result<u64, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(start);
    }
    let mut unmapped = 0u32;
    let mut addr = start;
    while addr < end {
        match crate::user_mappings::ostd_unmap_ring_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) => unmapped += 1,
            Ok(false) => {}
            Err(err) => {
                if unmapped > 0 {
                    tlb::flush_all_for_process(key);
                }
                return Err(unmap_region_error(err, addr));
            }
        }
        addr += PAGE_SIZE_4KB;
    }
    // The cursor-unmap issues only a local INVLPG, and a ring region is
    // routinely re-created at the same VA, so a migrated task could read the
    // prior ring's stale translation without a process-wide shootdown.
    if unmapped > 0 {
        tlb::flush_all_for_process(key);
    }
    Ok(end)
}

/// Unmap shared-memfd pages. Each unmap drops only this mapping's MetaSlot
/// ref; the memfd object holds its own, so the page returns to the buddy
/// exactly once — when the last of {memfd ref, every mapping} drops.
fn unmap_range_nofree_dir(
    vm_space: &mut KArc<VmSpace>,
    key: TlbProcessKey,
    start: u64,
    end: u64,
) -> Result<u64, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(start);
    }
    let mut unmapped = 0u32;
    let mut addr = start;
    while addr < end {
        match ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) => unmapped += 1,
            Ok(false) => {}
            Err(err) => {
                if unmapped > 0 {
                    tlb::flush_all_for_process(key);
                }
                return Err(unmap_region_error(err, addr));
            }
        }
        addr += PAGE_SIZE_4KB;
    }
    if unmapped > 0 {
        tlb::flush_all_for_process(key);
    }
    Ok(end)
}

type VmaOverlap = (u64, u64, VmaRegion);

/// A failed range unmap, and the address it got to; the caller drops VMA
/// metadata for exactly the prefix `processed_end` names.
struct UnmapRegionError {
    err: MapError,
    processed_end: u64,
}

fn unmap_region_error(err: MapError, processed_end: u64) -> UnmapRegionError {
    UnmapRegionError { err, processed_end }
}

fn vma_page_count(start: u64, end: u64) -> u32 {
    ((end - start) / PAGE_SIZE_4KB) as u32
}

fn dec_removed_shared_mapcount(start: u64, end: u64, region: &VmaRegion) {
    if let Some(handle) = region.memfd_handle() {
        crate::memfd::memfd_dec_mapcount_by(handle, vma_page_count(start, end));
    }
}

fn collect_overlapping_vmas(
    inner: &ProcessVm,
    start: u64,
    end: u64,
) -> Result<KVec<VmaOverlap>, ()> {
    KVec::from_iter_fallible(
        inner
            .vma_map
            .iter()
            .filter(move |(s, e, _)| *s < end && *e > start)
            .map(move |(s, e, region)| (s.max(start), e.min(end), region.clone())),
    )
    .map_err(|_| ())
}

fn unmap_region_range_dir(
    vm_space: &mut KArc<VmSpace>,
    key: TlbProcessKey,
    start: u64,
    end: u64,
    region: &VmaRegion,
) -> Result<u64, UnmapRegionError> {
    if region.is_ring() {
        unmap_ring_range_dir(vm_space, key, start, end)
    } else if region.is_shared() {
        unmap_range_nofree_dir(vm_space, key, start, end)
    } else {
        unmap_and_free_range_dir(vm_space, start, end)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Ehdr {
    ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

const SHT_RELA: u32 = 4;

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;

/// Unaligned read; `None` if it would extend past the slice.
#[inline]
fn read_elf_pod<T: Copy>(payload: &[u8], offset: usize) -> Option<T> {
    slopos_ostd::util::ptr_buf::read_pod_at::<T>(payload, offset)
}

fn apply_elf_relocations(
    payload_slice: &[u8],
    vm_space: &KArc<VmSpace>,
    section_mappings: &[(u64, u64, u64)], // (kernel_va_start, kernel_va_end, user_va_start)
) -> c_int {
    if payload_slice.is_empty() {
        return -1;
    }
    let payload_len = payload_slice.len();

    let ehdr: Elf64Ehdr = match read_elf_pod::<Elf64Ehdr>(payload_slice, 0) {
        Some(h) => h,
        None => return -1,
    };
    if &ehdr.ident[0..4] != b"\x7fELF" || ehdr.e_shoff == 0 || ehdr.e_shnum == 0 {
        return -1;
    }

    let sh_size = ehdr.e_shentsize as usize;
    let sh_num = ehdr.e_shnum as usize;
    let sh_off = ehdr.e_shoff as usize;
    let shstrndx = ehdr.e_shstrndx as usize;

    if sh_off + sh_num * sh_size > payload_len || shstrndx >= sh_num {
        return -1;
    }

    let shstrtab_shdr = match read_elf_pod::<Elf64Shdr>(payload_slice, sh_off + shstrndx * sh_size)
    {
        Some(s) => s,
        None => return -1,
    };
    let shstrtab_base = shstrtab_shdr.sh_offset as usize;
    let shstrtab_size = shstrtab_shdr.sh_size as usize;
    if shstrtab_base + shstrtab_size > payload_len {
        return -1;
    }

    let get_section_name = |sh_name_off: u32| -> Option<&[u8]> {
        let off = shstrtab_base + sh_name_off as usize;
        if off >= payload_len {
            return None;
        }
        let max = payload_len - off;
        let bytes = &payload_slice[off..off + max];
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(max);
        Some(&bytes[..len])
    };

    let map_kernel_va_to_user = |kernel_va: u64| -> Option<u64> {
        for &(kern_start, kern_end, user_start) in section_mappings {
            if kernel_va >= kern_start && kernel_va < kern_end {
                return Some(user_start + (kernel_va - kern_start));
            }
        }
        None
    };

    for i in 0..sh_num {
        let shdr = match read_elf_pod::<Elf64Shdr>(payload_slice, sh_off + i * sh_size) {
            Some(s) => s,
            None => continue,
        };
        if shdr.sh_type != SHT_RELA {
            continue;
        }

        let name_off = shdr.sh_name;
        let Some(name) = get_section_name(name_off) else {
            continue;
        };

        if !name.starts_with(b".rela.") {
            continue;
        }

        let target_section_idx = shdr.sh_info as usize;
        if target_section_idx >= sh_num {
            continue;
        }
        let target_shdr =
            match read_elf_pod::<Elf64Shdr>(payload_slice, sh_off + target_section_idx * sh_size) {
                Some(s) => s,
                None => continue,
            };

        let target_kern_va = target_shdr.sh_addr;
        let Some(target_user_va_base) = map_kernel_va_to_user(target_kern_va) else {
            continue;
        };

        let rela_base = shdr.sh_offset as usize;
        let rela_size = shdr.sh_size as usize;
        let rela_entsize = if shdr.sh_entsize != 0 {
            shdr.sh_entsize as usize
        } else {
            core::mem::size_of::<Elf64Rela>()
        };

        if rela_base + rela_size > payload_len {
            continue;
        }

        let num_relocs = rela_size / rela_entsize;
        for j in 0..num_relocs {
            let rela = match read_elf_pod::<Elf64Rela>(payload_slice, rela_base + j * rela_entsize)
            {
                Some(r) => r,
                None => continue,
            };

            let reloc_type = (rela.r_info & 0xffffffff) as u32;

            let reloc_kern_addr = rela.r_offset;
            let reloc_user_addr = if reloc_kern_addr >= target_kern_va {
                target_user_va_base + (reloc_kern_addr - target_kern_va)
            } else {
                target_user_va_base.wrapping_add(rela.r_offset)
            };

            // An unaligned access here can straddle a 4 KiB boundary; it
            // resolves only because `load_segment_pages` maps sequentially
            // allocated — hence physically contiguous — buddy frames. The
            // page-walking alternative needs no such invariant but pushes ELF
            // load past the NMI watchdog budget under TCG.
            let symbol_va = match reloc_type {
                R_X86_64_PC32 | 4 => {
                    // 4 = R_X86_64_PLT32.
                    let read_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
                    let read_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;
                    let read_phys = ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(read_page_va));
                    if read_phys.is_null() {
                        continue;
                    }
                    let read_virt = read_phys.to_virt();
                    if read_virt.is_null() {
                        continue;
                    }
                    let current_offset = match slopos_ostd::mm::hhdm_bytes::read_unaligned::<i32>(
                        read_virt,
                        read_page_off,
                    ) {
                        Some(v) => v as i64,
                        None => continue,
                    };
                    // S = offset + P - A, where P is the rip after the rel32.
                    let original_kernel_rip_after = reloc_kern_addr.wrapping_add(4);
                    (original_kernel_rip_after as i64)
                        .wrapping_add(current_offset)
                        .wrapping_sub(rela.r_addend) as u64
                }
                _ => {
                    if rela.r_addend != 0 {
                        rela.r_addend as u64
                    } else {
                        let read_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
                        let read_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;
                        let read_phys =
                            ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(read_page_va));
                        if read_phys.is_null() {
                            continue;
                        }
                        let read_virt = read_phys.to_virt();
                        if read_virt.is_null() {
                            continue;
                        }
                        match reloc_type {
                            R_X86_64_64 => {
                                match slopos_ostd::mm::hhdm_bytes::read_unaligned::<u64>(
                                    read_virt,
                                    read_page_off,
                                ) {
                                    Some(v) => v,
                                    None => continue,
                                }
                            }
                            R_X86_64_32 | R_X86_64_32S => {
                                let val = match slopos_ostd::mm::hhdm_bytes::read_unaligned::<u32>(
                                    read_virt,
                                    read_page_off,
                                ) {
                                    Some(v) => v as u64,
                                    None => continue,
                                };
                                if reloc_type == R_X86_64_32S {
                                    (val as i32 as i64) as u64
                                } else {
                                    val
                                }
                            }
                            _ => continue,
                        }
                    }
                }
            };

            let Some(user_symbol_va) = map_kernel_va_to_user(symbol_va) else {
                continue;
            };

            let reloc_page_va = reloc_user_addr & !(PAGE_SIZE_4KB - 1);
            let reloc_page_off = (reloc_user_addr & (PAGE_SIZE_4KB - 1)) as usize;
            let reloc_phys = ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(reloc_page_va));
            if reloc_phys.is_null() {
                continue;
            }
            let reloc_virt = reloc_phys.to_virt();
            if reloc_virt.is_null() {
                continue;
            }
            match reloc_type {
                R_X86_64_64 => {
                    let _ = slopos_ostd::mm::hhdm_bytes::write_unaligned::<u64>(
                        reloc_virt,
                        reloc_page_off,
                        user_symbol_va,
                    );
                }
                R_X86_64_PC32 | 4 => {
                    let rip_after = reloc_user_addr + 4;
                    let offset = (user_symbol_va as i64 - rip_after as i64) as i32;
                    let _ = slopos_ostd::mm::hhdm_bytes::write_unaligned::<i32>(
                        reloc_virt,
                        reloc_page_off,
                        offset,
                    );
                }
                R_X86_64_32 | R_X86_64_32S => {
                    let _ = slopos_ostd::mm::hhdm_bytes::write_unaligned::<u32>(
                        reloc_virt,
                        reloc_page_off,
                        user_symbol_va as u32,
                    );
                }
                _ => continue,
            }
        }
    }

    0
}

pub fn process_vm_load_elf_data(
    process: ProcessId,
    data: &[u8],
    entry_out: &mut u64,
) -> Result<crate::elf::ElfExecInfo, ElfError> {
    let code_base = crate::memory_layout_defs::PROCESS_CODE_START_VA;

    let validator = ElfValidator::new(data)?.with_load_base(code_base);

    if validator.has_interpreter()? {
        return Err(ElfError::DynamicNotSupported);
    }

    let mut segments_store =
        slopos_ostd::KVec::<crate::elf::ValidatedSegment>::zeroed(crate::elf::MAX_LOAD_SEGMENTS)
            .map_err(|_| ElfError::NullPointer)?;
    let segment_count = validator.validate_load_segments_into(segments_store.as_mut_slice())?;

    let slot = find_slot_for_pid(process).ok_or(ElfError::NullPointer)?;

    let info = load_segments_and_tls(
        &validator,
        data,
        code_base,
        slot,
        process,
        &segments_store.as_slice()[..segment_count],
    )?;
    *entry_out = info.entry;
    Ok(info)
}

/// Out of line so its locked slot, section-mapping table and nine-field
/// return value stay out of the caller's frame, which is measured against the
/// 2 KiB stack gate.
#[inline(never)]
fn load_segments_and_tls(
    validator: &ElfValidator<'_>,
    data: &[u8],
    code_base: u64,
    slot: usize,
    process: ProcessId,
    segments: &[crate::elf::ValidatedSegment],
) -> Result<crate::elf::ElfExecInfo, ElfError> {
    let header = validator.header();

    // TLS geometry is recorded for diagnostics only: libc discovers PT_TLS via
    // AT_PHDR and owns all TLS construction, main thread and spawned alike.
    let tls_segment = validator.find_tls_segment()?;
    let (tls_vaddr, tls_filesz, tls_memsz, tls_align) = match tls_segment {
        Some((vaddr, filesz, memsz, align)) => (vaddr, filesz, memsz, align),
        None => (0, 0, 0, 0),
    };

    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return Err(ElfError::NullPointer);
    }
    if guard.vm_space.is_none() {
        return Err(ElfError::NullPointer);
    }

    let (min_vaddr, needs_reloc) = calculate_load_offset(segments, code_base);

    {
        let vm_space_ref = guard
            .vm_space
            .as_mut()
            .expect("load_segments_and_tls: vm_space present for live pid");
        unmap_existing_code_region(vm_space_ref, code_base).map_err(|_| ElfError::NullPointer)?;
    }

    let mut section_mappings = slopos_ostd::KVec::<(u64, u64, u64)>::zeroed(MAX_LOAD_SEGMENTS)
        .map_err(|_| ElfError::NullPointer)?;
    let mut mapping_count = 0usize;
    let mut mapped_pages: u32 = 0;

    for segment in segments.iter() {
        let user_start =
            process_vm_translate_elf_address(segment.original_vaddr, min_vaddr, code_base);
        let user_end = process_vm_translate_elf_address(
            segment.original_vaddr + segment.mem_size,
            min_vaddr,
            code_base,
        );

        if mapping_count < section_mappings.len() {
            section_mappings[mapping_count] = (
                segment.original_vaddr,
                segment.original_vaddr + segment.mem_size,
                user_start,
            );
            mapping_count += 1;
        }

        let vm_space_ref = guard
            .vm_space
            .as_mut()
            .expect("load_segments_and_tls: vm_space present per segment");
        let pages = load_segment_pages(vm_space_ref, data, segment, user_start, user_end)?;
        mapped_pages = mapped_pages.saturating_add(pages);
    }

    let tls_tp = 0u64;

    if needs_reloc {
        let vm_space_ref = guard
            .vm_space
            .as_ref()
            .expect("apply_elf_relocations: vm_space present for live pid");
        let _ = apply_elf_relocations(data, vm_space_ref, &section_mappings[..mapping_count]);
    }

    let user_entry = process_vm_translate_elf_address(header.e_entry, min_vaddr, code_base);
    let phdr_user_addr = compute_phdr_user_addr(header, segments, min_vaddr, code_base);
    // Zero means the linker left the phdrs out of every PT_LOAD. libc walks
    // AT_PHDR to find PT_TLS, so refuse the exec rather than ship a process
    // that faults on its first thread-local access.
    if phdr_user_addr == 0 {
        return Err(ElfError::InvalidPhdrOffset);
    }

    drop(guard);

    Ok(crate::elf::ElfExecInfo {
        entry: user_entry,
        phdr_addr: phdr_user_addr,
        phent_size: header.e_phentsize,
        phnum: header.e_phnum,
        tls_filesz,
        tls_memsz,
        tls_align,
        tls_vaddr,
        tls_tp,
    })
}

/// Out of line so the loader body does not carry its loop locals.
#[inline(never)]
fn compute_phdr_user_addr(
    header: &crate::elf::Elf64Header,
    segments: &[crate::elf::ValidatedSegment],
    min_vaddr: u64,
    code_base: u64,
) -> u64 {
    let phoff = header.e_phoff;
    let phdr_end = phoff + (header.e_phnum as u64) * (header.e_phentsize as u64);
    for seg in segments.iter() {
        let seg_file_end = seg.file_offset + seg.file_size;
        if phoff >= seg.file_offset && phdr_end <= seg_file_end {
            let offset_in_seg = phoff - seg.file_offset;
            let seg_user =
                process_vm_translate_elf_address(seg.original_vaddr, min_vaddr, code_base);
            return seg_user + offset_in_seg;
        }
    }
    0
}

fn calculate_load_offset(segments: &[ValidatedSegment], code_base: u64) -> (u64, bool) {
    let min_vaddr = segments.iter().map(|s| s.original_vaddr).min().unwrap_or(0);

    let needs_reloc = min_vaddr >= KERNEL_VIRTUAL_BASE || min_vaddr != code_base;
    (min_vaddr, needs_reloc)
}

pub fn process_vm_translate_elf_address(addr: u64, min_vaddr: u64, code_base: u64) -> u64 {
    if addr >= KERNEL_VIRTUAL_BASE {
        let offset = addr.wrapping_sub(KERNEL_VIRTUAL_BASE);
        code_base.wrapping_add(offset)
    } else if min_vaddr >= KERNEL_VIRTUAL_BASE {
        let offset = addr.wrapping_sub(min_vaddr);
        code_base.wrapping_add(offset)
    } else if min_vaddr < code_base {
        addr.wrapping_add(code_base.wrapping_sub(min_vaddr))
    } else {
        addr
    }
}

fn unmap_existing_code_region(
    vm_space: &mut KArc<VmSpace>,
    code_base: u64,
) -> Result<(), MapError> {
    // Exactly [code_start, data_start), so a neighbouring region is never
    // caught by the arithmetic.
    let data_start = crate::memory_layout_defs::PROCESS_DATA_START_VA;
    unmap_user_range(vm_space, code_base, data_start)?;
    Ok(())
}

/// `None` if the page is unmapped.
pub fn process_vm_read_user_u8(vm_space: &KArc<VmSpace>, addr: u64) -> Option<u8> {
    let mut buf = [0u8; 1];
    process_vm_read_user_bytes(vm_space, addr, &mut buf).ok()?;
    Some(buf[0])
}

/// Little-endian. `None` if any byte of the range is unmapped.
pub fn process_vm_read_user_u64(vm_space: &KArc<VmSpace>, addr: u64) -> Option<u64> {
    let mut buf = [0u8; 8];
    process_vm_read_user_bytes(vm_space, addr, &mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

pub fn process_vm_read_user_bytes(
    vm_space: &KArc<VmSpace>,
    addr: u64,
    dst: &mut [u8],
) -> Result<(), ElfError> {
    let mut read = 0usize;
    while read < dst.len() {
        let va = addr
            .checked_add(read as u64)
            .ok_or(ElfError::SegmentSizeOverflow)?;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;
        let chunk = core::cmp::min(dst.len() - read, PAGE_SIZE_4KB as usize - page_off);

        let phys = crate::user_mappings::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ElfError::NullPointer);
        }
        let virt = phys.to_virt();
        if !hhdm_read_bytes(virt, page_off, &mut dst[read..read + chunk]) {
            return Err(ElfError::NullPointer);
        }
        read += chunk;
    }
    Ok(())
}

/// `Err(ElfError::NullPointer)` if any user page in the range is not mapped.
pub fn process_vm_write_user_bytes(
    vm_space: &KArc<VmSpace>,
    dst_addr: u64,
    data: &[u8],
) -> Result<(), ElfError> {
    write_user_bytes(vm_space, dst_addr, data)
}

fn write_user_bytes(vm_space: &KArc<VmSpace>, dst_addr: u64, data: &[u8]) -> Result<(), ElfError> {
    let mut written = 0usize;
    while written < data.len() {
        let va = dst_addr
            .checked_add(written as u64)
            .ok_or(ElfError::SegmentSizeOverflow)?;
        let page_va = va & !(PAGE_SIZE_4KB - 1);
        let page_off = (va & (PAGE_SIZE_4KB - 1)) as usize;
        let chunk = core::cmp::min(data.len() - written, PAGE_SIZE_4KB as usize - page_off);

        let phys = crate::user_mappings::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(page_va));
        if phys.is_null() {
            return Err(ElfError::NullPointer);
        }
        let virt = phys.to_virt();
        if !hhdm_write_bytes(virt, page_off, &data[written..written + chunk]) {
            return Err(ElfError::NullPointer);
        }
        written += chunk;
    }
    Ok(())
}

fn load_segment_pages(
    vm_space: &mut KArc<VmSpace>,
    data: &[u8],
    segment: &ValidatedSegment,
    user_start: u64,
    user_end: u64,
) -> Result<u32, ElfError> {
    let map_flags = if (segment.flags & PF_W) != 0 {
        PageFlags::USER_RW.bits()
    } else {
        PageFlags::USER_RO.bits()
    };

    let page_start = align_down(user_start as usize, PAGE_SIZE_4KB as usize) as u64;
    let page_end = align_up(user_end as usize, PAGE_SIZE_4KB as usize) as u64;

    let mut dst = page_start;
    let mut pages_mapped = 0u32;

    while dst < page_end {
        // Two ELF segments can overlap within a page, so an existing mapping
        // here is expected.
        let existing_phys =
            crate::user_mappings::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(dst));
        let phys = if !existing_phys.is_null() {
            if (map_flags & PageFlags::WRITABLE.bits()) != 0 {
                ostd_mark_range_user_4kb(
                    vm_space,
                    VirtAddr::new(dst),
                    VirtAddr::new(dst + PAGE_SIZE_4KB),
                    true,
                )
                .map_err(|_| ElfError::NullPointer)?;
            }
            existing_phys
        } else {
            let new_phys = alloc_kernel_page();
            if new_phys.is_null() {
                return Err(ElfError::NullPointer);
            }
            if let Err(err) = ostd_map_4kb_user(vm_space, VirtAddr::new(dst), new_phys, map_flags) {
                klog_info!("load_segment_pages: OSTD map failed: {:?}", err);
                free_page_frame(new_phys);
                return Err(ElfError::NullPointer);
            }
            pages_mapped += 1;
            new_phys
        };

        let dest_virt = phys.to_virt();
        if dest_virt.is_null() {
            if existing_phys.is_null() {
                free_page_frame(phys);
            }
            return Err(ElfError::NullPointer);
        }

        copy_segment_page_data(data, segment, dst, user_start, dest_virt);

        dst += PAGE_SIZE_4KB;
    }

    Ok(pages_mapped)
}

fn copy_segment_page_data(
    data: &[u8],
    segment: &ValidatedSegment,
    page_va: u64,
    user_seg_start: u64,
    dest_virt: VirtAddr,
) {
    let page_end_va = page_va.wrapping_add(PAGE_SIZE_4KB);
    let seg_file_end = user_seg_start.wrapping_add(segment.file_size);
    let seg_mem_end = user_seg_start.wrapping_add(segment.mem_size);

    let copy_start = core::cmp::max(page_va, user_seg_start);
    let copy_end = core::cmp::min(page_end_va, seg_file_end);

    if copy_start < copy_end {
        let page_off_in_seg = copy_start - user_seg_start;
        let dest_off = (copy_start - page_va) as usize;
        let copy_len = (copy_end - copy_start) as usize;
        let src_off = segment.file_offset.wrapping_add(page_off_in_seg) as usize;

        if src_off < data.len() && src_off.saturating_add(copy_len) <= data.len() {
            let _ = hhdm_write_bytes(dest_virt, dest_off, &data[src_off..src_off + copy_len]);
        }
    }

    if seg_mem_end > seg_file_end {
        let zero_start = core::cmp::max(page_va, seg_file_end);
        let zero_end = core::cmp::min(page_end_va, seg_mem_end);
        if zero_start < zero_end {
            let zero_off = (zero_start - page_va) as usize;
            let zero_len = (zero_end - zero_start) as usize;
            let _ = hhdm_fill_bytes(dest_virt, zero_off, zero_len, 0);
        }
    }
}
pub fn create_process_vm() -> u32 {
    create_process_vm_ref().map_or(INVALID_PROCESS_ID, |p| p.process_id)
}

/// Register a process and give it an address space, for callers that have no
/// process object of their own — tests and boot paths. A real spawn goes
/// through the scheduler's lease, so its accounting edge names its spawner.
fn create_process_vm_standalone() -> Option<ProcessVmRef> {
    let process = slopos_ostd::process::process_spawn_root().ok()?;
    let vm = create_process_vm_for(process.clone());
    if vm.is_none() {
        if let Some(handle) = process.handle() {
            slopos_ostd::process::process_retire(handle);
        }
    }
    vm
}

pub fn create_process_vm_ref() -> Option<ProcessVmRef> {
    create_process_vm_standalone()
}

/// Give `process` an address space, in the slot its registry handle names.
pub fn create_process_vm_for(process: KArc<Process>) -> Option<ProcessVmRef> {
    let layout = aslr::randomize_process_layout(&DEFAULT_PROCESS_LAYOUT);

    let Some(reservation) = VmReservation::claim(process) else {
        klog_info!("create_process_vm: could not claim the process's VM slot");
        return None;
    };
    let slot = reservation.slot;
    let process_id = reservation.process_id;
    let generation = reservation.generation;

    // Physical resources are allocated with no slot lock held.
    let mm_ctx_id = crate::mmu::alloc_mm_context_id();
    let vm_space = match VmSpace::new() {
        Ok(s) => s,
        Err(_) => {
            klog_info!(
                "create_process_vm: VmSpace::new failed (kernel-master / FrameAlloc not registered?)"
            );
            drop(reservation);
            return None;
        }
    };
    // The `CursorUnmapHook` / `on_activate` callbacks route LUF policy by this
    // handle, and read 0 as "not a per-process space".
    vm_space.set_mm_ctx_handle(mm_ctx_id.raw());
    let vm_space_arc = match KArc::try_new(vm_space) {
        Ok(a) => a,
        Err(_) => {
            klog_info!("create_process_vm: KArc<VmSpace> heap alloc failed");
            drop(reservation);
            return None;
        }
    };

    {
        let mut proc = PROCESS_VMS[slot].lock();
        proc.process = Some(reservation.process.clone());
        proc.process_id = process_id;
        proc.generation = generation;
        proc.vm_space = Some(vm_space_arc);
        proc.vma_map.clear();
        // Bound before the first mapping, so every page is charged to its
        // owner rather than to nobody.
        proc.vma_map.bind_account(reservation.process.account());
        proc.code_start = layout.code_start;
        proc.data_start = layout.data_start;
        proc.heap_start = layout.heap_start;
        proc.heap_end = layout.heap_start;
        proc.heap_break = layout.heap_start;
        proc.stack_start = layout.stack_top - layout.stack_size;
        proc.stack_end = layout.stack_top;
        proc.flags = 0;

        let code_s = proc.code_start;
        let data_s = proc.data_start;
        let heap_s = proc.heap_start;
        let stack_s = proc.stack_start;
        let stack_e = proc.stack_end;

        let code_region = VmaRegion {
            protection: Protection::RX,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Code,
        };
        let data_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Data,
        };
        let stack_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Stack,
        };

        if add_vma_to_inner(&mut proc, code_s, data_s, code_region) != 0
            || add_vma_to_inner(&mut proc, data_s, heap_s, data_region) != 0
            || add_vma_to_inner(&mut proc, stack_s, stack_e, stack_region) != 0
        {
            klog_info!("create_process_vm: Failed to seed initial VMAs");
            teardown_inner_mappings(&mut proc, slot_tlb_key(slot));
            proc.vm_space = None;
            // Unbind fully: a slot that kept its process reference would still
            // answer the generation check, so the next `claim` would refuse a
            // slot nothing uses.
            proc.process = None;
            proc.process_id = INVALID_PROCESS_ID;
            proc.generation = 0;
            drop(proc);
            drop(reservation);
            return None;
        }

        let stack_page_flags = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Stack,
        }
        .to_page_flags();

        let stack_start = proc.stack_start;
        let stack_end = proc.stack_end;
        let stack_flags_bits = stack_page_flags.bits();
        let vm_space_for_map = proc
            .vm_space
            .as_mut()
            .expect("create_process_vm: vm_space present before stack map");
        if map_user_range(vm_space_for_map, stack_start, stack_end, stack_flags_bits).is_err() {
            klog_info!("create_process_vm: Failed to map process stack");
            teardown_inner_mappings(&mut proc, slot_tlb_key(slot));
            proc.vm_space = None;
            proc.process = None;
            proc.process_id = INVALID_PROCESS_ID;
            proc.generation = 0;
            drop(proc);
            drop(reservation);
            return None;
        }

        // Map a single zero page to tolerate benign null accesses in early userland.

        let vm_space_for_null = proc
            .vm_space
            .as_mut()
            .expect("create_process_vm: vm_space still present after stack map");
        if map_user_range(
            vm_space_for_null,
            0,
            PAGE_SIZE_4KB,
            PageFlags::USER_RW.bits(),
        )
        .is_ok()
        {
            let null_region = VmaRegion {
                protection: Protection::RW,
                backing: RegionBacking::Anonymous,
                lazy: false,
                cow: false,
                user: true,
                purpose: RegionPurpose::General,
            };
            let _ = add_vma_to_inner(&mut proc, 0, PAGE_SIZE_4KB, null_region);
        } else {
            klog_info!("create_process_vm: Failed to map null page for user task");
        }

        klog_info!("Created process VM space for PID {}", process_id);
    }
    tlb::register_process_tlb(slot_tlb_key(slot));
    Some(ProcessVmRef {
        process_id,
        handle: Handle::from_parts(slot as u32, generation),
    })
}

pub fn destroy_process_vm(process: ProcessId) -> c_int {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };

    {
        let guard = PROCESS_VMS[slot].lock();
        if guard.process_id == INVALID_PROCESS_ID {
            return 0;
        }
    }
    klog_info!("Destroying process VM space for PID {}", process.id());
    let released: Option<KArc<Process>>;

    {
        let mut proc = PROCESS_VMS[slot].lock();
        if proc.process_id != process.id() {
            return 0;
        }

        klog_debug!("destroy_process_vm({}): teardown_process_mappings", process);
        teardown_inner_mappings(&mut proc, slot_tlb_key(slot));
        // Cleared while the slot is still bound, after the shootdown above has
        // landed: otherwise the next occupant inherits this one's CPU set and
        // shoots down CPUs that never mapped it.
        tlb::unregister_process_tlb(slot_tlb_key(slot));
        proc.vm_space = None;
        klog_debug!("destroy_process_vm({}): page table cleanup done", process);

        proc.vm_space = None;

        proc.process_id = INVALID_PROCESS_ID;
        proc.generation = 0;
        proc.flags = 0;
        // Released below, off the slot lock: this can be the last reference,
        // and `Process::drop` returns the id to an allocator no lock here
        // covers.
        released = proc.process.take();
    }

    // Retired after the unbind, so the id outlives every translation to the
    // address space it named.
    if let Some(process) = released.as_ref()
        && let Some(handle) = process.handle()
    {
        slopos_ostd::process::process_retire(handle);
    }
    drop(released);
    0
}

pub fn process_vm_alloc(process: ProcessId, size: u64, flags: u32) -> u64 {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return 0;
    }
    let size_aligned = (size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if size_aligned == 0 {
        return 0;
    }
    let start_addr = proc.heap_end;
    let end_addr = start_addr + size_aligned;
    if end_addr > DEFAULT_PROCESS_LAYOUT.heap_max {
        klog_info!("process_vm_alloc: Heap overflow");
        return 0;
    }

    let heap_region = VmaRegion {
        protection: Protection {
            read: true,
            write: flags & PageFlags::WRITABLE.bits() as u32 != 0,
            exec: false,
        },
        backing: RegionBacking::Anonymous,
        lazy: true,
        cow: false,
        user: true,
        purpose: RegionPurpose::Heap,
    };

    if add_vma_to_inner(&mut proc, start_addr, end_addr, heap_region) != 0 {
        klog_info!("process_vm_alloc: Failed to record VMA");
        return 0;
    }

    proc.heap_end = end_addr;
    proc.heap_break = end_addr;
    start_addr
}

pub fn process_vm_free(process: ProcessId, vaddr: u64, size: u64) -> c_int {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return -1,
    };
    if size == 0 {
        return -1;
    }
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return -1;
    }

    let start = vaddr & !(PAGE_SIZE_4KB - 1);
    let end = (vaddr + size + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);
    if !vma_range_valid(start, end) {
        klog_info!("process_vm_free: Invalid or unaligned range");
        return -1;
    }

    if proc.vma_map.find_covering(start, end).is_none() {
        klog_info!("process_vm_free: Range not covered by a VMA");
        return -1;
    }

    match unmap_and_free_range_inner(&mut *proc, start, end) {
        Ok(_) => {}
        Err(err) => {
            klog_info!("process_vm_free: unmap failed: {:?}", err);
            return -1;
        }
    };

    proc.vma_map
        .remove_range(start, end, |_overlap_start, _overlap_end, _region| {
            // The pages were freed above by `unmap_and_free_range_inner`.
        });

    if proc.heap_end == end && end > proc.heap_start {
        proc.heap_end = start;
        proc.heap_break = start;
    }

    0
}

/// Tear down every bound address space.
pub fn init_process_vm() -> c_int {
    slopos_ostd::process::quota::register_pages_reconciler(reconcile_page_charges);
    for i in 0..MAX_PROCESSES {
        // The slot's own process, so the teardown names the object rather than
        // a number that may already have been reissued.
        let bound = PROCESS_VMS[i].lock().process.clone();
        if let Some(process) = bound
            && let Some(id) = ProcessId::of(&process)
        {
            destroy_process_vm(id);
        }
    }

    for i in 0..MAX_PROCESSES {
        PROCESS_VMS[i].lock().reset();
    }
    klog_info!("Process VM manager initialized");

    0
}

/// Report every bound address space's mapped-versus-charged page counts.
/// `try_lock` rather than `lock`: blocking would order the diagnostic console
/// behind every address-space operation.
fn reconcile_page_charges(report: &mut dyn FnMut(slopos_ostd::process::AccountId, u32, u32)) {
    for slot in PROCESS_VMS.iter() {
        let Some(guard) = slot.try_lock() else {
            continue;
        };
        if guard.process.is_none() {
            continue;
        }
        let (walked, tracked, charged) = guard.vma_map.audit();
        // `walked` is recomputed from the tree, `tracked` the incrementally
        // maintained span; reporting the recomputed one makes drift visible.
        debug_assert_eq!(
            walked, tracked,
            "VmaMap span drifted from the tree it summarises"
        );
        report(guard.vma_map.account(), walked, charged);
    }
}

/// Process-address-space slot occupancy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessVmStats {
    pub total_processes: u32,
    pub active_processes: u32,
}

pub fn get_process_vm_stats() -> ProcessVmStats {
    ProcessVmStats {
        total_processes: MAX_PROCESSES as u32,
        active_processes: count_bound_slots(),
    }
}

pub fn get_current_process_id() -> u32 {
    // TODO(tech-debt): always returns 0 — the VM layer has no "current"
    // process, the scheduler does; callers should ask it and this should go.
    0
}

pub fn process_vm_get_region(process: ProcessId, addr: u64) -> Option<VmaRegion> {
    let slot = find_slot_for_pid(process)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return None;
    }

    let aligned_addr = addr & !(PAGE_SIZE_4KB - 1);
    let (_start, _end, region) = guard.vma_map.find_containing(aligned_addr)?;
    Some(region.clone())
}

pub fn process_vm_get_stack_top(process: ProcessId) -> u64 {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process.id() {
        return 0;
    }
    guard.stack_end
}

pub fn process_vm_reset_stack(process: ProcessId) -> c_int {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return -1,
    };

    // Read the extent before taking the slot lock: allocating under that
    // IRQs-off lock can itself trigger a cross-CPU drain, and deadlock.
    let (stack_start, stack_end) = slot_read_lock_free(&PROCESS_VMS[slot], |inner| {
        (inner.stack_start, inner.stack_end)
    });
    if stack_end <= stack_start {
        return -1;
    }
    let page_count = ((stack_end - stack_start) / PAGE_SIZE_4KB) as usize;

    // Gathering the frames tears the whole stack down as one operation, so the
    // replacement mapping is installed against a settled address space.
    let mut gathered: KVec<UFrame<AnonymousMeta>> = match KVec::with_capacity(page_count) {
        Ok(v) => v,
        Err(_) => return -1,
    };

    let result = {
        let mut guard = PROCESS_VMS[slot].lock();
        if guard.process_id != process.id() {
            -1
        } else if let Some(vm_space_ref) = guard.vm_space.as_mut() {
            let mut addr = stack_start;
            let mut ok = true;
            while addr < stack_end {
                match crate::user_mappings::ostd_unmap_4kb_user_take(
                    vm_space_ref,
                    VirtAddr::new(addr),
                ) {
                    Ok(Some(frame)) => {
                        if gathered.push(frame).is_err() {
                            ok = false;
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        klog_info!("process_vm_reset_stack: unmap failed: {:?}", err);
                        ok = false;
                        break;
                    }
                }
                addr += PAGE_SIZE_4KB;
            }

            if !ok {
                -1
            } else {
                let stack_page_flags = VmaRegion {
                    protection: Protection::RW,
                    backing: RegionBacking::Anonymous,
                    lazy: false,
                    cow: false,
                    user: true,
                    purpose: RegionPurpose::Stack,
                }
                .to_page_flags();
                let vm_space_ref = guard
                    .vm_space
                    .as_mut()
                    .expect("process_vm_reset_stack: vm_space still present after unmap");
                if map_user_range(
                    vm_space_ref,
                    stack_start,
                    stack_end,
                    stack_page_flags.bits(),
                )
                .is_err()
                {
                    -1
                } else {
                    0
                }
            }
        } else {
            -1
        }
    };

    // Shootdown of the old mappings, off the slot lock with interrupts
    // enabled.
    tlb::flush_all_for_process(slot_tlb_key(slot));

    // The old frames are now safe to release.
    drop(gathered);

    result
}

/// Set the process program break (Linux `brk` semantics).
///
/// Returns exactly `new_brk` on success; a query (`new_brk == 0`) or an
/// out-of-range request returns the current break unchanged; a mapping
/// failure returns 0. Userland allocators rely on that exact-equality
/// handshake, so never page-round the returned value. Page granularity is
/// internal: the mapped extent tracks `round_up_4k(heap_break)` in `heap_end`.
pub fn process_vm_brk(process: ProcessId, new_brk: u64) -> u64 {
    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return 0;
    }

    if new_brk == 0 {
        return proc.heap_break;
    }

    if new_brk < proc.heap_start || new_brk > DEFAULT_PROCESS_LAYOUT.heap_max {
        return proc.heap_break;
    }

    let new_end = match new_brk.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return proc.heap_break,
    };

    if new_end > proc.heap_end {
        let start_addr = proc.heap_end;
        let end_addr = new_end;
        let heap_region = VmaRegion {
            protection: Protection::RW,
            backing: RegionBacking::Anonymous,
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::Heap,
        };

        let heap_map_flags = heap_region.to_page_flags().bits();

        if add_vma_to_inner(&mut proc, start_addr, end_addr, heap_region) != 0 {
            return 0;
        }

        let vm_space_for_brk = proc
            .vm_space
            .as_mut()
            .expect("process_vm_brk: vm_space present for live pid");
        if map_user_range(vm_space_for_brk, start_addr, end_addr, heap_map_flags).is_err() {
            proc.vma_map
                .remove_range(start_addr, end_addr, |_, _, _| {});
            return 0;
        }
        proc.heap_end = new_end;
    } else if new_end < proc.heap_end {
        let start_addr = new_end;
        let end_addr = proc.heap_end;

        match unmap_and_free_range_inner(&mut *proc, start_addr, end_addr) {
            Ok(_) => {}
            Err(err) => {
                klog_info!("process_vm_brk: shrink unmap failed: {:?}", err);
                return 0;
            }
        };
        proc.vma_map
            .remove_range(start_addr, end_addr, |_, _, _| {});

        proc.heap_end = new_end;
    }

    proc.heap_break = new_brk;
    proc.heap_break
}

fn find_mmap_gap_inner(inner: &ProcessVm, size: u64) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};

    if size == 0 {
        return 0;
    }

    inner
        .vma_map
        .find_gap(PROCESS_MMAP_START_VA, PROCESS_MMAP_END_VA, size)
        .unwrap_or(0)
}

pub fn process_vm_mmap(
    process: ProcessId,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    process_vm_mmap_inner(
        process, addr_hint, length, prot, flags_val, fd, offset, None,
    )
}

/// Extended mmap for shared mappings. `memfd_raw` is the packed memfd handle
/// from the fd's `OpenFile.handle` (resolved by the syscall handler).
pub fn process_vm_mmap_shared(
    process: ProcessId,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    offset: u64,
    memfd_raw: usize,
) -> u64 {
    process_vm_mmap_inner(
        process,
        addr_hint,
        length,
        prot,
        flags_val,
        -1,
        offset,
        Some(crate::memfd::handle_from_raw(memfd_raw)),
    )
}

/// Map a SlopRing region into `process` (SLOPRING § 5.1). `paddrs` lists the
/// `RingMeta` frame physical addresses, one per 4 KiB page in region order.
/// Each PTE takes an independent ref on its frame, so a mapping that outlives
/// the ring fd cannot UAF. Returns the user virtual base address, or `0` on
/// failure — partial maps are rolled back.
pub fn process_vm_map_ring(process: ProcessId, paddrs: &[PhysAddr]) -> u64 {
    use crate::user_mappings::{ostd_map_ring_4kb_user, ostd_unmap_ring_4kb_user};

    if paddrs.is_empty() {
        return 0;
    }
    let size = (paddrs.len() as u64) * PAGE_SIZE_4KB;

    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return 0;
    }

    let start_addr = find_mmap_gap_inner(&proc, size);
    if start_addr == 0 {
        klog_info!("process_vm_map_ring: no free region for {} bytes", size);
        return 0;
    }
    let end_addr = start_addr + size;

    let region = VmaRegion {
        protection: Protection::RW,
        backing: RegionBacking::Ring,
        lazy: false,
        cow: false,
        user: true,
        purpose: RegionPurpose::General,
    };

    let inner = &mut *proc;
    // Charged before a single PTE is written, so a refusal costs no rollback.
    let ring_pages = match inner.vma_map.reserve_pages(start_addr, end_addr) {
        Ok(r) => r,
        Err(_) => {
            klog_info!("process_vm_map_ring: address space is at its page ceiling");
            return 0;
        }
    };
    let vm_space = inner
        .vm_space
        .as_mut()
        .expect("process_vm_map_ring: vm_space present for live pid");

    let pte_flags = PageFlags::USER_RW.bits();

    for (i, pa) in paddrs.iter().enumerate() {
        let vaddr = start_addr + (i as u64) * PAGE_SIZE_4KB;
        if let Err(err) = ostd_map_ring_4kb_user(vm_space, VirtAddr::new(vaddr), *pa, pte_flags) {
            klog_info!("process_vm_map_ring: cursor map failed: {:?}", err);
            for j in 0..i {
                let rv = start_addr + (j as u64) * PAGE_SIZE_4KB;
                if let Err(rollback_err) = ostd_unmap_ring_4kb_user(vm_space, VirtAddr::new(rv)) {
                    klog_info!(
                        "process_vm_map_ring: rollback unmap failed: {:?}",
                        rollback_err
                    );
                    return 0;
                }
            }
            return 0;
        }
    }

    inner
        .vma_map
        .insert_reserved(start_addr, end_addr, region, ring_pages);

    start_addr
}

fn process_vm_mmap_inner(
    process: ProcessId,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
    memfd_handle: Option<crate::memfd::MemfdHandle>,
) -> u64 {
    use crate::memory_layout_defs::{PROCESS_MMAP_END_VA, PROCESS_MMAP_START_VA};
    use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED};

    let is_shared = flags_val & MAP_SHARED != 0;
    let is_anonymous = flags_val & MAP_ANONYMOUS != 0;
    let is_private = flags_val & MAP_PRIVATE != 0;

    if is_shared && is_private {
        return 0;
    }
    if is_shared {
        if memfd_handle.is_none() || offset != 0 {
            klog_info!("process_vm_mmap: MAP_SHARED requires memfd_handle and offset=0");
            return 0;
        }
    } else {
        if !is_anonymous || !is_private {
            klog_info!("process_vm_mmap: requires MAP_ANONYMOUS|MAP_PRIVATE or MAP_SHARED");
            return 0;
        }
        if fd != -1 || offset != 0 {
            klog_info!("process_vm_mmap: fd must be -1 and offset 0 for anonymous");
            return 0;
        }
    }

    if length == 0 {
        return 0;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return 0,
    };

    let shared_info = if is_shared {
        let Some((phys, memfd_size, pages)) = memfd_handle.and_then(crate::memfd::memfd_get_info)
        else {
            klog_info!("process_vm_mmap: invalid or unsized memfd_handle");
            return 0;
        };
        if size > memfd_size as u64 {
            klog_info!(
                "process_vm_mmap: requested size {} > memfd size {}",
                size,
                memfd_size
            );
            return 0;
        }
        Some((phys, pages))
    } else {
        None
    };

    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return 0;
    }

    let is_fixed = flags_val & MAP_FIXED != 0;

    let start_addr = if is_fixed {
        if (addr_hint & (PAGE_SIZE_4KB - 1)) != 0 {
            klog_info!("process_vm_mmap: MAP_FIXED address not page-aligned");
            return 0;
        }
        if addr_hint < PROCESS_MMAP_START_VA
            || addr_hint
                .checked_add(size)
                .map_or(true, |end| end > PROCESS_MMAP_END_VA)
        {
            klog_info!("process_vm_mmap: MAP_FIXED address out of mmap region");
            return 0;
        }
        let end_addr = addr_hint + size;
        let inner = &mut *proc;
        let overlaps = match collect_overlapping_vmas(inner, addr_hint, end_addr) {
            Ok(overlaps) => overlaps,
            Err(_) => {
                klog_info!("process_vm_mmap MAP_FIXED: overlap allocation failed");
                return 0;
            }
        };
        // Force a panic fatal while `vm_space` is out of `inner`; unwinding
        // through the half-mutated global would leave it torn for later
        // syscalls.
        let abort_guard = AbortOnUnwind::new();
        let mut vm_space_taken = inner
            .vm_space
            .take()
            .expect("process_vm_mmap MAP_FIXED: vm_space present for live pid");

        for (overlap_start, overlap_end, region) in overlaps.iter() {
            match unmap_region_range_dir(
                &mut vm_space_taken,
                slot_tlb_key(slot),
                *overlap_start,
                *overlap_end,
                region,
            ) {
                Ok(_) => {}
                Err(err) => {
                    if err.processed_end > addr_hint {
                        inner.vma_map.remove_range(
                            addr_hint,
                            err.processed_end,
                            |removed_start, removed_end, region| {
                                dec_removed_shared_mapcount(removed_start, removed_end, region);
                            },
                        );
                    }
                    klog_info!(
                        "process_vm_mmap MAP_FIXED: overlap unmap failed: {:?}",
                        err.err
                    );
                    inner.vm_space = Some(vm_space_taken);
                    abort_guard.disarm();
                    return 0;
                }
            }
        }

        inner
            .vma_map
            .remove_range(addr_hint, end_addr, |removed_start, removed_end, region| {
                dec_removed_shared_mapcount(removed_start, removed_end, region);
            });

        inner.vm_space = Some(vm_space_taken);
        abort_guard.disarm();

        addr_hint
    } else {
        let chosen = find_mmap_gap_inner(&proc, size);
        if chosen == 0 {
            klog_info!("process_vm_mmap: No free region found for {} bytes", size);
            return 0;
        }
        chosen
    };

    let end_addr = start_addr + size;

    if let Some((phys, _pages)) = shared_info {
        use slopos_abi::syscall::PROT_WRITE;

        // `shared_info` is Some only on the MAP_SHARED path, whose validation
        // above proved a memfd handle is present.
        let memfd_handle = memfd_handle.expect("shared mapping requires a memfd handle");

        let shared_region = VmaRegion {
            protection: Protection {
                read: prot_to_region(prot).protection.read,
                write: prot_to_region(prot).protection.write,
                exec: prot_to_region(prot).protection.exec,
            },
            backing: RegionBacking::SharedMemfd {
                handle: memfd_handle,
            },
            lazy: false,
            cow: false,
            user: true,
            purpose: RegionPurpose::General,
        };

        let inner = &mut *proc;
        let shared_pages = match inner.vma_map.reserve_pages(start_addr, end_addr) {
            Ok(r) => r,
            Err(_) => {
                klog_info!("process_vm_mmap shared: address space is at its page ceiling");
                return 0;
            }
        };
        let vm_space_for_shared = inner
            .vm_space
            .as_mut()
            .expect("process_vm_mmap shared: vm_space present for live pid");
        let page_count = (size / PAGE_SIZE_4KB) as u32;

        let pte_flags = if prot & PROT_WRITE != 0 {
            PageFlags::USER_RW.bits()
        } else {
            PageFlags::USER_RO.bits()
        };

        for i in 0..page_count {
            let vaddr = start_addr + (i as u64) * PAGE_SIZE_4KB;
            let paddr = PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
            if let Err(err) =
                ostd_map_4kb_user(vm_space_for_shared, VirtAddr::new(vaddr), paddr, pte_flags)
            {
                klog_info!("process_vm_mmap shared: OSTD cursor map failed: {:?}", err);
                for j in 0..i {
                    let rv = start_addr + (j as u64) * PAGE_SIZE_4KB;
                    if let Err(rollback_err) =
                        ostd_unmap_4kb_user(vm_space_for_shared, VirtAddr::new(rv))
                    {
                        klog_info!(
                            "process_vm_mmap shared: rollback unmap failed: {:?}",
                            rollback_err
                        );
                        return 0;
                    }
                }
                return 0;
            }
        }

        inner
            .vma_map
            .insert_reserved(start_addr, end_addr, shared_region, shared_pages);

        crate::memfd::memfd_inc_mapcount_by(memfd_handle, page_count);

        start_addr
    } else {
        let region = prot_to_region(prot);

        if add_vma_to_inner(&mut proc, start_addr, end_addr, region) != 0 {
            klog_info!("process_vm_mmap: Failed to insert VMA");
            return 0;
        }

        start_addr
    }
}

pub fn process_vm_munmap(process: ProcessId, addr: u64, length: u64) -> i32 {
    if length == 0 || (addr & (PAGE_SIZE_4KB - 1)) != 0 {
        return -1;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return -1,
    };

    let end = match addr.checked_add(size) {
        Some(v) => v,
        None => return -1,
    };

    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return -1;
    }

    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    let inner = &mut *proc;

    // munmap of an executable mapping is forbidden.
    for (s, e, r) in inner.vma_map.iter() {
        if s < end && e > addr && r.protection.exec {
            return -1;
        }
    }

    // Unmap first, then remove VMA metadata only after OSTD accepted every page.
    let overlaps = match collect_overlapping_vmas(inner, addr, end) {
        Ok(overlaps) => overlaps,
        Err(_) => {
            klog_info!("process_vm_munmap: overlap allocation failed");
            return -1;
        }
    };
    let abort_guard = AbortOnUnwind::new();
    let mut vm_space_taken = inner
        .vm_space
        .take()
        .expect("process_vm_munmap: vm_space present for live pid");

    for (overlap_start, overlap_end, region) in overlaps.iter() {
        match unmap_region_range_dir(
            &mut vm_space_taken,
            slot_tlb_key(slot),
            *overlap_start,
            *overlap_end,
            region,
        ) {
            Ok(_) => {}
            Err(err) => {
                if err.processed_end > addr {
                    inner.vma_map.remove_range(
                        addr,
                        err.processed_end,
                        |removed_start, removed_end, region| {
                            dec_removed_shared_mapcount(removed_start, removed_end, region);
                        },
                    );
                }
                klog_info!("process_vm_munmap: unmap failed: {:?}", err.err);
                inner.vm_space = Some(vm_space_taken);
                abort_guard.disarm();
                return -1;
            }
        }
    }

    inner
        .vma_map
        .remove_range(addr, end, |removed_start, removed_end, region| {
            dec_removed_shared_mapcount(removed_start, removed_end, region);
        });

    inner.vm_space = Some(vm_space_taken);

    abort_guard.disarm();
    0
}

pub fn process_vm_mprotect(process: ProcessId, addr: u64, length: u64, prot: u64) -> i32 {
    if length == 0 || (addr & (PAGE_SIZE_4KB - 1)) != 0 {
        return -1;
    }

    let size = match length.checked_add(PAGE_SIZE_4KB - 1) {
        Some(v) => v & !(PAGE_SIZE_4KB - 1),
        None => return -1,
    };

    let end = match addr.checked_add(size) {
        Some(v) => v,
        None => return -1,
    };

    let slot = match find_slot_for_pid(process) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process.id() {
        return -1;
    }

    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    let new_prot = prot_to_region(prot);
    let old_protection;
    let new_page_flags;
    {
        let (_vma_start, _vma_end, region) = match proc.vma_map.find_covering_mut(addr, end) {
            Some(v) => v,
            None => {
                klog_info!("process_vm_mprotect: Range not covered by VMA");
                return -1;
            }
        };

        old_protection = region.protection;
        region.protection = new_prot.protection;
        new_page_flags = region.to_page_flags();
    }

    if let Some(vm_space) = proc.vm_space.as_mut() {
        if let Err(err) = ostd_protect_range_4kb(
            vm_space,
            VirtAddr::new(addr),
            VirtAddr::new(end),
            new_page_flags,
        ) {
            klog_info!("process_vm_mprotect: OSTD protect failed: {:?}", err);
            if let Some((_vma_start, _vma_end, region)) = proc.vma_map.find_covering_mut(addr, end)
            {
                region.protection = old_protection;
            }
            return -1;
        }
    }

    0
}

/// `(vaddr, paddr, PageFlags bits)`, captured under the parent's per-process
/// lock so the walkers never re-read the parent's PML4 with that lock dropped.
type ClonePageSnapshot = (u64, PhysAddr, u64);

/// Captured parent-VMA + snapshot tuple, owned so the clone body never holds
/// the parent lock across a stack-allocated snapshot.
type CloneVmaEntry = (u64, u64, VmaRegion, KVec<ClonePageSnapshot>);

/// Under the parent's per-process lock: snapshot its scalars and VMAs, and
/// COW-mark every writable+user page of its anonymous VMAs. `None` if the
/// parent slot has no address space. Out of line for the 2 KiB stack gate.
#[inline(never)]
fn clone_cow_snapshot_parent(
    parent_slot: usize,
    parent_id: u32,
) -> Option<(u64, u64, u64, u64, u64, u64, u64, u32, KVec<CloneVmaEntry>)> {
    let mut guard = PROCESS_VMS[parent_slot].lock();
    if guard.process_id != parent_id || guard.vm_space.is_none() {
        klog_info!("process_vm_clone_cow: Parent has no address space");
        return None;
    }

    let vmas_iter: KVec<(u64, u64, VmaRegion)> =
        KVec::from_iter_fallible(guard.vma_map.iter().map(|(s, e, r)| (s, e, r.clone())))
            .expect("clone_cow: vmas alloc");

    let parent_vm_space_ref = guard
        .vm_space
        .as_mut()
        .expect("clone_cow: parent vm_space present for live pid");

    let mut vmas: KVec<CloneVmaEntry> = KVec::new();
    for (vma_start, vma_end, region) in vmas_iter.iter() {
        let vma_start = *vma_start;
        let vma_end = *vma_end;
        // SlopRing regions are not inherited (SLOPRING § 14): the SQ/CQ is
        // SPSC, so a second producer in the child is forbidden. Neither the
        // snapshot here nor the child-side walk touches one.
        if region.is_ring() {
            vmas.push((vma_start, vma_end, region.clone(), KVec::new()))
                .expect("clone_cow: vmas alloc");
            continue;
        }
        let mut snapshot: KVec<ClonePageSnapshot> = KVec::new();
        let is_shared = region.is_shared();
        let mut addr = vma_start;
        while addr < vma_end {
            let vaddr = VirtAddr::new(addr);
            let phys = ostd_virt_to_phys_4kb(parent_vm_space_ref, vaddr);
            if !phys.is_null() {
                if let Some(flags) = ostd_get_pte_flags_4kb(parent_vm_space_ref, vaddr) {
                    let keep = is_shared || flags.contains(PageFlags::USER);
                    if keep {
                        snapshot
                            .push((addr, phys, flags.bits()))
                            .expect("clone_cow: snapshot alloc");
                        if !is_shared
                            && flags.contains(PageFlags::USER)
                            && flags.contains(PageFlags::WRITABLE)
                        {
                            if let Err(err) = ostd_mark_cow_4kb(parent_vm_space_ref, vaddr) {
                                klog_info!(
                                    "process_vm_clone_cow: parent COW mark failed: {:?}",
                                    err
                                );
                                return None;
                            }
                        }
                    }
                }
            }
            addr += PAGE_SIZE_4KB;
        }
        vmas.push((vma_start, vma_end, region.clone(), snapshot))
            .expect("clone_cow: vmas alloc");
    }

    Some((
        guard.code_start,
        guard.data_start,
        guard.heap_start,
        guard.heap_end,
        guard.heap_break,
        guard.stack_start,
        guard.stack_end,
        guard.flags,
        vmas,
    ))
}

/// Maps the parent's pages into the child verbatim: no COW marker, the child
/// shares the same memfd pages. `Err(())` on the first failure.
#[inline(never)]
fn clone_cow_walk_shared_vma(
    child_vm_space: &mut KArc<VmSpace>,
    snapshot: &[ClonePageSnapshot],
) -> Result<u32, ()> {
    let mut cow_pages: u32 = 0;
    for &(addr, phys, flags_bits) in snapshot.iter() {
        let vaddr = VirtAddr::new(addr);
        if let Err(err) = ostd_map_4kb_user(child_vm_space, vaddr, phys, flags_bits) {
            klog_info!("clone_cow shared: OSTD child map failed: {:?}", err);
            return Err(());
        }
        cow_pages += 1;
    }
    Ok(cow_pages)
}

/// Maps the captured parent pages into the child with `WRITABLE` cleared and
/// the COW marker set; the parent side was marked during the snapshot phase.
/// `Err(())` on the first failure.
#[inline(never)]
fn clone_cow_walk_anon_vma(
    child_vm_space: &mut KArc<VmSpace>,
    snapshot: &[ClonePageSnapshot],
) -> Result<u32, ()> {
    let mut cow_pages: u32 = 0;
    for &(addr, phys, flags_bits) in snapshot.iter() {
        let vaddr = VirtAddr::new(addr);
        let parent_flags = PageFlags::from_bits_truncate(flags_bits);
        if !parent_flags.contains(PageFlags::USER) {
            continue;
        }

        let child_flags = (flags_bits & !PageFlags::WRITABLE.bits())
            | PageFlags::COW.bits()
            | PageFlags::USER.bits()
            | PageFlags::PRESENT.bits();

        if let Err(err) = ostd_map_4kb_user(child_vm_space, vaddr, phys, child_flags) {
            klog_info!("clone_cow anon: OSTD child map failed: {:?}", err);
            return Err(());
        }

        cow_pages += 1;
    }
    Ok(cow_pages)
}

/// Returns the child pid, or `INVALID_PROCESS_ID`.
pub fn process_vm_clone_cow(parent: ProcessId) -> u32 {
    process_vm_clone_cow_ref(parent).map_or(INVALID_PROCESS_ID, |p| p.process_id)
}

/// Registers a fresh process for the child. For callers with no process
/// object; a real fork goes through [`process_vm_clone_cow_for`] so the
/// child's accounting edge names its actual spawner.
pub fn process_vm_clone_cow_ref(parent: ProcessId) -> Option<ProcessVmRef> {
    let child = slopos_ostd::process::process_spawn_root().ok()?;
    let vm = process_vm_clone_cow_for(parent, child.clone());
    if vm.is_none() {
        if let Some(handle) = child.handle() {
            slopos_ostd::process::process_retire(handle);
        }
    }
    vm
}

/// Out of line and `#[cold]`: `format_args!` builds its argument array in the
/// caller's frame, which is measured against the 2 KiB stack gate.
#[cold]
#[inline(never)]
fn report_clone_page_ceiling(start: u64, end: u64) {
    klog_info!(
        "process_vm_clone_cow: child at its page ceiling mapping [{:#x},{:#x})",
        start,
        end
    );
}

/// Copy every inheritable VMA of `parent_vmas` into `child`, COW-marking the
/// anonymous ones. `Err` carries the count walked before the clone was
/// abandoned. Split out of `process_vm_clone_cow_for` for the 2 KiB stack gate.
#[inline(never)]
fn clone_cow_populate_child(
    child: &mut ProcessVm,
    parent_vmas: &[(u64, u64, VmaRegion, KVec<(u64, PhysAddr, u64)>)],
) -> Result<u32, u32> {
    let mut cow_pages = 0u32;
    for (vma_start, vma_end, parent_region, snapshot) in parent_vmas.iter() {
        let vma_start = *vma_start;
        let vma_end = *vma_end;
        if parent_region.is_ring() {
            continue;
        }
        let is_shared_vma = parent_region.is_shared();

        let child_region = if is_shared_vma {
            parent_region.clone()
        } else {
            let mut r = parent_region.clone();
            r.cow = true;
            r
        };

        if child
            .vma_map
            .insert(vma_start, vma_end, child_region)
            .is_err()
        {
            report_clone_page_ceiling(vma_start, vma_end);
            return Err(cow_pages);
        }
        if let Some(memfd_handle) = parent_region.memfd_handle() {
            crate::memfd::memfd_inc_mapcount_by(memfd_handle, vma_page_count(vma_start, vma_end));
        }

        // The child slot lock held by the caller is the sole owner of the
        // `KArc`, so `as_mut` succeeds.
        let child_vm_space_for_vma = child
            .vm_space
            .as_mut()
            .expect("clone_cow: child vm_space populated above");

        let walked = if is_shared_vma {
            clone_cow_walk_shared_vma(child_vm_space_for_vma, snapshot.as_slice())
        } else {
            clone_cow_walk_anon_vma(child_vm_space_for_vma, snapshot.as_slice())
        };

        match walked {
            Ok(n) => cow_pages += n,
            Err(()) => return Err(cow_pages),
        }
    }
    Ok(cow_pages)
}

pub fn process_vm_clone_cow_for(parent: ProcessId, child: KArc<Process>) -> Option<ProcessVmRef> {
    let parent_slot = match find_slot_for_pid(parent) {
        Some(s) => s,
        None => {
            klog_info!(
                "process_vm_clone_cow: Parent process {} not found",
                parent.id()
            );
            return None;
        }
    };

    let (
        parent_code_start,
        parent_data_start,
        parent_heap_start,
        parent_heap_end,
        parent_heap_break,
        parent_stack_start,
        parent_stack_end,
        parent_flags,
        parent_vmas,
    ) = match clone_cow_snapshot_parent(parent_slot, parent.id()) {
        Some(t) => t,
        None => return None,
    };

    let Some(reservation) = VmReservation::claim(child) else {
        klog_info!("process_vm_clone_cow: could not claim the child's VM slot");
        return None;
    };
    let child_slot = reservation.slot;
    let child_id = reservation.process_id;
    let child_generation = reservation.generation;

    let child_mm_ctx_id = crate::mmu::alloc_mm_context_id();
    let child_vm_space = match VmSpace::new() {
        Ok(s) => s,
        Err(_) => {
            klog_info!(
                "process_vm_clone_cow: VmSpace::new failed for child PID {}",
                child_id
            );
            drop(reservation);
            return None;
        }
    };
    child_vm_space.set_mm_ctx_handle(child_mm_ctx_id.raw());
    let child_vm_space_arc = match KArc::try_new(child_vm_space) {
        Ok(a) => a,
        Err(_) => {
            klog_info!(
                "process_vm_clone_cow: KArc<VmSpace> heap alloc failed for child PID {}",
                child_id
            );
            drop(reservation);
            return None;
        }
    };

    // The parent cannot be destroyed while a fork is in progress — a
    // scheduler guarantee.
    let cow_pages: u32;
    let mut clone_failed = false;

    {
        let mut child = PROCESS_VMS[child_slot].lock();
        child.process = Some(reservation.process.clone());
        child.process_id = child_id;
        child.generation = child_generation;
        child.vm_space = Some(child_vm_space_arc);
        child.vma_map.clear();
        child.vma_map.bind_account(reservation.process.account());
        child.code_start = parent_code_start;
        child.data_start = parent_data_start;
        child.heap_start = parent_heap_start;
        child.heap_end = parent_heap_end;
        child.heap_break = parent_heap_break;
        child.stack_start = parent_stack_start;
        child.stack_end = parent_stack_end;
        child.flags = parent_flags;

        match clone_cow_populate_child(&mut child, parent_vmas.as_slice()) {
            Ok(n) => cow_pages = n,
            Err(n) => {
                cow_pages = n;
                clone_failed = true;
            }
        }
    }

    // paging_mark_cow defers TLB invalidation -- flush once for all COW pages.
    if cow_pages > 0 {
        tlb::flush_all();
    }

    if clone_failed {
        klog_info!("process_vm_clone_cow: Clone failed, cleaning up");
        {
            // One acquisition: a slot still bound to `child_id` must never be
            // observable with its address space released. `reset` clears
            // `process`, which `teardown_inner_mappings` needs, so it is last.
            let mut child = PROCESS_VMS[child_slot].lock();
            // Dropping the child's VmSpace reclaims the partial COW tree.
            let _ = child.vm_space.take();
            teardown_inner_mappings(&mut child, slot_tlb_key(child_slot));
            tlb::unregister_process_tlb(slot_tlb_key(child_slot));
            child.reset();
        }
        drop(reservation);
        return None;
    }

    klog_info!(
        "process_vm_clone_cow: Cloned PID {} -> PID {} ({} COW pages)",
        parent.id(),
        child_id,
        cow_pages
    );

    tlb::register_process_tlb(slot_tlb_key(child_slot));

    Some(ProcessVmRef {
        process_id: child_id,
        handle: Handle::from_parts(child_slot as u32, child_generation),
    })
}
