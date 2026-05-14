//! Per-CPU ASID pool and context-switch fast path.
//!
//! Mirrors Linux's `arch/x86/mm/tlb.c` design:
//!   - Each CPU owns [`DYN_ASIDS_PER_CPU`] PCID slots (16 here; Linux uses 6).
//!     The hardware PCID loaded into `CR3[11:0]` is `slot_index + 1`.
//!     PCID `0` is reserved for kernel-only address-space loads.
//!   - A slot remembers which `MmContextId` last occupied it and what
//!     `tlb_gen` was current at the time. On switch-in, if the requested
//!     `(ctx_id, tlb_gen)` still matches, the CPU writes CR3 with the
//!     `NOFLUSH` bit so its TLB entries survive.
//!   - A stale generation reuses the same slot but issues `INVPCID` type 1
//!     (single-context flush) to drop the old translations.
//!   - A miss rotates `next_asid` forward, evicts whatever was in that slot
//!     (flushing it), binds the new context, and writes a `NOFLUSH=0` CR3.
//!
//! CPUs that do not support `CR4.PCIDE` (or where the errata layer in
//! `mmu::errata` has force-disabled it) fall through to the legacy path:
//! every call returns `Cr3Value::kernel(phys)` and the hardware flushes
//! the whole TLB on each CR3 reload — still correct, just slower.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_arch::cpu::control_regs::{Cr4Flags, read_cr4, write_cr4};
use slopos_arch::cpu::cpuid::{
    CPUID_FEAT_ECX_PCID, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_INVPCID,
    cpuid, cpuid_count,
};
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::{klog_debug, klog_info};

use super::cr3::{Cr3Value, MmContextId, Pcid};

/// Number of dynamic per-CPU PCID slots.
///
/// Linux uses `TLB_NR_DYN_ASIDS = 6`; we have enough headroom and cheap
/// cache to justify 16. Slot index 0 is unused (it maps to PCID 1 —
/// kernel PCID 0 is reserved). With 16 slots, PCIDs in use on any one
/// CPU are `0` (kernel) plus `1..=16` (user). That fits comfortably in
/// the 12-bit PCID space and leaves room for the KPTI bit-11 user/kernel
/// pair (see `mmu::kpti`).
pub const DYN_ASIDS_PER_CPU: usize = 16;

#[derive(Clone, Copy)]
struct AsidSlot {
    ctx_id: u64,
    tlb_gen: u64,
}

impl AsidSlot {
    const EMPTY: Self = Self {
        ctx_id: 0,
        tlb_gen: 0,
    };
}

/// Per-CPU ASID state. Mutated only by its owning CPU, with interrupts
/// disabled (callers are the context switcher and shootdown IPI handler).
/// Counters are `AtomicU64` so they can be read non-authoritatively from
/// other CPUs (debug / diagnostics).
#[repr(C, align(64))]
struct PerCpuAsids {
    slots: [AsidSlot; DYN_ASIDS_PER_CPU],
    /// Which slot was bound by the most recent `switch_to`.
    last_loaded_slot: u8,
    /// Round-robin cursor for slot allocation on miss.
    next_asid: u8,
    /// Whether the last switch wrote CR3 at all. Purely diagnostic.
    _padding: [u8; 6],
    /// Fast path: context + generation match → CR3 with NOFLUSH.
    pub hot_hits: AtomicU64,
    /// Slot hit, generation stale → INVPCID single-context + reuse.
    pub gen_refresh: AtomicU64,
    /// No slot held this context → rotated into a fresh slot.
    pub misses: AtomicU64,
    /// Legacy non-PCID path taken (CR4.PCIDE disabled at boot).
    pub legacy: AtomicU64,
}

impl PerCpuAsids {
    const fn new() -> Self {
        Self {
            slots: [AsidSlot::EMPTY; DYN_ASIDS_PER_CPU],
            last_loaded_slot: u8::MAX, // force miss on first switch
            next_asid: 0,
            _padding: [0; 6],
            hot_hits: AtomicU64::new(0),
            gen_refresh: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            legacy: AtomicU64::new(0),
        }
    }
}

/// `KernelSync` wraps the `UnsafeCell` so the surrounding cell auto-derives
/// `Sync`; per-CPU interrupts-off single-writer discipline gates real
/// access. Cross-CPU reads touch only the `AtomicU64` counters above.
struct PerCpuAsidsCell(slopos_ostd::sync::KernelSync<UnsafeCell<PerCpuAsids>>);

static PER_CPU: [PerCpuAsidsCell; MAX_CPUS] = {
    const INIT: PerCpuAsidsCell = PerCpuAsidsCell(slopos_ostd::sync::KernelSync::new(
        UnsafeCell::new(PerCpuAsids::new()),
    ));
    [INIT; MAX_CPUS]
};

/// Global switch: set once at BSP boot after CPUID + errata check. Mirrors
/// the hardware's `CR4.PCIDE` but in software so the context-switch hot
/// path never reads `CR4`.
static PCID_ENABLED: AtomicBool = AtomicBool::new(false);
static INVPCID_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Is the PCID fast path active on this machine?
#[inline]
pub fn pcid_enabled() -> bool {
    PCID_ENABLED.load(Ordering::Relaxed)
}

#[inline]
pub fn invpcid_available() -> bool {
    INVPCID_AVAILABLE.load(Ordering::Relaxed)
}

fn cpu_has_pcid() -> bool {
    let (_, _, ecx, _) = cpuid(CPUID_LEAF_FEATURES);
    (ecx & CPUID_FEAT_ECX_PCID) != 0
}

fn cpu_has_invpcid() -> bool {
    let (max_leaf, _, _, _) = cpuid(0);
    if max_leaf < CPUID_LEAF_STRUCTURED_EXT {
        return false;
    }
    let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_STRUCTURED_EXT, 0);
    (ebx & CPUID_SEXT_EBX_INVPCID) != 0
}

/// BSP bring-up: probe PCID, probe INVPCID, enable `CR4.PCIDE` on the
/// current CPU, and flip the global feature switch.
///
/// Must run with CR3's PCID bits already zero (they are, because all
/// prior writes went through `Cr3Value::kernel`). Safe to call again on
/// APs via [`init_ap`].
///
/// Returns `true` if PCID is live system-wide after this call.
pub fn init_bsp() -> bool {
    let pcid = cpu_has_pcid();
    let invpcid = cpu_has_invpcid();

    INVPCID_AVAILABLE.store(invpcid, Ordering::Relaxed);

    if !pcid {
        klog_info!("MMU: PCID unsupported; falling back to legacy CR3 flushes");
        PCID_ENABLED.store(false, Ordering::Release);
        return false;
    }

    if super::errata::should_disable_pcid() {
        klog_info!(
            "MMU: PCID disabled by errata ({}); falling back to legacy CR3 flushes",
            super::errata::active_erratum_tag().unwrap_or("unknown")
        );
        PCID_ENABLED.store(false, Ordering::Release);
        return false;
    }

    set_cr4_pcide_local();
    PCID_ENABLED.store(true, Ordering::Release);

    klog_info!(
        "MMU: PCID enabled (INVPCID={}, slots/CPU={}, backend={})",
        invpcid,
        DYN_ASIDS_PER_CPU,
        super::rar::backend().name()
    );
    true
}

/// AP bring-up counterpart to [`init_bsp`]. Must run after the AP has
/// inherited the kernel page tables but before its first user-address
/// CR3 load.
pub fn init_ap() {
    if pcid_enabled() {
        set_cr4_pcide_local();
    }
}

/// Raw per-CPU CR4.PCIDE enable. Assumes PCID is supported and the
/// current CR3 has PCID == 0.
fn set_cr4_pcide_local() {
    let cr4 = read_cr4() | Cr4Flags::PCIDE.bits();
    write_cr4(cr4);
    klog_debug!("MMU: CR4.PCIDE set on this CPU");
}

/// Flush all entries belonging to a specific PCID (INVPCID type 1).
///
/// Used when reassigning a PCID slot to a different context, or when a
/// stale generation forces us to drop the slot's prior TLB caches.
pub fn flush_pcid(pcid: u16) {
    if !invpcid_available() {
        // Fallback: full flush via CR3 reload. Correct — just more
        // expensive because it takes out the other PCIDs too.
        let cr3 = slopos_arch::cpu::read_cr3();
        slopos_arch::cpu::write_cr3(cr3);
        return;
    }
    slopos_ostd::cpu::x86_64::tlb::invpcid(1, pcid, 0);
}

/// Choose a CR3 value for the next address space to load on this CPU.
///
/// This is the **hot path** of every context switch. Three outcomes:
///
///   1. *Hot hit* — same `(ctx_id, tlb_gen)` as last time → write CR3
///      with `NOFLUSH`, TLB retained.
///   2. *Gen refresh* — a slot already held this `ctx_id` but its
///      `tlb_gen` is stale (an unmap happened on another CPU and bumped
///      the generation). Issue `INVPCID` type 1 on the PCID and reuse
///      the slot with NOFLUSH.
///   3. *Miss* — no slot had this `ctx_id`. Advance `next_asid`, evict
///      whatever was there (full PCID flush), install the new binding,
///      and write CR3 without NOFLUSH.
///
/// When PCID is globally disabled, always returns
/// `Cr3Value::kernel(phys)` which corresponds to PCID 0 + NOFLUSH=0.
///
/// Callers: `scheduler::prepare_switch_to`. Interrupts must be off.
pub fn select_cr3(
    cpu_id: usize,
    ctx_id: MmContextId,
    pml4_phys: PhysAddr,
    ctx_tlb_gen: u64,
) -> Cr3Value {
    if cpu_id >= MAX_CPUS {
        return Cr3Value::kernel(pml4_phys);
    }

    if !pcid_enabled() {
        // Per-CPU, IRQs off, single writer on the current core; the
        // `cell_get_mut` helper folds the `&mut *get()` reborrow.
        PER_CPU[cpu_id]
            .0
            .cell_get_mut()
            .legacy
            .fetch_add(1, Ordering::Relaxed);
        return Cr3Value::kernel(pml4_phys);
    }

    // Kernel-only switches (ctx_id invalid) use PCID 0 unconditionally.
    if !ctx_id.is_valid() {
        return Cr3Value::new(pml4_phys, Pcid::KERNEL, true);
    }

    // Per-CPU, IRQs off, single writer on the current core.
    let state = PER_CPU[cpu_id].0.cell_get_mut();

    // Fast path: slot currently loaded still valid.
    if (state.last_loaded_slot as usize) < DYN_ASIDS_PER_CPU {
        let slot = &state.slots[state.last_loaded_slot as usize];
        if slot.ctx_id == ctx_id.raw() && slot.tlb_gen == ctx_tlb_gen {
            state.hot_hits.fetch_add(1, Ordering::Relaxed);
            let pcid = Pcid::new_unchecked(state.last_loaded_slot as u16 + 1);
            return Cr3Value::new(pml4_phys, pcid, true);
        }
    }

    // Linear scan for a pre-existing slot for this ctx_id.
    for (i, slot) in state.slots.iter_mut().enumerate() {
        if slot.ctx_id == ctx_id.raw() {
            let pcid_raw = i as u16 + 1;
            if slot.tlb_gen != ctx_tlb_gen {
                flush_pcid(pcid_raw);
                slot.tlb_gen = ctx_tlb_gen;
                state.gen_refresh.fetch_add(1, Ordering::Relaxed);
            } else {
                state.hot_hits.fetch_add(1, Ordering::Relaxed);
            }
            state.last_loaded_slot = i as u8;
            return Cr3Value::new(pml4_phys, Pcid::new_unchecked(pcid_raw), true);
        }
    }

    // Miss: rotate into the next slot, evict prior contents.
    let slot_idx = state.next_asid as usize;
    state.next_asid = ((state.next_asid as usize + 1) % DYN_ASIDS_PER_CPU) as u8;
    let pcid_raw = slot_idx as u16 + 1;

    // Flush whatever the prior tenant left behind. Cheap on INVPCID-
    // capable hardware; a full CR3 reload otherwise.
    if state.slots[slot_idx].ctx_id != 0 {
        flush_pcid(pcid_raw);
    }
    state.slots[slot_idx] = AsidSlot {
        ctx_id: ctx_id.raw(),
        tlb_gen: ctx_tlb_gen,
    };
    state.last_loaded_slot = slot_idx as u8;
    state.misses.fetch_add(1, Ordering::Relaxed);

    // NOFLUSH=false because we must have the processor flush any stale
    // non-global entries cached with this PCID value since we set the
    // slot above. In practice INVPCID already did that, but pairing
    // NOFLUSH=false with the allocation keeps correctness simple.
    Cr3Value::new(pml4_phys, Pcid::new_unchecked(pcid_raw), false)
}

/// Invalidate all per-CPU slot bindings for a particular context.
///
/// Called when an `MmContextId` is being destroyed so its TLB entries
/// cannot linger under their previously-assigned PCIDs on any CPU this
/// function is invoked on. Each CPU must call this for its own state.
/// Cross-CPU invalidation rides on the shootdown path.
pub fn forget_context_local(cpu_id: usize, ctx_id: MmContextId) {
    if cpu_id >= MAX_CPUS || !pcid_enabled() {
        return;
    }
    let state = unsafe { &mut *PER_CPU[cpu_id].0.get().get() };
    for (i, slot) in state.slots.iter_mut().enumerate() {
        if slot.ctx_id == ctx_id.raw() {
            flush_pcid(i as u16 + 1);
            *slot = AsidSlot::EMPTY;
        }
    }
    if state.last_loaded_slot as usize != DYN_ASIDS_PER_CPU
        && (state.last_loaded_slot as usize) < DYN_ASIDS_PER_CPU
        && state.slots[state.last_loaded_slot as usize].ctx_id == 0
    {
        state.last_loaded_slot = u8::MAX;
    }
}
