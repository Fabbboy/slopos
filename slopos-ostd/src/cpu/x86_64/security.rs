//! Supervisor-mode CPU security features: PGE, SMEP, SMAP.
//!
//! Detected once per CPU and enabled unconditionally where advertised; they
//! never change at runtime. `CR4.SMAP` is what forces every user-memory access
//! through the dedicated `raw_usercopy` path — anything else faults.
//!
//! Called from `boot::early_init` on the BSP and `boot::smp::ap_entry_rust` on
//! each AP, after SSE/XSAVE are brought up and before any user-visible work.

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
/// Idempotent: safe to call on every CPU during bring-up.
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
