//! Per-CPU errata gating for the `mmu` subsystem.
//!
//! Single authoritative location for silicon-bug workarounds that
//! affect PCID / INVPCID correctness.
//!
//! Known errata handled here:
//!
//!   - **Intel 12th Gen (Alder Lake) / 13th Gen (Raptor Lake) E-cores**
//!     — `INVLPG` combined with PCID can leave global mappings resident
//!     on efficiency cores. Intel erratum ADL059 / RPL023. Reference:
//!     `Phoronix: Linux disables PCID on Alder/Raptor Lake` (2023) and
//!     the Intel errata spec updates. Fixed microcode was not broadly
//!     available at Linux's workaround-merge time. We mirror the Linux
//!     mainline decision: force PCID off on affected CPUs and let the
//!     non-PCID fallback path carry the machine.
//!
//! The `should_disable_pcid` function is consulted by `mmu::asid::init_bsp`
//! before it writes `CR4.PCIDE` — if the answer is true, we never enable
//! PCID system-wide and every CR3 write degrades to the legacy
//! `Cr3Value::kernel(phys)` path.

use slopos_arch::cpu::cpuid::{cpu_brand_string, cpu_family_model_stepping};

/// Family 6, model 0x97 — Alder Lake / Raptor Lake client desktop/mobile
/// (the hybrid E-core + P-core SKUs that carry the erratum).
const INTEL_ALDER_LAKE_MODEL: u8 = 0x97;
/// Family 6, model 0x9A — Alder Lake mobile-H / Raptor Lake refresh with
/// the same microarchitectural E-core bug surface.
const INTEL_ALDER_LAKE_L_MODEL: u8 = 0x9A;
/// Family 6, model 0xB7 / 0xBA / 0xBE / 0xBF — Raptor Lake refresh S/HX.
const INTEL_RAPTOR_LAKE_MODELS: &[u8] = &[0xB7, 0xBA, 0xBE, 0xBF];

fn is_genuine_intel() -> bool {
    // CPUID leaf 0 vendor string ("GenuineIntel"). `cpu_brand_string`
    // reads the extended leaves (0x8000_0002..0x8000_0004) which are
    // Intel-only but also used by AMD — to be unambiguous we pattern
    // match on the leaf-0 vendor.
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

/// Does the current CPU require PCID to be disabled?
///
/// Conservative: returns `true` if and only if we're positively sure
/// we're running on silicon that carries the INVLPG+PCID erratum. Any
/// failure to read CPUID (shouldn't happen in `no_std` kernel context)
/// defaults to `false` — we'd rather run with PCID and hit a tractable
/// stale-TLB on a bad CPU than leave the fast path disabled on everyone.
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

/// Prefer brand-string match only for diagnostic log lines — the
/// detection itself relies on family/model/stepping because early Intel
/// pre-production CPUs sometimes ship with placeholder brand strings.
pub fn brand_for_logs() -> [u8; 48] {
    cpu_brand_string()
}
