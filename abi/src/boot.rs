use crate::DisplayInfo;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct LimineMemmapEntry {
    pub base: u64,
    pub length: u64,
    pub typ: u64,
}

#[repr(C)]
pub struct LimineMemmapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *const *const LimineMemmapEntry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum MemoryRegionKind {
    Usable = 0,
    Reserved = 1,
    AcpiReclaimable = 2,
    AcpiNvs = 3,
    BadMemory = 4,
    BootloaderReclaimable = 5,
    KernelAndModules = 6,
    Framebuffer = 7,
}

impl MemoryRegionKind {
    #[inline]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Usable)
    }

    #[inline]
    pub const fn is_reclaimable(self) -> bool {
        matches!(self, Self::AcpiReclaimable | Self::BootloaderReclaimable)
    }

    #[inline]
    pub const fn is_reserved(self) -> bool {
        matches!(self, Self::Reserved | Self::AcpiNvs | Self::BadMemory)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Usable => "Usable",
            Self::Reserved => "Reserved",
            Self::AcpiReclaimable => "ACPI Reclaimable",
            Self::AcpiNvs => "ACPI NVS",
            Self::BadMemory => "Bad Memory",
            Self::BootloaderReclaimable => "Bootloader Reclaimable",
            Self::KernelAndModules => "Kernel and Modules",
            Self::Framebuffer => "Framebuffer",
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub kind: MemoryRegionKind,
}

impl MemoryRegion {
    #[inline]
    pub const fn new(base: u64, length: u64, kind: MemoryRegionKind) -> Self {
        Self { base, length, kind }
    }

    #[inline]
    pub const fn end(&self) -> u64 {
        self.base.saturating_add(self.length)
    }

    #[inline]
    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.end()
    }

    #[inline]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct BootFramebuffer {
    pub address: *mut u8,
    pub info: DisplayInfo,
}

impl BootFramebuffer {
    #[inline]
    pub const fn new(address: *mut u8, info: DisplayInfo) -> Self {
        Self { address, info }
    }

    #[inline]
    pub const fn size_bytes(&self) -> usize {
        self.info.pitch as usize * self.info.height as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub hhdm_offset: u64,
    pub cmdline: Option<&'static str>,
    pub framebuffer: Option<BootFramebuffer>,
    pub kernel_phys_base: u64,
    pub kernel_virt_base: u64,
    pub rsdp_address: u64,
}

impl BootInfo {
    pub const fn new() -> Self {
        Self {
            hhdm_offset: 0,
            cmdline: None,
            framebuffer: None,
            kernel_phys_base: 0,
            kernel_virt_base: 0,
            rsdp_address: 0,
        }
    }

    #[inline]
    pub const fn has_hhdm(&self) -> bool {
        self.hhdm_offset != 0
    }

    #[inline]
    pub const fn has_framebuffer(&self) -> bool {
        self.framebuffer.is_some()
    }

    #[inline]
    pub const fn has_rsdp(&self) -> bool {
        self.rsdp_address != 0
    }

    #[inline]
    pub const fn phys_to_virt(&self, phys: u64) -> u64 {
        phys.wrapping_add(self.hhdm_offset)
    }
}

impl Default for BootInfo {
    fn default() -> Self {
        Self::new()
    }
}
