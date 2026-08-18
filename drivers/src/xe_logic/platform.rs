//! PCI Device-ID → Intel display platform table.
//!
//! The GMD_ID version register does not exist on pre-Meteor-Lake silicon (the
//! target a7a8 included), so the Device ID is the only ground truth here.

pub const PCI_VENDOR_INTEL: u16 = 0x8086;

/// Intel display-engine generation.
///
/// Every generation in this table shares one register-block layout (SKL+
/// universal-plane group bases, fixed 0x1000 per-pipe stride), so the tag
/// distinguishes the display IP *version*, not a different register map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum XeDisplayGen {
    /// Tiger Lake, Alder Lake-S — display IP version 12.
    Gen12,
    /// Alder Lake-P, Raptor Lake-P/-U — display IP version 13.
    Gen13,
}

impl XeDisplayGen {
    pub const fn display_ip_version(self) -> u8 {
        match self {
            Self::Gen12 => 12,
            Self::Gen13 => 13,
        }
    }

    /// Primary-plane group bases pipe A 0x70180 / B 0x71180 / C 0x72180, stepped
    /// by the 0x1000 per-pipe stride — true for every generation supported here.
    pub const fn uses_skl_universal_plane_regs(self) -> bool {
        true
    }
}

/// A supported Intel integrated display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XePlatform {
    pub name: &'static str,
    pub generation: XeDisplayGen,
}

impl XePlatform {
    pub const fn display_ip_version(self) -> u8 {
        self.generation.display_ip_version()
    }

    pub const fn uses_skl_universal_plane_regs(self) -> bool {
        self.generation.uses_skl_universal_plane_regs()
    }
}

const fn platform(name: &'static str, generation: XeDisplayGen) -> XePlatform {
    XePlatform { name, generation }
}

/// Intentionally narrow: the target a7a8 family plus a representative spread of
/// the Gen12/Gen13 platforms that share these register conventions.
const XE_PLATFORM_TABLE: &[(u16, XePlatform)] = &[
    (0x9a40, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    (0x9a49, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    (0x9a60, platform("Tiger Lake GT1", XeDisplayGen::Gen12)),
    (0x9a68, platform("Tiger Lake GT1", XeDisplayGen::Gen12)),
    (0x9a78, platform("Tiger Lake GT2", XeDisplayGen::Gen12)),
    (0x4680, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4682, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4690, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x4692, platform("Alder Lake-S GT1", XeDisplayGen::Gen12)),
    (0x46a6, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
    (0x46a8, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
    (0x46aa, platform("Alder Lake-P GT2", XeDisplayGen::Gen13)),
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

pub const fn is_intel_vendor(vendor: u16) -> bool {
    vendor == PCI_VENDOR_INTEL
}

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
