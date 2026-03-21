/// IA32_APIC_BASE MSR value (MSR 0x1B).
///
/// Layout:
/// - Bits 0-7: Reserved
/// - Bit 8: BSP flag (1 = bootstrap processor)
/// - Bit 9: Reserved
/// - Bit 10: x2APIC enable
/// - Bit 11: APIC global enable
/// - Bits 12-51: APIC base physical address (4KB aligned)
/// - Bits 52-63: Reserved
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ApicBaseMsr(pub u64);

impl ApicBaseMsr {
    pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
    pub const BSP: u64 = 1 << 8;
    pub const X2APIC_ENABLE: u64 = 1 << 10;
    pub const GLOBAL_ENABLE: u64 = 1 << 11;

    #[inline]
    pub const fn address(self) -> u64 {
        self.0 & Self::ADDR_MASK
    }

    #[inline]
    pub const fn is_bsp(self) -> bool {
        self.0 & Self::BSP != 0
    }

    #[inline]
    pub const fn is_x2apic(self) -> bool {
        self.0 & Self::X2APIC_ENABLE != 0
    }

    #[inline]
    pub const fn is_enabled(self) -> bool {
        self.0 & Self::GLOBAL_ENABLE != 0
    }

    #[inline]
    pub const fn new(base: u64, bsp: bool, x2apic: bool, enable: bool) -> Self {
        let mut val = base & Self::ADDR_MASK;
        if bsp {
            val |= Self::BSP;
        }
        if x2apic {
            val |= Self::X2APIC_ENABLE;
        }
        if enable {
            val |= Self::GLOBAL_ENABLE;
        }
        Self(val)
    }
}
