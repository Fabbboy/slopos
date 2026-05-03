//! Supervisor-mode CPU security features: PGE, SMEP, SMAP.
//!
//! These controls are detected once per CPU at boot and enabled unconditionally
//! when the hardware advertises support. They never change at runtime.
//!
//! - **PGE** (`CR4.PGE`) — keeps page-table entries marked with the `G`
//!   (global) bit resident in the TLB across CR3 reloads. Kernel mappings
//!   are identical in every address space, so flushing them on every context
//!   switch is pure waste. The kernel sets the global bit on its own leaf
//!   PTEs; PGE is the CPU-side switch that makes those bits meaningful.
//! - **SMEP** (`CR4.SMEP`) — kernel (ring 0) code fetches from user-mode
//!   pages trigger `#PF`. Mitigates return-to-user-code exploits.
//! - **SMAP** (`CR4.SMAP`) — kernel data reads/writes to user-mode pages
//!   trigger `#PF` unless `RFLAGS.AC` is set. Forces the kernel to use the
//!   dedicated `raw_usercopy` path for all user-memory accesses; anything
//!   else faults loudly instead of silently corrupting across rings.
//!
//! This module is called from `boot::early_init` on the BSP and from
//! `boot::smp::ap_entry_rust` on each AP, after SSE/XSAVE are brought up
//! and before any user-visible work runs.

use super::control_regs::{Cr4Flags, read_cr4, write_cr4};
use crate::arch::x86_64::cpuid::{
    CPUID_FEAT_EDX_PGE, CPUID_LEAF_FEATURES, CPUID_LEAF_STRUCTURED_EXT, CPUID_SEXT_EBX_SMAP,
    CPUID_SEXT_EBX_SMEP, cpuid, cpuid_count,
};

/// Which supervisor-mode features the current CPU advertises.
#[derive(Clone, Copy, Debug, Default)]
pub struct SupervisorFeatures {
    pub pge: bool,
    pub smep: bool,
    pub smap: bool,
}

impl SupervisorFeatures {
    pub fn detect() -> Self {
        let (_, _, _, edx1) = cpuid(CPUID_LEAF_FEATURES);
        let pge = (edx1 & CPUID_FEAT_EDX_PGE) != 0;

        let (max_leaf, _, _, _) = cpuid(0);
        let (smep, smap) = if max_leaf >= CPUID_LEAF_STRUCTURED_EXT {
            let (_, ebx, _, _) = cpuid_count(CPUID_LEAF_STRUCTURED_EXT, 0);
            (
                (ebx & CPUID_SEXT_EBX_SMEP) != 0,
                (ebx & CPUID_SEXT_EBX_SMAP) != 0,
            )
        } else {
            (false, false)
        };

        Self { pge, smep, smap }
    }
}

/// Enable the supervisor-mode features advertised by this CPU.
///
/// Idempotent: writing the same bits twice is a no-op. Safe to call on
/// every CPU during bring-up. Returns the feature mask actually applied.
pub fn enable_supervisor_features() -> SupervisorFeatures {
    let feats = SupervisorFeatures::detect();
    let mut cr4 = read_cr4();

    if feats.pge {
        cr4 |= Cr4Flags::PGE.bits();
    }
    if feats.smep {
        cr4 |= Cr4Flags::SMEP.bits();
    }
    if feats.smap {
        cr4 |= Cr4Flags::SMAP.bits();
    }

    write_cr4(cr4);

    feats
}

/// Read back which features are active in `CR4` on the current CPU.
pub fn active_supervisor_features() -> SupervisorFeatures {
    let flags = Cr4Flags::from_bits_truncate(read_cr4());
    SupervisorFeatures {
        pge: flags.contains(Cr4Flags::PGE),
        smep: flags.contains(Cr4Flags::SMEP),
        smap: flags.contains(Cr4Flags::SMAP),
    }
}
