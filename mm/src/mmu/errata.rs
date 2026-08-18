//! Per-CPU errata gating for the `mmu` subsystem.
//!
//! Single authoritative location for silicon-bug workarounds that
//! affect PCID / INVPCID correctness.
//!
//! Intel 12th Gen (Alder Lake) / 13th Gen (Raptor Lake) E-cores, erratum
//! ADL059 / RPL023: `INVLPG` combined with PCID can leave global mappings
//! resident on efficiency cores. `mmu::asid::init_bsp` consults
//! [`should_disable_pcid`] before writing `CR4.PCIDE`, so an affected CPU
//! never enables PCID and every CR3 write takes the legacy non-PCID path.

use slopos_arch::cpu::cpuid::{cpu_brand_string, cpu_family_model_stepping};

/// Family 6, model 0x97 — Alder Lake / Raptor Lake client desktop/mobile.
const INTEL_ALDER_LAKE_MODEL: u8 = 0x97;
/// Family 6, model 0x9A — Alder Lake mobile-H / Raptor Lake refresh.
const INTEL_ALDER_LAKE_L_MODEL: u8 = 0x9A;
/// Family 6, model 0xB7 / 0xBA / 0xBE / 0xBF — Raptor Lake refresh S/HX.
const INTEL_RAPTOR_LAKE_MODELS: &[u8] = &[0xB7, 0xBA, 0xBE, 0xBF];

fn is_genuine_intel() -> bool {
    // Match the leaf-0 vendor string: the extended brand-string leaves
    // (0x8000_0002..0x8000_0004) are also populated by AMD, so they cannot
    // identify the vendor unambiguously.
    use slopos_arch::cpu::cpuid::cpu_vendor_string;
    let v = cpu_vendor_string();
    v.starts_with(b"GenuineIntel")
}

fn is_affected_alder_raptor_model(family: u8, model: u8) -> bool {
    if family != 6 {
        return false;
    }
    if model == INTEL_ALDER_LAKE_MODEL || model == INTEL_ALDER_LAKE_L_MODEL {
        return true;
    }
    INTEL_RAPTOR_LAKE_MODELS.contains(&model)
}

/// `true` only for silicon positively identified as carrying the
/// INVLPG+PCID erratum: an unreadable CPUID defaults to `false`, preferring a
/// tractable stale-TLB on a bad CPU over losing the fast path everywhere.
pub fn should_disable_pcid() -> bool {
    if !is_genuine_intel() {
        return false;
    }
    let (family, model, _stepping) = cpu_family_model_stepping();
    is_affected_alder_raptor_model(family, model)
}

/// Short human-readable tag for boot logs. `None` when no erratum is
/// active.
pub fn active_erratum_tag() -> Option<&'static str> {
    if should_disable_pcid() {
        Some("intel-adl-rpl-invlpg-pcid")
    } else {
        None
    }
}

/// Diagnostics only — detection uses family/model/stepping because
/// pre-production CPUs sometimes ship with placeholder brand strings.
pub fn brand_for_logs() -> [u8; 48] {
    cpu_brand_string()
}
