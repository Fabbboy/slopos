//! Per-CPU ASID pool and context-switch fast path.
//!
//! Each CPU owns [`DYN_ASIDS_PER_CPU`] PCID slots; the hardware PCID loaded
//! into `CR3[11:0]` is `slot_index + 1`, and PCID `0` is reserved for
//! kernel-only address-space loads. Without `CR4.PCIDE` (unsupported, or
//! force-disabled by `mmu::errata`) every call returns `Cr3Value::kernel(phys)`
//! and the hardware flushes the whole TLB on each CR3 reload.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_arch::cpu::IrqDisabled;
use slopos_arch::cpu::control_regs::{Cr4Flags, read_cr4, write_cr4};
use slopos_arch::cpu::cpuid::{
    CPUID_FEAT_ECX_PCID, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_INVPCID,
    cpuid, cpuid_count,
};
use slopos_arch::pcr::MAX_CPUS;
use slopos_ostd::sync::PerCpuSlot;
use slopos_ostd::{klog_debug, klog_info};

use super::cr3::{Cr3Value, MmContextId, Pcid};

/// `INVPCID` descriptor type 3: all-context invalidation, excluding globals.
/// SDM Vol 2A §3.2.
const INVPCID_ALL_CONTEXT_NO_GLOBALS: u64 = 3;

/// PCIDs in use on any one CPU are `0` (kernel) plus `1..=16` (user), which fits
/// the 12-bit PCID space and leaves room for the KPTI bit-11 user/kernel pair
/// (see `mmu::kpti`).
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

/// Mutated only by its owning CPU with interrupts disabled. Counters are
/// `AtomicU64` only so other CPUs can read them non-authoritatively.
#[repr(C, align(64))]
struct PerCpuAsids {
    slots: [AsidSlot; DYN_ASIDS_PER_CPU],
    last_loaded_slot: u8,
    next_asid: u8,
    _padding: [u8; 6],
    pub hot_hits: AtomicU64,
    pub gen_refresh: AtomicU64,
    pub misses: AtomicU64,
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

/// `forget_context_local` takes a caller-supplied index, so nothing about the
/// signature confines a caller to its own slot; `PerCpuSlot` checks the
/// exclusivity the discipline assumes.
static PER_CPU: [PerCpuSlot<PerCpuAsids>; MAX_CPUS] = {
    const INIT: PerCpuSlot<PerCpuAsids> = PerCpuSlot::new(PerCpuAsids::new());
    [INIT; MAX_CPUS]
};

/// Software mirror of `CR4.PCIDE`, set once at BSP boot, so the
/// context-switch hot path never reads `CR4`.
static PCID_ENABLED: AtomicBool = AtomicBool::new(false);
static INVPCID_AVAILABLE: AtomicBool = AtomicBool::new(false);

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

/// BSP bring-up for the PCID fast path.
///
/// Must run with CR3's PCID bits already zero. Returns `true` if PCID is
/// live system-wide after this call.
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

/// AP counterpart to [`init_bsp`]. Must run after the AP has inherited the
/// kernel page tables but before its first user-address CR3 load.
pub fn init_ap() {
    if pcid_enabled() {
        set_cr4_pcide_local();
    }
}

/// Assumes PCID is supported and the current CR3 has PCID == 0.
fn set_cr4_pcide_local() {
    let cr4 = read_cr4() | Cr4Flags::PCIDE.bits();
    write_cr4(cr4);
    klog_debug!("MMU: CR4.PCIDE set on this CPU");
}

/// Flush all entries belonging to a specific PCID (INVPCID type 1).
pub fn flush_pcid(pcid: u16) {
    if !invpcid_available() {
        // A CR3 reload drops only the tag it loads, so with `CR4.PCIDE` set it
        // would leave `pcid` untouched whenever `pcid` is not the tag in CR3.
        flush_local_all_contexts();
        return;
    }
    slopos_ostd::cpu::x86_64::tlb::invpcid(1, pcid, 0);
}

/// Invalidate every non-global TLB entry on this CPU, across **all** PCIDs.
///
/// `mov cr3` is not this operation: with `CR4.PCIDE` set it invalidates only
/// the tag in the value being loaded, and [`select_cr3`] hands back `NOFLUSH`
/// values, so a frame handed to a new owner stays readable and writable
/// through a surviving stale entry.
///
/// The `CR4.PGE` fallback works because a `MOV to CR4` that *changes* PGE
/// invalidates the whole TLB including globals, whichever way round it
/// started; the CR3 reload is correct only because PCID is off there, making
/// CR3's tag space a single context.
pub fn flush_local_all_contexts() {
    if !pcid_enabled() {
        slopos_arch::cpu::flush_tlb_all();
        return;
    }
    if invpcid_available() {
        slopos_ostd::cpu::x86_64::tlb::invpcid(INVPCID_ALL_CONTEXT_NO_GLOBALS, 0, 0);
        return;
    }
    let cr4 = read_cr4();
    write_cr4(cr4 ^ Cr4Flags::PGE.bits());
    write_cr4(cr4);
}

/// Choose a CR3 value for the next address space to load on this CPU.
///
/// Interrupts must be off.
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
        with_asid_state(cpu_id, |state| {
            state.legacy.fetch_add(1, Ordering::Relaxed);
        });
        return Cr3Value::kernel(pml4_phys);
    }

    if !ctx_id.is_valid() {
        return Cr3Value::new(pml4_phys, Pcid::KERNEL, true);
    }

    with_asid_state(cpu_id, |state| {
        if (state.last_loaded_slot as usize) < DYN_ASIDS_PER_CPU {
            let slot = &state.slots[state.last_loaded_slot as usize];
            if slot.ctx_id == ctx_id.raw() && slot.tlb_gen == ctx_tlb_gen {
                state.hot_hits.fetch_add(1, Ordering::Relaxed);
                let pcid = Pcid::new_unchecked(state.last_loaded_slot as u16 + 1);
                return Cr3Value::new(pml4_phys, pcid, true);
            }
        }

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

        let slot_idx = state.next_asid as usize;
        state.next_asid = ((state.next_asid as usize + 1) % DYN_ASIDS_PER_CPU) as u8;
        let pcid_raw = slot_idx as u16 + 1;

        if state.slots[slot_idx].ctx_id != 0 {
            flush_pcid(pcid_raw);
        }
        state.slots[slot_idx] = AsidSlot {
            ctx_id: ctx_id.raw(),
            tlb_gen: ctx_tlb_gen,
        };
        state.last_loaded_slot = slot_idx as u8;
        state.misses.fetch_add(1, Ordering::Relaxed);

        // NOFLUSH=false so the processor drops any stale non-global entries
        // still cached under this PCID.
        Cr3Value::new(pml4_phys, Pcid::new_unchecked(pcid_raw), false)
    })
}

/// Panics on a declined borrow: every caller runs with interrupts off on the
/// slot's own CPU, so a decline means two writers reached one slot.
#[inline]
fn with_asid_state<R>(cpu_id: usize, f: impl FnOnce(&mut PerCpuAsids) -> R) -> R {
    IrqDisabled::with(|irq| {
        PER_CPU[cpu_id]
            .with_mut(irq, f)
            .expect("asid: per-CPU slot already borrowed")
    })
}

/// Drop this CPU's slot bindings for a destroyed `MmContextId`. Each CPU must
/// call it for its own state; cross-CPU invalidation rides the shootdown path.
pub fn forget_context_local(cpu_id: usize, ctx_id: MmContextId) {
    if cpu_id >= MAX_CPUS || !pcid_enabled() {
        return;
    }
    with_asid_state(cpu_id, |state| {
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
    });
}
