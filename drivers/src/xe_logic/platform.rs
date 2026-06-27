//! PCI Device-ID → Intel display platform table.
//!
//! Pure data: a supported integrated display is identified by its PCI Device ID
//! alone. The GMD_ID version register does not exist on pre-Meteor-Lake silicon
//! (the target a7a8 included), so it is never consulted here — the Device ID is
//! the only ground truth. Each entry carries a display-IP-generation tag that
//! selects the register conventions the hardware half drives.

/// PCI vendor ID shared by every Intel graphics device.
pub const PCI_VENDOR_INTEL: u16 = 0x8086;

/// Intel display-engine generation.
///
/// The register-block layout this driver targets — SKL+ "universal plane" group
/// bases stepped by a fixed 0x1000 stride across pipes A/B/C — is shared by every
/// generation in this table; the tag therefore distinguishes the display IP
/// *version* (and any per-generation quirks the sequencing half cares about)
/// rather than a wholly different register map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum XeDisplayGen {
    /// Gen12 display engine (Tiger Lake, Alder Lake-S): display IP version 12.
    Gen12,
    /// Gen13 display engine (Alder Lake-P, Raptor Lake-P/-U): display IP version 13.
    Gen13,
}

impl XeDisplayGen {
    /// Display IP major version number for this generation.
    pub const fn display_ip_version(self) -> u8 {
        match self {
            Self::Gen12 => 12,
            Self::Gen13 => 13,
        }
    }

    /// Whether the SKL+ universal-plane register conventions apply (primary-plane
    /// group bases pipe A 0x70180 / B 0x71180 / C 0x72180, stepped by the 0x1000
    /// per-pipe stride). True for every generation this driver supports.
    pub const fn uses_skl_universal_plane_regs(self) -> bool {
        true
    }
}

/// A supported Intel integrated display, identified purely by PCI Device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XePlatform {
    /// Human-readable platform name for logs and diagnostics.
    pub name: &'static str,
    /// Display-engine generation / register-convention tag.
    pub generation: XeDisplayGen,
}

impl XePlatform {
    /// Display IP major version (12 or 13) for this platform.
    pub const fn display_ip_version(self) -> u8 {
        self.generation.display_ip_version()
    }

    /// Whether this platform uses the SKL+ universal-plane register conventions.
    pub const fn uses_skl_universal_plane_regs(self) -> bool {
        self.generation.uses_skl_universal_plane_regs()
    }
}

const fn platform(name: &'static str, generation: XeDisplayGen) -> XePlatform {
    XePlatform { name, generation }
}

/// Known Intel display Device IDs (under vendor 0x8086), each paired with its
/// platform descriptor. Identification is by Device ID only — no MMIO version
/// register is read. The list is intentionally narrow: the target a7a8 family
/// plus a representative spread of the Gen12/Gen13 platforms that share these
/// register conventions.
const XE_PLATFORM_TABLE: &[(u16, XePlatform)] = &[
    // Tiger Lake-U/-H (Gen12, display IP version 12).
    (0x9a40, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    (0x9a49, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    (0x9a60, platform("Tiger Lake GT1", XeDisplayGen::Gen12)),
    (0x9a68, platform("Tiger Lake GT1", XeDisplayGen::Gen12)),
    (0x9a78, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    // Alder Lake-S (Gen12, display IP version 12).
    (0x4680, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4682, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4690, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4692, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    // Alder Lake-P GT2 (Gen13, display IP version 13).
    (0x46a6, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
    (0x46a8, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
    (0x46aa, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
    // Alder Lake-P / Raptor Lake-P/-U A7xx family (Gen13, display IP version 13).
    // 0xa7a8 is the SlopOS target laptop's iGPU (alderlake_p / raptorlake_p).
    (0xa720, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa721, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7a0, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7a1, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (
        0xa7a8,
        platform("Alder Lake-P / Raptor Lake-P", XeDisplayGen::Gen13),
    ),
    (0xa7a9, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7aa, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7ab, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7ac, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
    (0xa7ad, platform("Raptor Lake-P/-U", XeDisplayGen::Gen13)),
];

/// True when `vendor` is Intel's PCI vendor ID.
pub const fn is_intel_vendor(vendor: u16) -> bool {
    vendor == PCI_VENDOR_INTEL
}

/// Resolve a PCI (vendor, device) pair to its display platform, or `None` for a
/// non-Intel vendor or an unrecognised Device ID. Identification is by Device ID
/// only — never by an MMIO version register.
pub fn identify(vendor: u16, device: u16) -> Option<XePlatform> {
    if !is_intel_vendor(vendor) {
        return None;
    }
    for &(did, plat) in XE_PLATFORM_TABLE {
        if did == device {
            return Some(plat);
        }
    }
    None
}
