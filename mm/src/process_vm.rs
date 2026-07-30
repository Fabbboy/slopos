use core::ffi::c_int;
use core::ptr;
use slopos_ostd::KVec;
use slopos_ostd::handle::{Handle, HandleError};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};
use slopos_ostd::panic::AbortOnUnwind;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{align_down, align_up, klog_debug, klog_info};

use crate::aslr;
use crate::dual_paging::{
    ostd_get_pte_flags_4kb, ostd_map_4kb_user, ostd_mark_cow_4kb, ostd_mark_range_user_4kb,
    ostd_protect_range_4kb, ostd_unmap_4kb_user, ostd_virt_to_phys_4kb,
};
use crate::elf::{ElfError, ElfValidator, MAX_LOAD_SEGMENTS, PF_W, ValidatedSegment};
use crate::hhdm::PhysAddrHhdm;
use crate::memory_layout_defs::DEFAULT_PROCESS_LAYOUT;
use crate::memory_layout_defs::{KERNEL_VIRTUAL_BASE, MAX_PROCESSES};
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging::ProcessPageDir;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::slab::{kfree, kmalloc};
use crate::tlb;
use crate::vma_region::{Protection, RegionBacking, RegionPurpose, VmaMap, VmaRegion};
use slopos_abi::task::INVALID_PROCESS_ID;

/// Per-process VM state, protected by the per-slot lock in `PROCESS_VMS`.
///
/// `page_dir` is the legacy `mm/src/paging` ProcessPageDir handle —
/// drives every user mapping today. `vm_space` is the
/// framekernel-correct OSTD handle — allocated alongside but not yet
/// used as the CR3 source. The pending consumer-migration work
/// rewrites every user-mapping callsite onto `vm_space.cursor_mut()`
/// and flips the reader; the legacy `page_dir` deletes after the
/// migration completes.
/// Per-process VM slot. Exposed as an opaque marker so other crates can
/// name [`Handle<ProcessVm>`]; all fields stay private to this module.
pub struct ProcessVm {
    process_id: u32,
    /// Slot-reuse generation. A fresh, globally-unique value is stamped
    /// each time this slot is bound to a process; it is the generation
    /// half of the slot's [`Handle`]. A handle minted for a previous
    /// occupant fails to resolve once the slot has been rebound, so a
    /// stale address-space reference becomes a typed `HandleError`
    /// rather than silently aliasing the new occupant.
    generation: u64,
    /// Wrapped in `KernelSync` because `*mut ProcessPageDir` is `!Send`
    /// by default; the actual page-dir is heap-allocated and shared
    /// only through the per-slot `SpinLock<ProcessVm>` (so
    /// concurrent access is serialised at the lock).
    page_dir: KernelSync<*mut ProcessPageDir>,
    /// Framekernel-correct address-space handle. `None` only between
    /// `reset()` and the next `create_process_vm` re-init; populated
    /// alongside `page_dir` for every live process.
    vm_space: Option<KArc<VmSpace>>,
    vma_map: VmaMap,
    code_start: u64,
    data_start: u64,
    heap_start: u64,
    /// Page-aligned end of the mapped heap extent. Always equals
    /// `heap_break` rounded up to the next page boundary.
    heap_end: u64,
    /// Byte-granular program break as last set via `process_vm_brk`
    /// (Linux `brk` semantics). Userland allocators do an
    /// exact-equality handshake on this value, so it must be returned
    /// verbatim — never page-rounded.
    heap_break: u64,
    stack_start: u64,
    stack_end: u64,
    total_pages: u32,
    flags: u32,
}

impl ProcessVm {
    const fn new() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            generation: 0,
            page_dir: KernelSync::new(ptr::null_mut()),
            vm_space: None,
            vma_map: VmaMap::new(),
            code_start: 0,
            data_start: 0,
            heap_start: 0,
            heap_end: 0,
            heap_break: 0,
            stack_start: 0,
            stack_end: 0,
            total_pages: 0,
            flags: 0,
        }
    }

    fn reset(&mut self) {
        self.process_id = INVALID_PROCESS_ID;
        self.page_dir = KernelSync::new(ptr::null_mut());
        // Drop the OSTD VmSpace (and its KArc-counted PML4 +
        // user-half tree) here. `destroy_process_vm` clears the
        // Option before calling reset so this normally re-runs on
        // an already-None field; defensive double-clear is fine.
        self.vm_space = None;
        self.vma_map.clear();
        self.code_start = 0;
        self.data_start = 0;
        self.heap_start = 0;
        self.heap_end = 0;
        self.heap_break = 0;
        self.stack_start = 0;
        self.stack_end = 0;
        self.total_pages = 0;
        self.flags = 0;
    }
}

/// Global slot-allocation state: only held during create/destroy/init to
/// manage which slots are in use and the next PID counter.
struct VmSlotAlloc {
    num_processes: u32,
    next_process_id: u32,
    /// Monotonic source of per-slot generations. Each slot binding draws
    /// a fresh value so a handle never collides with a later occupant of
    /// the same slot.
    next_generation: u64,
}

impl VmSlotAlloc {
    const fn new() -> Self {
        Self {
            num_processes: 0,
            next_process_id: 1,
            next_generation: 1,
        }
    }

    /// Draw a fresh, never-reused generation value.
    fn alloc_generation(&mut self) -> u64 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        g
    }
}

/// Per-process VM locks.  Each slot is independently lockable so that
/// independent processes never contend on each other's VM operations.
static PROCESS_VMS: [SpinLock<ProcessVm>; MAX_PROCESSES] = {
    const INIT: SpinLock<ProcessVm> = SpinLock::new(ProcessVm::new(), LOCK_LEVEL_RESOURCE);
    [INIT; MAX_PROCESSES]
};

/// Global slot allocator -- only taken for fork/exit/init to find free slots
/// and update the process count.
static VM_SLOT_ALLOC: SpinLock<VmSlotAlloc> =
    SpinLock::new(VmSlotAlloc::new(), LOCK_LEVEL_REGISTRY);

fn vma_range_valid(start: u64, end: u64) -> bool {
    start < end && (start & (PAGE_SIZE_4KB - 1)) == 0 && (end & (PAGE_SIZE_4KB - 1)) == 0
}

fn map_user_range(
    vm_space: &mut KArc<VmSpace>,
    start_addr: u64,
    end_addr: u64,
    map_flags: u64,
    pages_mapped_out: *mut u32,
) -> c_int {
    if (start_addr & (PAGE_SIZE_4KB - 1)) != 0
        || (end_addr & (PAGE_SIZE_4KB - 1)) != 0
        || end_addr <= start_addr
    {
        klog_info!("map_user_range: Unaligned or invalid range");
        return -1;
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
            write_optional_u32(pages_mapped_out, 0);
            return -1;
        }
        if let Err(err) = ostd_map_4kb_user(vm_space, VirtAddr::new(current), phys, map_flags) {
            klog_info!("map_user_range: OSTD cursor map failed: {:?}", err);
            free_page_frame(phys);
            if let Err(rollback_err) = rollback_range(vm_space, current, start_addr, &mut mapped) {
                klog_info!("map_user_range: rollback failed: {:?}", rollback_err);
            }
            write_optional_u32(pages_mapped_out, 0);
            return -1;
        }
        mapped += 1;
        current += PAGE_SIZE_4KB;
    }

    write_optional_u32(pages_mapped_out, mapped);
    0
}

/// Write `value` through `out` if non-null. Used for `*mut u32` C-ABI
/// shim outputs that the legacy callers pass in.
#[inline]
fn write_optional_u32(out: *mut u32, value: u32) {
    slopos_ostd::util::ptr_buf::nullable_write(out, value);
}

/// Copy `src.len()` bytes through the HHDM mapping at `virt + offset`.
/// Thin shim over OSTD's [`slopos_ostd::mm::hhdm_bytes::write_bytes`];
/// the interior `unsafe` lives in OSTD. Caller contract: `virt` is a
/// fresh resolution of a 4 KiB user-mapped frame's physical address
/// and the user-space `VmSpace` cursor pins the underlying page for
/// the duration of the call.
#[inline]
fn hhdm_write_bytes(virt: VirtAddr, offset: usize, src: &[u8]) -> bool {
    slopos_ostd::mm::hhdm_bytes::write_bytes(virt, offset, src)
}

/// Read `dst.len()` bytes from the HHDM mapping at `virt + offset` into
/// `dst`. Same caller contract as [`hhdm_write_bytes`].
#[inline]
fn hhdm_read_bytes(virt: VirtAddr, offset: usize, dst: &mut [u8]) -> bool {
    slopos_ostd::mm::hhdm_bytes::read_bytes(virt, offset, dst)
}

/// Fill `len` bytes at the HHDM mapping at `virt + offset` with `value`.
/// Same caller contract as [`hhdm_write_bytes`].
#[inline]
fn hhdm_fill_bytes(virt: VirtAddr, offset: usize, len: usize, value: u8) -> bool {
    slopos_ostd::mm::hhdm_bytes::fill_bytes(virt, offset, len, value)
}

/// Read `pml4_phys` from a `*mut ProcessPageDir` without exposing the
/// raw pointer to the caller. Returns `PhysAddr::NULL` if the handle is
/// null. Delegates to `ProcessPageDir::pml4_phys_from_raw` so the
/// interior `unsafe` (raw-pointer field deref) lives in
/// `paging/tables.rs` alongside the type definition.
#[inline]
fn page_dir_pml4_phys(page_dir: *mut ProcessPageDir) -> PhysAddr {
    ProcessPageDir::pml4_phys_from_raw(page_dir)
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

/// Lock-free read of a naturally-aligned field of `ProcessVm`.
/// Thin wrapper over OSTD's `SpinLock::read_atomic_field` so the single
/// `unsafe` reborrow lives in the lock implementation, not here.
///
/// Caller contract (matches `SpinLock::read_atomic_field`'s): every
/// field accessed through `f` must be a naturally-aligned scalar (u32
/// / pointer / atomic) that is only written under the per-slot lock,
/// so a plain load is tear-free on x86-64. Callers MUST re-acquire the
/// per-slot lock before reading any composite (multi-word) field.
#[inline]
fn slot_read_lock_free<R>(slot: &SpinLock<ProcessVm>, f: impl FnOnce(&ProcessVm) -> R) -> R {
    slot.read_atomic_field(f)
}

#[inline]
fn slot_pid_lock_free(slot: &SpinLock<ProcessVm>) -> u32 {
    slot_read_lock_free(slot, |inner| inner.process_id)
}

#[inline]
fn slot_page_dir_lock_free(slot: &SpinLock<ProcessVm>) -> *mut ProcessPageDir {
    slot_read_lock_free(slot, |inner| *inner.page_dir)
}

/// Find the slot index for a given process ID using a lock-free scan.
fn find_slot_for_pid(process_id: u32) -> Option<usize> {
    if process_id == INVALID_PROCESS_ID {
        return None;
    }
    for i in 0..MAX_PROCESSES {
        if slot_pid_lock_free(&PROCESS_VMS[i]) == process_id {
            return Some(i);
        }
    }
    None
}

/// The generation-checked handle for `process_id`'s VM slot, if bound.
///
/// A `Handle<ProcessVm>` pairs the slot index with the slot's generation.
/// Held across time, it lets [`process_vm_with_handle`] detect — without
/// re-searching by pid — whether the slot still belongs to the same
/// process or has been recycled for another. The page-table / address
/// space a process owns lives inside this slot, so this is the
/// slot-reuse-safe reference to it.
pub fn process_vm_handle(process_id: u32) -> Option<Handle<ProcessVm>> {
    let slot = find_slot_for_pid(process_id)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }
    Some(Handle::from_parts(slot as u32, guard.generation))
}

/// Run `f` with mutable access to the `ProcessVm` named by `handle`.
///
/// Validates the slot index and generation: a handle whose slot was
/// rebound to a different process resolves to [`HandleError::Stale`]; an
/// unbound slot to [`HandleError::NoEntry`]; an out-of-range slot to
/// [`HandleError::OutOfBounds`]. The slot-reuse-safe counterpart to the
/// pid-keyed accessors — a stale reference is a typed error, never UB.
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

/// Lock-free page-directory lookup.  The page_dir pointer is only cleared
/// under the per-slot lock during `destroy_process_vm`, which is called after
/// the process has been fully descheduled, so concurrent readers see either
/// the valid pointer or null.
pub fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    slot_page_dir_lock_free(&PROCESS_VMS[slot])
}

/// Translate a user virtual address to its backing physical address
/// for `process_id`, via the OSTD `VmSpace` cursor. Returns 0 if the
/// slot is unbound, `vm_space` is missing, or no 4 KiB leaf is mapped
/// at `va`'s page-aligned address. The returned paddr includes the
/// page offset of `va` (mirrors legacy `virt_to_phys_in_dir`).
pub fn process_vm_user_va_to_paddr(process_id: u32, va: u64) -> u64 {
    let Some(slot) = find_slot_for_pid(process_id) else {
        return 0;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return 0;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return 0;
    };
    crate::dual_paging::ostd_virt_to_phys_4kb(vm_space, slopos_abi::addr::VirtAddr::new(va))
        .as_u64()
}

/// Look up `process_id`'s OSTD `VmSpace` and return a cloned
/// [`KArc<VmSpace>`] handle. Returns `None` if the slot is unbound
/// or the OSTD `vm_space` is not yet attached.
///
/// Used by `slopos_mm::user_copy::*` to bridge the legacy
/// PCR-implicit syscall ABI (no explicit `&VmSpace` argument) onto
/// OSTD's explicit-`&VmSpace` `copy_*_user` primitives. Holding a
/// `KArc` clone (rather than borrowing through the per-slot lock)
/// means the user-copy walk and the `__ostd_raw_usercopy` call can
/// run with the per-slot lock released, avoiding lock-order issues
/// with the page-fault recovery path.
pub fn process_vm_get_vm_space(
    process_id: u32,
) -> Option<slopos_ostd::KArc<slopos_ostd::mm::vm_space::VmSpace>> {
    let slot = find_slot_for_pid(process_id)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }
    guard.vm_space.as_ref().cloned()
}

/// Read-side check: is `va` mapped AND user-accessible in
/// `process_id`'s OSTD VmSpace? Mirrors legacy
/// `paging_is_user_accessible` — kernel-half pages return `false`.
pub fn process_vm_user_va_is_user_accessible(process_id: u32, va: u64) -> bool {
    let Some(slot) = find_slot_for_pid(process_id) else {
        return false;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return false;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return false;
    };
    crate::dual_paging::ostd_is_user_accessible_4kb(vm_space, slopos_abi::addr::VirtAddr::new(va))
}

/// Read the OSTD `VmSpace`'s PML4 paddr for `process_id`. Returns 0
/// if the slot is unbound or `vm_space` is missing. After
/// [`VmSpace::activate`] writes CR3, this matches the hardware CR3 —
/// callers that compare against the live CR3 (user-fault dispatcher,
/// task-table lookup) must use this rather than the legacy
/// `(*page_dir).pml4_phys` until the legacy half retires.
pub fn process_vm_get_ostd_pml4_paddr(process_id: u32) -> u64 {
    let Some(slot) = find_slot_for_pid(process_id) else {
        return 0;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return 0;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return 0;
    };
    vm_space.pml4_paddr().as_u64()
}

/// Install `process_id`'s OSTD `VmSpace` as the current CPU's CR3
/// via [`VmSpace::activate`]. Returns `true` on success, `false` if
/// the slot is unbound or `vm_space` is missing (caller should fall
/// back to `kernel_vm_space().lock().activate()`).
///
/// The per-process lock is held only across the brief `&VmSpace`
/// borrow + activate call; activate itself takes `&self`.
///
/// Safe entry: the scheduler upholds the context-switch contract for
/// `VmSpace::activate` (IRQs disabled, on this CPU, kernel-half
/// preserved). The activate body lazily resyncs the kernel half on
/// the way to CR3 reload, so consumers never observe a stale window.
pub fn process_vm_activate(process_id: u32) -> bool {
    let Some(slot) = find_slot_for_pid(process_id) else {
        return false;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return false;
    }
    let Some(vm_space) = guard.vm_space.as_ref() else {
        return false;
    };
    vm_space.activate_at_context_switch();
    true
}

/// Run `f` under the per-process lock with mutable access to
/// `process_id`'s OSTD `KArc<VmSpace>`. Returns `None` if the slot is
/// unbound or `vm_space` is missing.
///
/// The closure runs while the per-process lock is held — keep the
/// body fast. Used by the page-fault handlers
/// (`cow::handle_cow_fault`, `demand::handle_demand_fault`), which
/// need the address space and the lock that guards it in one step.
pub fn process_vm_with_dual_paging<R>(
    process_id: u32,
    f: impl FnOnce(&mut KArc<VmSpace>) -> R,
) -> Option<R> {
    let slot = find_slot_for_pid(process_id)?;
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }
    let vm_space = guard.vm_space.as_mut()?;
    Some(f(vm_space))
}

/// Like [`process_vm_with_dual_paging`] but also resolves the
/// covering [`VmaRegion`] for `fault_addr` under the same lock — so
/// the page-fault handlers can both dual-write and read the region
/// without dropping and re-acquiring the per-process lock (which
/// would deadlock recursive callers like the demand-fault path).
pub fn process_vm_with_dual_paging_and_region<R>(
    process_id: u32,
    fault_addr: u64,
    f: impl FnOnce(&mut KArc<VmSpace>, VmaRegion) -> R,
) -> Option<R> {
    let slot = find_slot_for_pid(process_id)?;
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }
    let region = {
        let (_rs, _re, region_ref) = guard.vma_map.find_containing(fault_addr)?;
        region_ref.clone()
    };
    let vm_space = guard.vm_space.as_mut()?;
    Some(f(vm_space, region))
}

/// Read the PML4 physical address for a process — the value
/// [`VmSpace::activate`] writes to CR3 during scheduler context-switch.
/// Returns `0` if the slot is unbound or the OSTD `vm_space` is missing
/// (callers treat 0 as "no VM"; the scheduler refuses to dispatch).
pub fn process_vm_get_cr3_phys(process_id: u32) -> u64 {
    process_vm_get_ostd_pml4_paddr(process_id)
}

/// Look up the stable 64-bit `MmContextId` associated with this process.
///
/// Returns `MmContextId::INVALID` if the process slot has been freed or
/// the page directory is not yet populated. The scheduler uses this value
/// to key the per-CPU ASID cache so PCID reuse survives `process_id`
/// recycling and works across the pre-/post-`MmContext` transition.
pub fn process_vm_get_mm_ctx_id(process_id: u32) -> crate::mmu::MmContextId {
    let Some(slot) = find_slot_for_pid(process_id) else {
        return crate::mmu::MmContextId::INVALID;
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
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
        // SAFETY: lock-free read of the process_id field. Validating
        // the OSTD `vm_space` requires a brief lock acquisition because
        // the `Option<KArc<VmSpace>>` is mutated under the per-process
        // SpinLock.
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

pub fn process_vm_sync_kernel_mappings(process_id: u32) {
    // Kernel-half resync now happens automatically inside
    // `VmSpace::activate` via OSTD's `resync_kernel_half_if_stale`. The
    // legacy `paging_sync_kernel_mappings` walk is gone; this entry
    // point stays as a no-op so older callers keep compiling until
    // they're cleaned up.
    let _ = process_id;
}

/// Insert a VMA into the process address space.
///
/// The caller must guarantee the range does not overlap with existing VMAs
/// (enforced by the gap finder for non-MAP_FIXED mmaps). This matches Linux's
/// design where the gap finder provides the non-overlap guarantee by construction.
///
/// After insertion, adjacent VMAs with compatible metadata are merged
/// automatically inside `VmaMap::insert`.
fn add_vma_to_inner(inner: &mut ProcessVm, start: u64, end: u64, region: VmaRegion) -> c_int {
    if !vma_range_valid(start, end) {
        return -1;
    }
    inner.vma_map.insert(start, end, region);
    0
}

/// Convert POSIX mmap prot flags to a `VmaRegion` (anonymous, lazy, user-mode).
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

/// Tear down the per-process VM bookkeeping: flush every CPU's TLB
/// for this address space, decrement memfd map_counts for shared
/// VMAs, and clear the VMA map. The caller is responsible for
/// dropping the OSTD `KArc<VmSpace>` from the slot — its `Drop`
/// walks the user half, decrements every leaf frame's META_SLOTS
/// (returning to the buddy when refcounts hit zero), and reclaims
/// the intermediate page tables. Flush-free at OSTD level, but we
/// issue the one authoritative `flush_all_for_process` shootdown
/// here to drop stale TLB entries on every CPU before any frame
/// is reused.
fn teardown_inner_mappings(inner: &mut ProcessVm) {
    let pid = inner.process_id;
    if pid != INVALID_PROCESS_ID {
        tlb::flush_all_for_process(pid);
    }
    inner.vma_map.drain(|start, end, region| {
        dec_removed_shared_mapcount(start, end, region);
    });
    inner.total_pages = 0;
    inner.heap_end = inner.heap_start;
    inner.heap_break = inner.heap_start;
}

/// Unmap and free pages in a range. The OSTD `cursor.unmap` returns a
/// `UFrame` whose Drop decrements META_SLOTS — when the count reaches
/// zero the registered allocator deallocs the underlying buddy frame.
fn unmap_and_free_range_dir(
    vm_space: &mut KArc<VmSpace>,
    pid: u32,
    start: u64,
    end: u64,
) -> Result<UnmapProgress, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(unmap_progress(start, 0));
    }
    let mut freed = 0u32;
    let mut addr = start;
    while addr < end {
        match ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) => freed += 1,
            Ok(false) => {}
            Err(err) => return Err(unmap_region_error(err, addr, freed)),
        }
        addr += PAGE_SIZE_4KB;
    }
    let _ = pid;
    Ok(unmap_progress(end, freed))
}

/// Unmap a SlopRing mapping range. Each page's PTE holds an
/// independent `from_in_use` ref on the `RingMeta` frame; dropping the
/// PTE ref here (via the typed ring cursor-unmap) leaves the frame
/// alive as long as the ring object still holds its ref, so this is
/// neither "free" (the ring object owns the lifecycle) nor "nofree"
/// (the PTE genuinely held a ref that must be released). Returns the
/// number of pages unmapped.
fn unmap_ring_range_dir(
    vm_space: &mut KArc<VmSpace>,
    pid: u32,
    start: u64,
    end: u64,
) -> Result<UnmapProgress, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(unmap_progress(start, 0));
    }
    let mut unmapped = 0u32;
    let mut addr = start;
    while addr < end {
        match crate::dual_paging::ostd_unmap_ring_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) => unmapped += 1,
            Ok(false) => {}
            Err(err) => {
                if unmapped > 0 && pid != INVALID_PROCESS_ID {
                    tlb::flush_all_for_process(pid);
                }
                return Err(unmap_region_error(err, addr, unmapped));
            }
        }
        addr += PAGE_SIZE_4KB;
    }
    // The OSTD cursor-unmap only issues a *local* INVLPG; a ring region is
    // routinely re-created at the same VA (each `Ring::setup` reuses the
    // lowest mmap gap), so a task migrated to another CPU could otherwise
    // read the prior ring's stale translation. Shoot down every CPU in the
    // process's cpumask, exactly as the shared-memfd unmap path does.
    if unmapped > 0 && pid != INVALID_PROCESS_ID {
        tlb::flush_all_for_process(pid);
    }
    Ok(unmap_progress(end, unmapped))
}

/// Unmap shared-memfd pages. Each `ostd_unmap_4kb_user` drops this
/// mapping's MetaSlot ref, but the page is NOT freed here: the memfd
/// object holds its own owning ref (claimed in `memfd_ftruncate`), so
/// the count stays ≥ 1 until the memfd itself is released. The page
/// returns to the buddy exactly once, when the last of {memfd owning
/// ref, every mapping} drops — never via a raw allocator free that
/// would bypass the MetaSlot. Returns the number of pages unmapped
/// (for total_pages accounting).
fn unmap_range_nofree_dir(
    vm_space: &mut KArc<VmSpace>,
    pid: u32,
    start: u64,
    end: u64,
) -> Result<UnmapProgress, UnmapRegionError> {
    if !vma_range_valid(start, end) {
        return Ok(unmap_progress(start, 0));
    }
    let mut unmapped = 0u32;
    let mut addr = start;
    while addr < end {
        match ostd_unmap_4kb_user(vm_space, VirtAddr::new(addr)) {
            Ok(true) => unmapped += 1,
            Ok(false) => {}
            Err(err) => {
                if unmapped > 0 && pid != INVALID_PROCESS_ID {
                    tlb::flush_all_for_process(pid);
                }
                return Err(unmap_region_error(err, addr, unmapped));
            }
        }
        addr += PAGE_SIZE_4KB;
    }
    // Memfd-backed unmaps need a targeted shootdown across every CPU
    // in the process's cpumask so threads drop stale translations to
    // the still-live memfd frames.
    if unmapped > 0 && pid != INVALID_PROCESS_ID {
        tlb::flush_all_for_process(pid);
    }
    Ok(unmap_progress(end, unmapped))
}

type VmaOverlap = (u64, u64, VmaRegion);

#[derive(Clone, Copy)]
struct UnmapProgress {
    processed_end: u64,
    unmapped_pages: u32,
}

struct UnmapRegionError {
    err: MapError,
    progress: UnmapProgress,
}

fn unmap_progress(processed_end: u64, unmapped_pages: u32) -> UnmapProgress {
    UnmapProgress {
        processed_end,
        unmapped_pages,
    }
}

fn unmap_region_error(err: MapError, processed_end: u64, unmapped_pages: u32) -> UnmapRegionError {
    UnmapRegionError {
        err,
        progress: unmap_progress(processed_end, unmapped_pages),
    }
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
    pid: u32,
    start: u64,
    end: u64,
    region: &VmaRegion,
) -> Result<UnmapProgress, UnmapRegionError> {
    if region.is_ring() {
        unmap_ring_range_dir(vm_space, pid, start, end)
    } else if region.is_shared() {
        unmap_range_nofree_dir(vm_space, pid, start, end)
    } else {
        unmap_and_free_range_dir(vm_space, pid, start, end)
    }
}

// ELF structures for relocation parsing.
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

// ELF section types
const SHT_RELA: u32 = 4;

// x86-64 relocation types
const R_X86_64_64: u32 = 1; // Absolute 64-bit
const R_X86_64_PC32: u32 = 2; // RIP-relative 32-bit
const R_X86_64_32: u32 = 10; // Absolute 32-bit
const R_X86_64_32S: u32 = 11; // Absolute 32-bit sign-extended

/// Read a `T: Copy` (unaligned) at byte `offset` of `payload`. Returns
/// `None` if the read would extend past the slice. Thin shim over
/// `slopos_ostd::util::ptr_buf::read_pod_at` so the relocation walker
/// stays in safe Rust.
#[inline]
fn read_elf_pod<T: Copy>(payload: &[u8], offset: usize) -> Option<T> {
    slopos_ostd::util::ptr_buf::read_pod_at::<T>(payload, offset)
}

fn apply_elf_relocations(
    payload: *const u8,
    payload_len: usize,
    vm_space: &KArc<VmSpace>,
    section_mappings: &[(u64, u64, u64)], // (kernel_va_start, kernel_va_end, user_va_start)
) -> c_int {
    if payload.is_null() || payload_len == 0 {
        return -1;
    }
    // Caller (process_vm_load_elf_data → exec) hands in a bootloader /
    // file-loader-published byte buffer of `payload_len` bytes whose
    // lifetime exceeds this call. Only the first `payload_len` bytes
    // are accessed; bounds are re-checked at every structured read
    // via `read_elf_pod`.
    let payload_slice: &[u8] =
        slopos_ostd::util::ptr_buf::anchored_buf(&payload_len, payload, payload_len);

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

    // Helper to fetch a NUL-terminated section name as a slice borrow.
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

    // Helper to map kernel VA to user VA.
    let map_kernel_va_to_user = |kernel_va: u64| -> Option<u64> {
        for &(kern_start, kern_end, user_start) in section_mappings {
            if kernel_va >= kern_start && kernel_va < kern_end {
                return Some(user_start + (kernel_va - kern_start));
            }
        }
        None
    };

    // Iterate through section headers to find .rela sections.
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

        // Find the target section this relocation applies to.
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

        // Process relocation entries.
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

            // Calculate symbol VA based on relocation type.
            //
            // Reads and writes of the relocation site itself use the
            // same direct-HHDM pattern as the legacy loader: resolve
            // the user-VA's leading page to a kernel-mode HHDM virt,
            // then `read_unaligned`/`write_unaligned` at the page
            // offset. ELF segments are written into the user VM by
            // `load_segment_pages`, which calls `alloc_kernel_page()`
            // sequentially and gets back contiguous physical frames
            // straight out of the buddy's freshly-split high-order
            // block — so an unaligned read/write that straddles a
            // 4 KiB user-VA boundary lands in the next user-VA's
            // page through the contiguous HHDM mapping. Routing the
            // read/write through `process_vm_{read,write}_user_bytes`
            // would handle the spanning case independently of buddy
            // contiguity but adds a second `ostd_virt_to_phys_4kb`
            // walk per relocation, doubling the per-relocation cost
            // and pushing the ELF-load syscall past the NMI watchdog
            // budget on slow TCG hosts (CI). The buddy-contiguity
            // invariant is already relied on elsewhere in SlopOS's
            // ELF load path; this code follows the same convention.
            let symbol_va = match reloc_type {
                R_X86_64_PC32 | 4 => {
                    // 4 = R_X86_64_PLT32. Read the existing rel32 displacement
                    // currently in the instruction (placed by the linker
                    // against kernel-VA) and recover the original symbol VA.
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
                    // `read_virt` resolves to a live HHDM mapping for the
                    // user-VA's physical page; the immediate straddles
                    // into the contiguous next physical page when present
                    // (see invariant explained above). The OSTD-side
                    // `read_unaligned` helper carries the one `unsafe`.
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
            // `reloc_virt` resolves to a live HHDM mapping for the
            // user-VA's physical page; the immediate straddles into the
            // contiguous next physical page when present. The OSTD-side
            // `write_unaligned` helper carries the one `unsafe`.
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
    process_id: u32,
    data: &[u8],
    entry_out: &mut u64,
) -> Result<crate::elf::ElfExecInfo, ElfError> {
    let code_base = crate::memory_layout_defs::PROCESS_CODE_START_VA;

    let validator = ElfValidator::new(data)?.with_load_base(code_base);

    // Reject dynamically-linked binaries (PT_INTERP present).
    if validator.has_interpreter()? {
        return Err(ElfError::DynamicNotSupported);
    }

    let mut segments_store =
        slopos_ostd::KVec::<crate::elf::ValidatedSegment>::zeroed(crate::elf::MAX_LOAD_SEGMENTS)
            .map_err(|_| ElfError::NullPointer)?;
    let segment_count = validator.validate_load_segments_into(segments_store.as_mut_slice())?;

    let slot = find_slot_for_pid(process_id).ok_or(ElfError::NullPointer)?;

    // Heap-allocated section_mappings; `load_segments_and_tls` also
    // holds the locked page dir and the ~9-field `ElfExecInfo` return
    // value. Splitting that work into an `#[inline(never)]` helper
    // keeps this function's frame under the stack-safety gate.
    let info = load_segments_and_tls(
        &validator,
        data,
        code_base,
        slot,
        process_id,
        &segments_store.as_slice()[..segment_count],
    )?;
    *entry_out = info.entry;
    Ok(info)
}

/// Inner body of `process_vm_load_elf_data`: resolves page dir, runs
/// the segment mapping + TLS + relocation passes, and assembles the
/// [`ElfExecInfo`] result. Extracted to keep the outer function's
/// frame small.
#[inline(never)]
fn load_segments_and_tls(
    validator: &ElfValidator<'_>,
    data: &[u8],
    code_base: u64,
    slot: usize,
    process_id: u32,
    segments: &[crate::elf::ValidatedSegment],
) -> Result<crate::elf::ElfExecInfo, ElfError> {
    let header = validator.header();

    // TLS geometry is recorded for diagnostics only; the kernel no longer
    // builds a TLS block. The C library discovers PT_TLS via AT_PHDR and owns
    // all TLS construction (main thread and spawned threads alike).
    let tls_segment = validator.find_tls_segment()?;
    let (tls_vaddr, tls_filesz, tls_memsz, tls_align) = match tls_segment {
        Some((vaddr, filesz, memsz, align)) => (vaddr, filesz, memsz, align),
        None => (0, 0, 0, 0),
    };

    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return Err(ElfError::NullPointer);
    }
    let page_dir = *guard.page_dir;
    if page_dir.is_null() {
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

    // No kernel-built TLS block: the .tdata init image is already mapped as
    // part of the loaded program segments, and libc copies it per-thread.
    let tls_tp = 0u64;

    if needs_reloc {
        let vm_space_ref = guard
            .vm_space
            .as_ref()
            .expect("apply_elf_relocations: vm_space present for live pid");
        let _ = apply_elf_relocations(
            data.as_ptr(),
            data.len(),
            vm_space_ref,
            &section_mappings[..mapping_count],
        );
    }

    let user_entry = process_vm_translate_elf_address(header.e_entry, min_vaddr, code_base);
    let phdr_user_addr = compute_phdr_user_addr(header, segments, min_vaddr, code_base);
    // The program headers MUST be mapped in the user address space so libc can
    // walk AT_PHDR -> PT_TLS. A zero here means the linker left the phdrs out of
    // every PT_LOAD; refuse the exec loudly rather than ship a process whose TLS
    // can never be set up (which would fault on the first thread-local access).
    if phdr_user_addr == 0 {
        return Err(ElfError::InvalidPhdrOffset);
    }

    guard.total_pages = guard.total_pages.saturating_add(mapped_pages);
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

/// Walk loaded segments to locate the user-space mapping of the program
/// headers. Isolated into its own frame so the containing loader body
/// doesn't pick up its `for` loop locals.
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
    // Unmap exactly the code region [code_start, data_start) — no more,
    // so a neighbouring region is never caught by the arithmetic.
    let data_start = crate::memory_layout_defs::PROCESS_DATA_START_VA;
    unmap_user_range(vm_space, code_base, data_start)?;
    Ok(())
}

/// Read a `u8` from the user-VA space identified by `vm_space`.
/// Returns `None` if the page is unmapped.
pub fn process_vm_read_user_u8(vm_space: &KArc<VmSpace>, addr: u64) -> Option<u8> {
    let mut buf = [0u8; 1];
    process_vm_read_user_bytes(vm_space, addr, &mut buf).ok()?;
    Some(buf[0])
}

/// Read a `u64` (little-endian) from the user-VA space.  Returns
/// `None` if any byte of the range is unmapped.
pub fn process_vm_read_user_u64(vm_space: &KArc<VmSpace>, addr: u64) -> Option<u64> {
    let mut buf = [0u8; 8];
    process_vm_read_user_bytes(vm_space, addr, &mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

/// Generic user-VA read: copies `dst.len()` bytes starting at `addr`
/// from the address space identified by `vm_space` into `dst`.
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

        let phys = crate::dual_paging::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(page_va));
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

/// Write `data` into the user address space identified by `vm_space`,
/// translating each `dst_addr + offset` through the `VmSpace` and
/// writing through the HHDM mapping.
///
/// `Err(ElfError::NullPointer)` if any user page in the range is not
/// mapped. Used by ELF load + the kernel-side `write_to_user_stack`
/// helper consumed from `core/src/exec/mod.rs`.
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

        let phys = crate::dual_paging::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(page_va));
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
        // After dual-write retirement in map_user_range, the legacy
        // ProcessPageDir is empty for fresh processes. Use the OSTD
        // cursor to detect existing mappings (e.g. overlapping ELF
        // segments).
        let existing_phys = crate::dual_paging::ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(dst));
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
    let layout = aslr::randomize_process_layout(&DEFAULT_PROCESS_LAYOUT);

    // Phase 1: allocate a slot under the global lock.
    let (slot, process_id, generation) = {
        let mut alloc = VM_SLOT_ALLOC.lock();
        if alloc.num_processes >= MAX_PROCESSES as u32 {
            klog_info!("create_process_vm: Maximum processes reached");
            return INVALID_PROCESS_ID;
        }
        let mut found_slot = None;
        for i in 0..MAX_PROCESSES {
            // SAFETY: lock-free read of naturally-aligned u32 to find free slot.
            let pid = slot_pid_lock_free(&PROCESS_VMS[i]);
            if pid == INVALID_PROCESS_ID {
                found_slot = Some(i);
                break;
            }
        }
        let slot = match found_slot {
            Some(s) => s,
            None => {
                klog_info!("create_process_vm: No free process slots available");
                return INVALID_PROCESS_ID;
            }
        };
        let process_id = alloc.next_process_id;
        alloc.next_process_id += 1;
        alloc.num_processes += 1;
        let generation = alloc.alloc_generation();
        (slot, process_id, generation)
    };

    // Phase 2: allocate physical resources (no locks held).
    let pml4_phys = alloc_kernel_page();
    if pml4_phys.is_null() {
        klog_info!("create_process_vm: Failed to allocate PML4");
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let pml4_virt = pml4_phys.to_virt();
    if pml4_virt.is_null() {
        klog_info!("create_process_vm: No HHDM/identity map available for PML4");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let _ = hhdm_fill_bytes(pml4_virt, 0, PAGE_SIZE_4KB as usize, 0);

    let page_dir_ptr = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if page_dir_ptr.is_null() {
        klog_info!("create_process_vm: Failed to allocate page directory");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let mm_ctx_id = crate::mmu::alloc_mm_context_id();
    // Kernel-half mappings flow from `KERNEL_MASTER_PML4` via OSTD's
    // `VmSpace::new` + `resync_kernel_half_if_stale`. The frame this
    // descriptor accounts for stays empty and is never installed in CR3.
    let page_dir_init = ProcessPageDir::new(pml4_phys, process_id, mm_ctx_id);
    // `page_dir_ptr` came from `kmalloc(size_of::<ProcessPageDir>())`,
    // so the slot is valid and exclusively owned. The OSTD-side
    // `init_slot` helper performs the one `ptr::write` so this site
    // stays in safe Rust.
    ProcessPageDir::init_in_kmalloc_slot(page_dir_ptr, page_dir_init);

    // Allocate the framekernel-correct OSTD VmSpace alongside the
    // legacy ProcessPageDir (transitional dual-allocation). Both PML4
    // frames are independent — the OSTD one gets its own kernel-half
    // copy from the registered KERNEL_MASTER_PML4 via `VmSpace::new`.
    // The OSTD handle is stashed in `ProcessVm.vm_space` but no
    // user-side mapping flows through it yet; the consumer-migration
    // pass that's still pending rewrites every map / unmap / activate
    // callsite. On `VmSpace::new` failure we roll back exactly as the
    // legacy path would: free the ProcessPageDir's own PML4, kfree the
    // descriptor, decrement the slot count.
    let vm_space = match VmSpace::new() {
        Ok(s) => s,
        Err(_) => {
            klog_info!(
                "create_process_vm: VmSpace::new failed (kernel-master / FrameAlloc not registered?)"
            );
            free_page_frame(pml4_phys);
            kfree(page_dir_ptr as *mut _);
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }
    };
    // Plumb the slopos-side mm_ctx_id into the OSTD handle so the
    // (registered, but currently unwired) `CursorUnmapHook` /
    // `on_activate` callbacks can route LUF policy. The legacy
    // ProcessPageDir already carries its own copy.
    vm_space.set_mm_ctx_handle(mm_ctx_id.raw());
    let vm_space_arc = match KArc::try_new(vm_space) {
        Ok(a) => a,
        Err(_) => {
            klog_info!("create_process_vm: KArc<VmSpace> heap alloc failed");
            free_page_frame(pml4_phys);
            kfree(page_dir_ptr as *mut _);
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }
    };

    // Phase 3: initialize the per-process slot under its own lock.
    {
        let mut proc = PROCESS_VMS[slot].lock();
        proc.process_id = process_id;
        proc.generation = generation;
        proc.page_dir = KernelSync::new(page_dir_ptr);
        proc.vm_space = Some(vm_space_arc);
        proc.vma_map.clear();
        proc.code_start = layout.code_start;
        proc.data_start = layout.data_start;
        proc.heap_start = layout.heap_start;
        proc.heap_end = layout.heap_start;
        proc.heap_break = layout.heap_start;
        proc.stack_start = layout.stack_top - layout.stack_size;
        proc.stack_end = layout.stack_top;
        proc.total_pages = 1;
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
            teardown_inner_mappings(&mut proc);
            free_page_frame(page_dir_pml4_phys(page_dir_ptr));
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = KernelSync::new(ptr::null_mut());
            proc.process_id = INVALID_PROCESS_ID;
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
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
        let mut stack_pages: u32 = 0;
        let stack_start = proc.stack_start;
        let stack_end = proc.stack_end;
        let stack_flags_bits = stack_page_flags.bits();
        let vm_space_for_map = proc
            .vm_space
            .as_mut()
            .expect("create_process_vm: vm_space allocated alongside page_dir before stack map");
        if map_user_range(
            vm_space_for_map,
            stack_start,
            stack_end,
            stack_flags_bits,
            &mut stack_pages,
        ) != 0
        {
            klog_info!("create_process_vm: Failed to map process stack");
            teardown_inner_mappings(&mut proc);
            free_page_frame(page_dir_pml4_phys(page_dir_ptr));
            kfree(page_dir_ptr as *mut _);
            proc.page_dir = KernelSync::new(ptr::null_mut());
            proc.vm_space = None;
            proc.process_id = INVALID_PROCESS_ID;
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }
        proc.total_pages += stack_pages;

        // Map a single zero page to tolerate benign null accesses in early userland.
        let mut null_pages: u32 = 0;
        let vm_space_for_null = proc
            .vm_space
            .as_mut()
            .expect("create_process_vm: vm_space still present after stack map");
        if map_user_range(
            vm_space_for_null,
            0,
            PAGE_SIZE_4KB,
            PageFlags::USER_RW.bits(),
            &mut null_pages,
        ) == 0
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
            proc.total_pages += null_pages;
        } else {
            klog_info!("create_process_vm: Failed to map null page for user task");
        }

        klog_info!("Created process VM space for PID {}", process_id);
    }
    tlb::register_process_tlb(process_id);
    process_id
}

pub fn destroy_process_vm(process_id: u32) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };

    // Read current state under the per-process lock.
    {
        let guard = PROCESS_VMS[slot].lock();
        if guard.process_id == INVALID_PROCESS_ID {
            return 0;
        }
    }
    klog_info!("Destroying process VM space for PID {}", process_id);
    // The authoritative cross-CPU flush is issued inside
    // `teardown_inner_mappings` by `MmTeardownGuard::begin`.

    // Teardown under the per-process lock.
    {
        let mut proc = PROCESS_VMS[slot].lock();
        // Re-check after re-acquiring lock.
        if proc.process_id != process_id {
            return 0;
        }

        klog_debug!(
            "destroy_process_vm({}): teardown_process_mappings",
            process_id
        );
        teardown_inner_mappings(&mut proc);
        // Drop the OSTD VmSpace — its `Drop` walks the user half and
        // frees every leaf frame through META_SLOTS plus the
        // intermediate page tables.
        let _ = proc.vm_space.take();
        if !proc.page_dir.is_null() {
            let pml4_phys = page_dir_pml4_phys(*proc.page_dir);
            if !pml4_phys.is_null() {
                free_page_frame(pml4_phys);
            }
            klog_debug!("destroy_process_vm({}): kfree(page_dir)", process_id);
            kfree(*proc.page_dir as *mut _);
            proc.page_dir = KernelSync::new(ptr::null_mut());
        }
        klog_debug!(
            "destroy_process_vm({}): page table cleanup done",
            process_id
        );

        // Drop the OSTD VmSpace KArc. While the dual-allocation
        // window remains in effect, the OSTD-managed PML4 is unused
        // (no user mappings written to it); on drop,
        // `Frame::<PageTableMeta>::on_drop` returns its PML4 frame to
        // the buddy allocator and the (empty) user-half tree walker
        // no-ops. The pending consumer-migration pass will exercise
        // the real teardown path once user-mapping callsites switch
        // to `vm_space.cursor_mut`.
        proc.vm_space = None;

        proc.process_id = INVALID_PROCESS_ID;
        proc.total_pages = 0;
        proc.flags = 0;
    }

    // Decrement global count.
    {
        let mut alloc = VM_SLOT_ALLOC.lock();
        alloc.num_processes = alloc.num_processes.saturating_sub(1);
    }
    0
}

pub fn process_vm_alloc(process_id: u32, size: u64, flags: u32) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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

pub fn process_vm_free(process_id: u32, vaddr: u64, size: u64) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    if size == 0 {
        return -1;
    }
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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

    let freed = match unmap_and_free_range_inner(&mut *proc, start, end) {
        Ok(freed) => freed,
        Err(err) => {
            klog_info!("process_vm_free: unmap failed: {:?}", err);
            return -1;
        }
    };

    proc.vma_map
        .remove_range(start, end, |_overlap_start, _overlap_end, _region| {
            // Physical pages already freed above by unmap_and_free_range_inner.
        });

    proc.total_pages = proc.total_pages.saturating_sub(freed);
    if proc.heap_end == end && end > proc.heap_start {
        proc.heap_end = start;
        proc.heap_break = start;
    }

    0
}

pub fn init_process_vm() -> c_int {
    for i in 0..MAX_PROCESSES {
        // SAFETY: lock-free read of naturally-aligned u32 sentinel.
        let pid = slot_pid_lock_free(&PROCESS_VMS[i]);
        if pid != INVALID_PROCESS_ID {
            destroy_process_vm(pid);
        }
    }

    {
        let mut alloc = VM_SLOT_ALLOC.lock();
        alloc.num_processes = 0;
        alloc.next_process_id = 1;
    }
    for i in 0..MAX_PROCESSES {
        PROCESS_VMS[i].lock().reset();
    }
    klog_info!("Process VM manager initialized");

    0
}

pub fn get_process_vm_stats(total_processes: *mut u32, active_processes: *mut u32) {
    let alloc = VM_SLOT_ALLOC.lock();
    write_optional_u32(total_processes, MAX_PROCESSES as u32);
    write_optional_u32(active_processes, alloc.num_processes);
}

pub fn get_current_process_id() -> u32 {
    // This function was always racy under SMP with the old global lock (it
    // returned active_process which could change immediately after).
    // With per-process locks there is no meaningful "current" concept at the
    // VM layer -- the scheduler owns that. Return 0 for backwards compat.
    0
}

/// Look up the VMA region at a given address. Returns a cloned region.
pub fn process_vm_get_region(process_id: u32, addr: u64) -> Option<VmaRegion> {
    let slot = find_slot_for_pid(process_id)?;
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return None;
    }

    let aligned_addr = addr & !(PAGE_SIZE_4KB - 1);
    let (_start, _end, region) = guard.vma_map.find_containing(aligned_addr)?;
    Some(region.clone())
}

pub fn process_vm_increment_pages(process_id: u32, count: u32) {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return,
    };
    let mut guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return;
    }
    guard.total_pages = guard.total_pages.saturating_add(count);
}

pub fn process_vm_get_stack_top(process_id: u32) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let guard = PROCESS_VMS[slot].lock();
    if guard.process_id != process_id {
        return 0;
    }
    guard.stack_end
}

pub fn process_vm_reset_stack(process_id: u32) -> c_int {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };

    // Size the frame gather from the stack extent BEFORE taking the
    // per-process lock: allocating under that IRQs-off lock can itself
    // trigger a cross-CPU drain — the deadlock class this restructure
    // removes.
    let (stack_start, stack_end) = slot_read_lock_free(&PROCESS_VMS[slot], |inner| {
        (inner.stack_start, inner.stack_end)
    });
    if stack_end <= stack_start {
        return -1;
    }
    let page_count = ((stack_end - stack_start) / PAGE_SIZE_4KB) as usize;

    // Hold every unmapped frame until the cross-CPU shootdown completes,
    // so a freed frame can't be reused while a peer CPU still caches a
    // stale translation. The shootdown then runs AFTER the lock is
    // dropped, with interrupts enabled — never the synchronous broadcast
    // under the IRQs-off per-process lock that wedges a CPU on a
    // non-acking peer.
    let mut gathered: KVec<UFrame<AnonymousMeta>> = match KVec::with_capacity(page_count) {
        Ok(v) => v,
        Err(_) => return -1,
    };

    let result = {
        let mut guard = PROCESS_VMS[slot].lock();
        if guard.process_id != process_id || guard.page_dir.is_null() {
            -1
        } else {
            let vm_space_ref = guard
                .vm_space
                .as_mut()
                .expect("process_vm_reset_stack: vm_space present for live pid");

            // Gather-unmap the old stack: suppress the per-page LUF
            // cross-CPU deferral (the cursor still invalidates locally),
            // collecting the freed frames so none is released yet.
            crate::mmu::luf::suppress_cross_cpu_drain();
            let mut addr = stack_start;
            let mut ok = true;
            while addr < stack_end {
                match crate::dual_paging::ostd_unmap_4kb_user_take(
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
            crate::mmu::luf::unsuppress_cross_cpu_drain();

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
                let mut pages: u32 = 0;
                let vm_space_ref = guard
                    .vm_space
                    .as_mut()
                    .expect("process_vm_reset_stack: vm_space still present after unmap");
                if map_user_range(
                    vm_space_ref,
                    stack_start,
                    stack_end,
                    stack_page_flags.bits(),
                    &mut pages,
                ) != 0
                {
                    -1
                } else {
                    0
                }
            }
        }
        // per-process lock released here
    };

    // Cross-CPU shootdown of the old mappings, lock-free with interrupts
    // enabled. A never-scheduled address space has an empty per-process
    // cpumask, so this sends zero IPIs; a live one targets exactly the
    // CPUs that loaded it.
    tlb::flush_all_for_process(process_id);

    // The old frames are now safe to release.
    drop(gathered);

    result
}

/// Set the process program break (Linux `brk` semantics).
///
/// The break is byte-granular: a successful call returns exactly
/// `new_brk`, a query (`new_brk == 0`) or out-of-range request returns
/// the current break unchanged, and a hard mapping failure returns 0.
/// Userland allocators rely on the exact-equality handshake
/// (`brk(x) == x` means the break moved, anything else means it did
/// not) — returning a page-rounded value here desyncs their heap
/// bookkeeping from the real mapping and turns the next allocation
/// into a wild write. Page granularity is internal only: the mapped
/// extent tracks `round_up_4k(heap_break)` in `heap_end`.
pub fn process_vm_brk(process_id: u32, new_brk: u64) -> u64 {
    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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

        let mut pages_mapped: u32 = 0;
        let vm_space_for_brk = proc
            .vm_space
            .as_mut()
            .expect("process_vm_brk: vm_space present for live pid");
        if map_user_range(
            vm_space_for_brk,
            start_addr,
            end_addr,
            heap_map_flags,
            &mut pages_mapped,
        ) != 0
        {
            proc.vma_map
                .remove_range(start_addr, end_addr, |_, _, _| {});
            return 0;
        }
        proc.total_pages += pages_mapped;
        proc.heap_end = new_end;
    } else if new_end < proc.heap_end {
        let start_addr = new_end;
        let end_addr = proc.heap_end;

        let freed = match unmap_and_free_range_inner(&mut *proc, start_addr, end_addr) {
            Ok(freed) => freed,
            Err(err) => {
                klog_info!("process_vm_brk: shrink unmap failed: {:?}", err);
                return 0;
            }
        };
        proc.vma_map
            .remove_range(start_addr, end_addr, |_, _, _| {});

        proc.total_pages = proc.total_pages.saturating_sub(freed);
        proc.heap_end = new_end;
    }

    proc.heap_break = new_brk;
    proc.heap_break
}

// =============================================================================
// mmap / munmap / mprotect
// =============================================================================

/// Find a free gap in the process address space within the mmap region.
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

/// Map memory into the process address space (mmap).
///
/// Supports anonymous private mappings (existing) and shared memfd mappings (new).
/// For shared mappings, `memfd_handle` must be a valid memfd handle obtained from
/// the syscall handler (which resolves the fd before calling this function).
pub fn process_vm_mmap(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    process_vm_mmap_inner(
        process_id, addr_hint, length, prot, flags_val, fd, offset, None,
    )
}

/// Extended mmap for shared mappings. `memfd_raw` is the packed memfd handle
/// from the fd's `OpenFile.handle` (resolved by the syscall handler).
pub fn process_vm_mmap_shared(
    process_id: u32,
    addr_hint: u64,
    length: u64,
    prot: u64,
    flags_val: u64,
    offset: u64,
    memfd_raw: usize,
) -> u64 {
    process_vm_mmap_inner(
        process_id,
        addr_hint,
        length,
        prot,
        flags_val,
        -1,
        offset,
        Some(crate::memfd::handle_from_raw(memfd_raw)),
    )
}

/// Map a SlopRing region into `process_id` (SLOPRING § 5.1). `paddrs`
/// lists the contiguous-or-not `RingMeta` frame physical addresses (one
/// per 4 KiB page, in region order) the ring object already owns. Each
/// page is mapped read+write into a freshly-found mmap gap; the PTE
/// takes an independent `from_in_use` ref on the `RingMeta` frame (so a
/// mapping that outlives the ring fd cannot UAF — the frame survives
/// until both refs drop).
///
/// Returns the user virtual base address on success, or `0` on failure
/// (no gap, or a cursor map error — partial maps are rolled back).
pub fn process_vm_map_ring(process_id: u32, paddrs: &[PhysAddr]) -> u64 {
    use crate::dual_paging::{ostd_map_ring_4kb_user, ostd_unmap_ring_4kb_user};

    if paddrs.is_empty() {
        return 0;
    }
    let size = (paddrs.len() as u64) * PAGE_SIZE_4KB;

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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
    let vm_space = inner
        .vm_space
        .as_mut()
        .expect("process_vm_map_ring: vm_space present for live pid");

    // Ring pages are always mapped read+write to user.
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

    inner.vma_map.insert(start_addr, end_addr, region);
    inner.total_pages += paddrs.len() as u32;

    start_addr
}

fn process_vm_mmap_inner(
    process_id: u32,
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

    // Validate flag combinations
    if is_shared && is_private {
        return 0; // Cannot be both
    }
    if is_shared {
        // Shared mapping requires a memfd handle (resolved by syscall handler)
        if memfd_handle.is_none() || offset != 0 {
            klog_info!("process_vm_mmap: MAP_SHARED requires memfd_handle and offset=0");
            return 0;
        }
    } else {
        // Anonymous private (existing path)
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

    // For shared mappings, validate the memfd and get physical pages
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

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return 0,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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
        let pid = inner.process_id;
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

        let mut total_freed = 0u32;
        for (overlap_start, overlap_end, region) in overlaps.iter() {
            let progress = match unmap_region_range_dir(
                &mut vm_space_taken,
                pid,
                *overlap_start,
                *overlap_end,
                region,
            ) {
                Ok(progress) => progress,
                Err(err) => {
                    let total_unmapped = total_freed.saturating_add(err.progress.unmapped_pages);
                    if err.progress.processed_end > addr_hint {
                        inner.vma_map.remove_range(
                            addr_hint,
                            err.progress.processed_end,
                            |removed_start, removed_end, region| {
                                dec_removed_shared_mapcount(removed_start, removed_end, region);
                            },
                        );
                        inner.total_pages = inner.total_pages.saturating_sub(total_unmapped);
                    }
                    klog_info!(
                        "process_vm_mmap MAP_FIXED: overlap unmap failed: {:?}",
                        err.err
                    );
                    inner.vm_space = Some(vm_space_taken);
                    abort_guard.disarm();
                    return 0;
                }
            };
            total_freed = total_freed.saturating_add(progress.unmapped_pages);
        }

        // Remove all overlapping VMAs in the fixed range after OSTD unmaps succeeded.
        inner
            .vma_map
            .remove_range(addr_hint, end_addr, |removed_start, removed_end, region| {
                dec_removed_shared_mapcount(removed_start, removed_end, region);
            });
        inner.total_pages = inner.total_pages.saturating_sub(total_freed);

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
        // -- Shared memfd path: eagerly map the memfd's physical pages --
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
        let vm_space_for_shared = inner
            .vm_space
            .as_mut()
            .expect("process_vm_mmap shared: vm_space present for live pid");
        let page_count = (size / PAGE_SIZE_4KB) as u32;

        // Determine PTE flags from protection
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

        // Insert the shared VMA
        inner.vma_map.insert(start_addr, end_addr, shared_region);

        inner.total_pages += page_count;

        // Tell the memfd about this mapping
        crate::memfd::memfd_inc_mapcount_by(memfd_handle, page_count);

        start_addr
    } else {
        // -- Anonymous private path (existing behavior) --
        let region = prot_to_region(prot);

        if add_vma_to_inner(&mut proc, start_addr, end_addr, region) != 0 {
            klog_info!("process_vm_mmap: Failed to insert VMA");
            return 0;
        }

        start_addr
    }
}

/// Unmap a previously mmap'd memory region.
pub fn process_vm_munmap(process_id: u32, addr: u64, length: u64) -> i32 {
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

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
        return -1;
    }

    if addr >= crate::memory_layout_defs::USER_SPACE_END_VA
        || end > crate::memory_layout_defs::USER_SPACE_END_VA
    {
        return -1;
    }

    let inner = &mut *proc;

    // Pre-scan for EXEC regions -- munmap of executable mappings is forbidden.
    for (s, e, r) in inner.vma_map.iter() {
        if s < end && e > addr && r.protection.exec {
            return -1;
        }
    }

    // Unmap first, then remove VMA metadata only after OSTD accepted every page.
    let mut total_freed = 0u32;
    let overlaps = match collect_overlapping_vmas(inner, addr, end) {
        Ok(overlaps) => overlaps,
        Err(_) => {
            klog_info!("process_vm_munmap: overlap allocation failed");
            return -1;
        }
    };
    // `vm_space` is out of `inner` for the duration of the unmap; a panic
    // before it is restored would leave the global `PROCESS_VMS[slot]` torn
    // (vm_space=None, stale vma_map) for every later syscall. Force such a
    // panic fatal instead of unwinding through the half-mutated global.
    let abort_guard = AbortOnUnwind::new();
    let mut vm_space_taken = inner
        .vm_space
        .take()
        .expect("process_vm_munmap: vm_space present for live pid");

    for (overlap_start, overlap_end, region) in overlaps.iter() {
        let progress = match unmap_region_range_dir(
            &mut vm_space_taken,
            process_id,
            *overlap_start,
            *overlap_end,
            region,
        ) {
            Ok(progress) => progress,
            Err(err) => {
                let total_unmapped = total_freed.saturating_add(err.progress.unmapped_pages);
                if err.progress.processed_end > addr {
                    inner.vma_map.remove_range(
                        addr,
                        err.progress.processed_end,
                        |removed_start, removed_end, region| {
                            dec_removed_shared_mapcount(removed_start, removed_end, region);
                        },
                    );
                    inner.total_pages = inner.total_pages.saturating_sub(total_unmapped);
                }
                klog_info!("process_vm_munmap: unmap failed: {:?}", err.err);
                inner.vm_space = Some(vm_space_taken);
                abort_guard.disarm();
                return -1;
            }
        };
        total_freed = total_freed.saturating_add(progress.unmapped_pages);
    }

    inner
        .vma_map
        .remove_range(addr, end, |removed_start, removed_end, region| {
            dec_removed_shared_mapcount(removed_start, removed_end, region);
        });

    inner.vm_space = Some(vm_space_taken);
    inner.total_pages = inner.total_pages.saturating_sub(total_freed);

    abort_guard.disarm();
    0
}

/// Change protection on a memory region.
pub fn process_vm_mprotect(process_id: u32, addr: u64, length: u64, prot: u64) -> i32 {
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

    let slot = match find_slot_for_pid(process_id) {
        Some(s) => s,
        None => return -1,
    };
    let mut proc = PROCESS_VMS[slot].lock();
    if proc.process_id != process_id {
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

/// Per-page snapshot tuple captured under the parent's per-process
/// lock during `process_vm_clone_cow`'s phase 1: `(vaddr, paddr,
/// legacy PageFlags bits)`. The walkers below consume this snapshot
/// instead of re-reading the parent's PML4 — that read previously
/// happened with the parent's lock dropped, racing any concurrent
/// parent-side mapping.
type ClonePageSnapshot = (u64, PhysAddr, u64);

/// Captured parent-VMA + snapshot tuple. Boxed-Vec'd into a single
/// owned handle so the outer `process_vm_clone_cow` body never has to
/// hold the lock across a stack-allocated snapshot.
type CloneVmaEntry = (u64, u64, VmaRegion, KVec<ClonePageSnapshot>);

/// Phase 1 of `process_vm_clone_cow`: under the parent's per-process
/// lock, snapshot scalar fields, the VMA list, and a per-VMA
/// `(vaddr, paddr, flags)` snapshot built via OSTD cursor reads. Marks
/// every writable+user page in anonymous VMAs as COW in the parent's
/// OSTD half before leaving the lock.
///
/// Returns `None` if the parent slot has no page directory.
///
/// `#[inline(never)]` so the helper's stack frame doesn't fold into
/// `process_vm_clone_cow`, which keeps the outer body under the
/// 2 KiB stack-size razor.
#[inline(never)]
fn clone_cow_snapshot_parent(
    parent_slot: usize,
    parent_id: u32,
) -> Option<(u64, u64, u64, u64, u64, u64, u64, u32, KVec<CloneVmaEntry>)> {
    let mut guard = PROCESS_VMS[parent_slot].lock();
    if guard.process_id != parent_id || guard.page_dir.is_null() {
        klog_info!("process_vm_clone_cow: Parent has no page directory");
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
        // SlopRing regions are not inherited (close-on-fork, SLOPRING
        // § 14): capture an empty snapshot and never mark them COW. The
        // child-side walk skips `is_ring()` VMAs entirely.
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

/// Per-VMA inner walk for shared (memfd) regions in `process_vm_clone_cow`.
/// Maps the parent's existing physical pages (captured in `snapshot`)
/// directly into the child's OSTD VmSpace, inheriting the parent's
/// permissions verbatim. No COW marker — the child shares the same
/// memfd pages. Each child mapping DOES take a MetaSlot ref (the
/// `from_in_use` inside `ostd_map_4kb_user`), layered on top of the
/// memfd object's own owning ref; the page frees only when the memfd
/// and every mapping have dropped their ref. Returns the number of
/// pages mapped, or `Err(())` on the first failure.
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

/// Per-VMA inner walk for anonymous regions in `process_vm_clone_cow`.
/// Maps the captured parent pages into the child's OSTD VmSpace with
/// `WRITABLE` cleared and the COW marker set. The parent's COW mark
/// already landed via `ostd_mark_cow_4kb` during the phase-1 snapshot;
/// META_SLOTS bookkeeping for the additional child reference is handled
/// inside `ostd_map_4kb_user` (the second `wrap_user_paddr` for a paddr
/// does `from_in_use`, bumping the META_SLOTS ref count). Returns the
/// number of pages walked, or `Err(())` on first failure.
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

/// Clone address space with COW for fork(). Returns child PID or INVALID_PROCESS_ID.
pub fn process_vm_clone_cow(parent_id: u32) -> u32 {
    let parent_slot = match find_slot_for_pid(parent_id) {
        Some(s) => s,
        None => {
            klog_info!(
                "process_vm_clone_cow: Parent process {} not found",
                parent_id
            );
            return INVALID_PROCESS_ID;
        }
    };

    // Phase 1 — under the parent's per-process lock — snapshot parent
    // scalar fields, the VMA list, and a per-VMA (vaddr, paddr, flags)
    // snapshot via OSTD cursor reads. Anonymous VMAs also get their
    // writable+user pages marked COW in the parent's OSTD half before
    // we drop the lock. Extracted into a helper so its stack frame
    // does not fold into this function's frame.
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
    ) = match clone_cow_snapshot_parent(parent_slot, parent_id) {
        Some(t) => t,
        None => return INVALID_PROCESS_ID,
    };

    // Phase 1: allocate child slot under global lock.
    let (child_slot, child_id, child_generation) = {
        let mut alloc = VM_SLOT_ALLOC.lock();
        if alloc.num_processes >= MAX_PROCESSES as u32 {
            klog_info!("process_vm_clone_cow: Maximum processes reached");
            return INVALID_PROCESS_ID;
        }
        let mut found_slot = None;
        for i in 0..MAX_PROCESSES {
            let pid = slot_pid_lock_free(&PROCESS_VMS[i]);
            if pid == INVALID_PROCESS_ID {
                found_slot = Some(i);
                break;
            }
        }
        let child_slot = match found_slot {
            Some(s) => s,
            None => {
                klog_info!("process_vm_clone_cow: No free process slots");
                return INVALID_PROCESS_ID;
            }
        };
        let child_id = alloc.next_process_id;
        alloc.next_process_id += 1;
        alloc.num_processes += 1;
        let child_generation = alloc.alloc_generation();
        (child_slot, child_id, child_generation)
    };

    // Phase 2: allocate physical resources (no locks held).
    let pml4_phys = alloc_kernel_page();
    if pml4_phys.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate PML4");
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let pml4_virt = pml4_phys.to_virt();
    if pml4_virt.is_null() {
        klog_info!("process_vm_clone_cow: No HHDM mapping for PML4");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let _ = hhdm_fill_bytes(pml4_virt, 0, PAGE_SIZE_4KB as usize, 0);

    let child_page_dir = kmalloc(core::mem::size_of::<ProcessPageDir>()) as *mut ProcessPageDir;
    if child_page_dir.is_null() {
        klog_info!("process_vm_clone_cow: Failed to allocate page directory struct");
        free_page_frame(pml4_phys);
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }
    let child_mm_ctx_id = crate::mmu::alloc_mm_context_id();
    // OSTD `VmSpace::new` populates the child's kernel half via
    // `KERNEL_MASTER_PML4`. The frame this descriptor accounts for stays
    // empty and is never installed in CR3.
    let child_init = ProcessPageDir::new(pml4_phys, child_id, child_mm_ctx_id);
    // `child_page_dir` came from `kmalloc(size_of::<ProcessPageDir>())`
    // — slot is valid and exclusively owned. OSTD's `init_slot`
    // helper carries the one `ptr::write` so this site stays in safe Rust.
    ProcessPageDir::init_in_kmalloc_slot(child_page_dir, child_init);

    // Allocate the child's framekernel-correct OSTD VmSpace alongside
    // the legacy ProcessPageDir (transitional dual-allocation). The
    // child's mm_ctx_id was just alloc'd above; thread it into the
    // OSTD handle for LUF hook routing. On VmSpace::new failure we
    // roll back the legacy frames and the slot count.
    let child_vm_space = match VmSpace::new() {
        Ok(s) => s,
        Err(_) => {
            klog_info!(
                "process_vm_clone_cow: VmSpace::new failed for child PID {}",
                child_id
            );
            free_page_frame(pml4_phys);
            kfree(child_page_dir as *mut _);
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
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
            free_page_frame(pml4_phys);
            kfree(child_page_dir as *mut _);
            VM_SLOT_ALLOC.lock().num_processes -= 1;
            return INVALID_PROCESS_ID;
        }
    };

    // Phase 3: initialize child slot and perform COW page walk.
    // We hold the child slot lock while building VMA map + page tables.
    // The parent's page_dir pointer is stable (read from snapshot; parent
    // cannot be destroyed while fork is in progress -- scheduler guarantees).
    let mut cow_pages: u32 = 0;
    let mut clone_failed = false;

    {
        let mut child = PROCESS_VMS[child_slot].lock();
        child.process_id = child_id;
        child.generation = child_generation;
        child.page_dir = KernelSync::new(child_page_dir);
        child.vm_space = Some(child_vm_space_arc);
        child.vma_map.clear();
        child.code_start = parent_code_start;
        child.data_start = parent_data_start;
        child.heap_start = parent_heap_start;
        child.heap_end = parent_heap_end;
        child.heap_break = parent_heap_break;
        child.stack_start = parent_stack_start;
        child.stack_end = parent_stack_end;
        child.total_pages = 0;
        child.flags = parent_flags;

        // Walk parent's VMA list (from snapshot Vec).
        for (vma_start, vma_end, parent_region, snapshot) in parent_vmas.iter() {
            let vma_start = *vma_start;
            let vma_end = *vma_end;
            // SlopRing regions are NOT inherited: the child's ring fd is
            // close-on-fork (SLOPRING § 14), and the SQ/CQ is SPSC, so a
            // second producer in the child is forbidden by construction.
            // Skip the VMA entirely — the child gets no ring mapping.
            if parent_region.is_ring() {
                continue;
            }
            let is_shared_vma = parent_region.is_shared();

            let child_region = if is_shared_vma {
                // Shared memfd: inherit directly, no COW
                parent_region.clone()
            } else {
                // Anonymous: mark as COW
                let mut r = parent_region.clone();
                r.cow = true;
                r
            };

            child.vma_map.insert(vma_start, vma_end, child_region);
            if let Some(memfd_handle) = parent_region.memfd_handle() {
                crate::memfd::memfd_inc_mapcount_by(
                    memfd_handle,
                    vma_page_count(vma_start, vma_end),
                );
            }

            // Pull a mutable handle to the child's OSTD VmSpace once
            // per VMA. The child slot lock above is the sole owner of
            // the KArc, so KArc::get_mut succeeds. Parent-side OSTD
            // mark_cow ran inline under the parent lock in the
            // snapshot phase above.
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
                Err(()) => {
                    clone_failed = true;
                }
            }

            if clone_failed {
                break;
            }
        }

        if !clone_failed {
            child.total_pages = cow_pages;
        }
    }

    // paging_mark_cow defers TLB invalidation -- flush once for all COW pages.
    if cow_pages > 0 {
        tlb::flush_all();
    }

    if clone_failed {
        klog_info!("process_vm_clone_cow: Clone failed, cleaning up");
        {
            let mut child = PROCESS_VMS[child_slot].lock();
            // Drop the child's OSTD VmSpace — Drop walks the partial
            // user-half tree and reclaims every leaf frame.
            let _ = child.vm_space.take();
            teardown_inner_mappings(&mut child);
        }
        let child_pml4 = page_dir_pml4_phys(child_page_dir);
        if !child_pml4.is_null() {
            free_page_frame(child_pml4);
        }
        kfree(child_page_dir as *mut _);
        PROCESS_VMS[child_slot].lock().reset();
        VM_SLOT_ALLOC.lock().num_processes -= 1;
        return INVALID_PROCESS_ID;
    }

    klog_info!(
        "process_vm_clone_cow: Cloned PID {} -> PID {} ({} COW pages)",
        parent_id,
        child_id,
        cow_pages
    );

    tlb::register_process_tlb(child_id);

    child_id
}
