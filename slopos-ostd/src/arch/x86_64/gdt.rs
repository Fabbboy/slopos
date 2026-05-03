//! Global Descriptor Table (GDT) — Single Source of Truth
//!
//! This module defines **all** GDT-related types, constants, and descriptor
//! constructors for SlopOS.  Every other crate (`boot`, `lib/pcr`, etc.)
//! imports from here rather than keeping private copies.
//!
//! # What lives here
//!
//! | Item | Purpose |
//! |------|---------|
//! | [`SegmentSelector`] | Type-safe segment selector with named constants |
//! | [`Tss64`] | 64-bit Task State Segment |
//! | [`GdtTssEntry`] | 16-byte TSS descriptor inside the GDT |
//! | [`GdtLayout`] | Complete GDT: 5 standard entries + TSS descriptor |
//! | [`GdtDescriptor`] | `lgdt` operand (limit + base) |
//! | `GDT_*` constants | Access-byte, flag, and descriptor constants |
//! | [`gdt_make_descriptor`] | `const fn` descriptor constructor |

// =========================================================================
// Segment Selector
// =========================================================================

/// x86_64 segment selector.
///
/// Layout (16 bits):
/// - Bits 0-1: Requested Privilege Level (RPL)
/// - Bit 2: Table Indicator (0 = GDT, 1 = LDT)
/// - Bits 3-15: Descriptor index
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SegmentSelector(pub u16);

impl SegmentSelector {
    /// Null selector (index 0, GDT, RPL 0).
    pub const NULL: Self = Self(0);

    /// Kernel code segment (GDT index 1, RPL 0) = 0x08.
    pub const KERNEL_CODE: Self = Self::new(1, false, 0);

    /// Kernel data segment (GDT index 2, RPL 0) = 0x10.
    pub const KERNEL_DATA: Self = Self::new(2, false, 0);

    /// User data segment (GDT index 3, RPL 3) = 0x1B.
    ///
    /// Must come before user code in GDT for SYSRET compatibility.
    pub const USER_DATA: Self = Self::new(3, false, 3);

    /// User code segment (GDT index 4, RPL 3) = 0x23.
    pub const USER_CODE: Self = Self::new(4, false, 3);

    /// TSS segment (GDT index 5, RPL 0) = 0x28.
    pub const TSS: Self = Self::new(5, false, 0);

    /// Create a new segment selector.
    #[inline]
    pub const fn new(index: u16, ldt: bool, rpl: u8) -> Self {
        let ti = if ldt { 1 << 2 } else { 0 };
        Self((index << 3) | ti | (rpl as u16 & 0x3))
    }

    /// Get the descriptor table index.
    #[inline]
    pub const fn index(self) -> u16 {
        self.0 >> 3
    }

    /// Check if this selector references the LDT.
    #[inline]
    pub const fn is_ldt(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Get the requested privilege level (0-3).
    #[inline]
    pub const fn rpl(self) -> u8 {
        (self.0 & 0x3) as u8
    }

    /// Get the raw selector value for loading into segment register.
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

// =========================================================================
// GDT Descriptor Access Byte Fields (bits 40-47)
// =========================================================================

/// Present bit (bit 7).
pub const GDT_ACCESS_PRESENT: u8 = 1 << 7;
/// DPL = 0 — Ring 0 / Kernel (bits 5-6).
pub const GDT_ACCESS_DPL_KERNEL: u8 = 0 << 5;
/// DPL = 3 — Ring 3 / User (bits 5-6).
pub const GDT_ACCESS_DPL_USER: u8 = 3 << 5;
/// Segment type bit (bit 4) — 1 for code/data segment.
pub const GDT_ACCESS_SEGMENT: u8 = 1 << 4;
/// Code segment type: executable, readable, non-conforming.
pub const GDT_ACCESS_CODE_TYPE: u8 = 0b1010;
/// Data segment type: writable, expand-up.
pub const GDT_ACCESS_DATA_TYPE: u8 = 0b0010;
/// TSS Available (64-bit) access byte — 0x89.
pub const GDT_ACCESS_TSS_AVAILABLE: u8 = 0x89;

// =========================================================================
// GDT Flags (bits 52-55 of descriptor)
// =========================================================================

/// Granularity flag (G=1) — limit in 4KB units.
pub const GDT_FLAG_GRANULARITY: u8 = 1 << 3;
/// Long mode flag (L=1) — 64-bit code segment.
pub const GDT_FLAG_LONG_MODE: u8 = 1 << 1;
/// Combined flags for 64-bit segments: G=1, D/B=0, L=1, AVL=0 = 0xA.
pub const GDT_FLAGS_64BIT: u8 = GDT_FLAG_GRANULARITY | GDT_FLAG_LONG_MODE;

// =========================================================================
// Descriptor Constructor
// =========================================================================

/// Build a 64-bit GDT descriptor from individual fields.
///
/// Bit layout of a GDT entry:
/// - Bits  0-15: Limit (low 16 bits)
/// - Bits 16-31: Base (low 16 bits)
/// - Bits 32-39: Base (middle 8 bits)
/// - Bits 40-47: Access byte
/// - Bits 48-51: Limit (high 4 bits)
/// - Bits 52-55: Flags
/// - Bits 56-63: Base (high 8 bits)
pub const fn gdt_make_descriptor(
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    limit_high: u8,
    flags: u8,
    base_high: u8,
) -> u64 {
    (limit_low as u64)
        | ((base_low as u64) << 16)
        | ((base_mid as u64) << 32)
        | ((access as u64) << 40)
        | ((limit_high as u64) << 48)
        | ((flags as u64) << 52)
        | ((base_high as u64) << 56)
}

// =========================================================================
// Standard 64-bit Descriptors (base/limit ignored by hardware, set to max)
// =========================================================================

/// Null descriptor — GDT index 0.
pub const GDT_NULL_DESCRIPTOR: u64 = 0;

/// Kernel code segment (Ring 0, 64-bit).
pub const GDT_KERNEL_CODE_DESCRIPTOR: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);

/// Kernel data segment (Ring 0, 64-bit).
pub const GDT_KERNEL_DATA_DESCRIPTOR: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_KERNEL | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);

/// User data segment (Ring 3, 64-bit) — must precede user code for SYSRET.
pub const GDT_USER_DATA_DESCRIPTOR: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_DATA_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);

/// User code segment (Ring 3, 64-bit).
pub const GDT_USER_CODE_DESCRIPTOR: u64 = gdt_make_descriptor(
    0xFFFF,
    0,
    0,
    GDT_ACCESS_PRESENT | GDT_ACCESS_DPL_USER | GDT_ACCESS_SEGMENT | GDT_ACCESS_CODE_TYPE,
    0xF,
    GDT_FLAGS_64BIT,
    0,
);

/// Standard GDT entries in the order expected by the kernel.
///
/// Index 0: null, 1: kernel code, 2: kernel data, 3: user data, 4: user code.
pub const GDT_STANDARD_ENTRIES: [u64; 5] = [
    GDT_NULL_DESCRIPTOR,
    GDT_KERNEL_CODE_DESCRIPTOR,
    GDT_KERNEL_DATA_DESCRIPTOR,
    GDT_USER_DATA_DESCRIPTOR,
    GDT_USER_CODE_DESCRIPTOR,
];

// =========================================================================
// Hardware Structures
// =========================================================================

/// Number of 8-byte GDT entries (5 standard + 2 for 16-byte TSS descriptor).
pub const GDT_ENTRY_COUNT: usize = 7;

/// 64-bit Task State Segment.
///
/// Hardware-defined layout — do not reorder or add fields.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tss64 {
    pub reserved0: u32,
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    pub reserved1: u64,
    pub ist: [u64; 7],
    pub reserved2: u64,
    pub reserved3: u16,
    pub iomap_base: u16,
}

impl Tss64 {
    /// Zeroed TSS, suitable for static initialization.
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: 0,
        }
    }
}

/// TSS descriptor entry (16 bytes for 64-bit mode, occupies two GDT slots).
///
/// Hardware-defined layout — do not reorder or add fields.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct GdtTssEntry {
    pub limit_low: u16,
    pub base_low: u16,
    pub base_mid: u8,
    pub access: u8,
    pub granularity: u8,
    pub base_high: u8,
    pub base_upper: u32,
    pub reserved: u32,
}

impl GdtTssEntry {
    /// Zeroed TSS descriptor.
    pub const fn new() -> Self {
        Self {
            limit_low: 0,
            base_low: 0,
            base_mid: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
            base_upper: 0,
            reserved: 0,
        }
    }

    /// Populate this descriptor to point at the given TSS.
    ///
    /// # Arguments
    /// * `tss_base` — virtual address of the [`Tss64`] instance
    /// * `tss_limit` — `size_of::<Tss64>() - 1`
    pub fn set_base_limit(&mut self, tss_base: u64, tss_limit: u16) {
        self.limit_low = tss_limit & 0xFFFF;
        self.base_low = (tss_base & 0xFFFF) as u16;
        self.base_mid = ((tss_base >> 16) & 0xFF) as u8;
        self.access = GDT_ACCESS_TSS_AVAILABLE;
        self.granularity = (((tss_limit as u32) >> 16) & 0x0F) as u8;
        self.base_high = ((tss_base >> 24) & 0xFF) as u8;
        self.base_upper = (tss_base >> 32) as u32;
        self.reserved = 0;
    }
}

/// Complete GDT: 5 standard entries (null + 4 segments) + 16-byte TSS descriptor.
///
/// Hardware-defined layout — do not reorder or add fields.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct GdtLayout {
    /// Standard GDT entries: null, kernel code, kernel data, user data, user code.
    pub entries: [u64; 5],
    /// TSS descriptor (16 bytes, spans two GDT slots).
    pub tss_entry: GdtTssEntry,
}

impl GdtLayout {
    /// Zeroed GDT layout.
    pub const fn new() -> Self {
        Self {
            entries: [0; 5],
            tss_entry: GdtTssEntry::new(),
        }
    }

    /// Populate the standard segment descriptors.
    pub fn load_standard_entries(&mut self) {
        self.entries = GDT_STANDARD_ENTRIES;
    }

    /// Populate the TSS descriptor to point at the given TSS.
    pub fn load_tss(&mut self, tss: &Tss64) {
        let tss_base = tss as *const Tss64 as u64;
        let tss_limit = (core::mem::size_of::<Tss64>() as u16) - 1;
        self.tss_entry.set_base_limit(tss_base, tss_limit);
    }

    /// Byte size of the entire GDT (for the `lgdt` limit field).
    pub const fn byte_size() -> usize {
        core::mem::size_of::<Self>()
    }
}

/// `lgdt` operand: limit (byte count - 1) + linear base address.
#[repr(C, packed)]
pub struct GdtDescriptor {
    pub limit: u16,
    pub base: u64,
}

impl GdtDescriptor {
    /// Build a descriptor pointing at the given [`GdtLayout`].
    pub fn from_layout(layout: &GdtLayout) -> Self {
        Self {
            limit: (GdtLayout::byte_size() - 1) as u16,
            base: layout as *const GdtLayout as u64,
        }
    }
}

// =========================================================================
// GDT install — issues lgdt + segment reload + ltr
// =========================================================================

/// Load the given [`GdtLayout`] into GDTR, reload the segment registers
/// to point at `KERNEL_CODE` / `KERNEL_DATA`, and load the TSS.
///
/// # Safety
///
/// SAFETY (Inv. 2): the layout must remain valid memory for as long as
/// the GDT is in use (typically `'static`). Caller is responsible for
/// ensuring `tss_selector` references a populated TSS descriptor inside
/// the same layout.
#[cfg(target_arch = "x86_64")]
pub unsafe fn install(layout: &GdtLayout, tss_selector: SegmentSelector) {
    let gdtr = GdtDescriptor::from_layout(layout);

    // SAFETY: gdtr lives on the local stack; lgdt reads its 10 bytes
    // synchronously and the loaded GDT base points back into `layout`,
    // which the caller guarantees outlives this call. Inv. 2.
    unsafe {
        core::arch::asm!(
            "lgdt [{0}]",
            in(reg) &gdtr,
            options(nostack, preserves_flags),
        );
    }

    // SAFETY: a far return through KERNEL_CODE reloads CS atomically; the
    // subsequent data-segment writes are valid because KERNEL_DATA is
    // populated by `load_standard_entries`. Inv. 2.
    unsafe {
        core::arch::asm!(
            "pushq ${code}",
            "lea 2f(%rip), %rax",
            "pushq %rax",
            "lretq",
            "2:",
            "movw ${data}, %ax",
            "movw %ax, %ds",
            "movw %ax, %es",
            "movw %ax, %ss",
            "movw %ax, %fs",
            "movw %ax, %gs",
            code = const SegmentSelector::KERNEL_CODE.bits() as usize,
            data = const SegmentSelector::KERNEL_DATA.bits() as usize,
            out("rax") _,
            options(att_syntax, nostack),
        );
    }

    let tss_sel = tss_selector.bits();
    // SAFETY: ltr reads the TSS descriptor at index `tss_sel >> 3` from
    // the freshly-loaded GDT; descriptor is populated by
    // `GdtLayout::load_tss`. Inv. 2.
    unsafe {
        core::arch::asm!(
            "ltr {0:x}",
            in(reg) tss_sel,
            options(nostack, preserves_flags),
        );
    }
}

// =========================================================================
// Compile-time safety assertions
// =========================================================================

const _: () = {
    // SegmentSelector raw values
    assert!(SegmentSelector::KERNEL_CODE.0 == 0x08);
    assert!(SegmentSelector::KERNEL_DATA.0 == 0x10);
    assert!(SegmentSelector::USER_DATA.0 == 0x1B);
    assert!(SegmentSelector::USER_CODE.0 == 0x23);
    assert!(SegmentSelector::TSS.0 == 0x28);

    // Hardware structure sizes
    assert!(core::mem::size_of::<Tss64>() == 104);
    assert!(core::mem::size_of::<GdtTssEntry>() == 16);
    // GdtLayout = 5*8 + 16 = 56 bytes
    assert!(core::mem::size_of::<GdtLayout>() == 56);
    // GdtDescriptor = 2 + 8 = 10 bytes
    assert!(core::mem::size_of::<GdtDescriptor>() == 10);

    // GDT_ENTRY_COUNT covers the standard entries (5) + TSS spanning 2 slots
    assert!(GDT_ENTRY_COUNT == 7);

    // TSS access byte value
    assert!(GDT_ACCESS_TSS_AVAILABLE == 0x89);

    // Flags sanity
    assert!(GDT_FLAGS_64BIT == 0x0A);
};

// =========================================================================
// IST Slots — typed indices into Tss64.ist[]
// =========================================================================

/// Named IST (Interrupt Stack Table) slot.
///
/// Replaces the previous `u8` index parameter on `gdt_set_ist` /
/// `idt_set_ist`. Out-of-range values (0, 8+) are non-representable, so
/// the runtime checks in those functions become compile-time errors.
///
/// The layout assignments here mirror `IST_CONFIGS` in
/// `boot/src/ist_stacks.rs`. Changing one without the other breaks the
/// IDT/TSS contract.
///
/// # Compile-fail examples
///
/// ```compile_fail
/// use slopos_arch::arch::gdt::IstSlot;
/// let _ = IstSlot::S0;  // does not exist (0 is "no IST")
/// ```
///
/// ```compile_fail
/// use slopos_arch::arch::gdt::IstSlot;
/// let _ = IstSlot::S8;  // does not exist (IST has 7 slots)
/// ```
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IstSlot {
    /// IST1 — bound to #DF (double fault).
    DoubleFault = 1,
    /// IST2 — bound to stack-fault exception.
    StackFault = 2,
    /// IST3 — bound to #GP.
    GeneralProtection = 3,
    /// IST4 — bound to #PF.
    PageFault = 4,
    /// IST5 — bound to keyboard IRQ.
    KeyboardIrq = 5,
    /// IST6 — bound to mouse IRQ.
    MouseIrq = 6,
    /// IST7 — reserved for future use.
    Reserved7 = 7,
}

impl IstSlot {
    /// Numeric IST index (1..=7) as expected by IDT entries.
    pub const fn as_index(self) -> u8 {
        self as u8
    }

    /// Zero-based offset into `Tss64.ist[]` (0..=6).
    pub const fn as_tss_offset(self) -> usize {
        (self as u8 - 1) as usize
    }

    /// Construct from the runtime `u8` index. Returns `None` for 0
    /// or 8+ (which `IstSlot` does not represent).
    pub const fn from_index(idx: u8) -> Option<Self> {
        match idx {
            1 => Some(IstSlot::DoubleFault),
            2 => Some(IstSlot::StackFault),
            3 => Some(IstSlot::GeneralProtection),
            4 => Some(IstSlot::PageFault),
            5 => Some(IstSlot::KeyboardIrq),
            6 => Some(IstSlot::MouseIrq),
            7 => Some(IstSlot::Reserved7),
            _ => None,
        }
    }

    /// Diagnostic name.
    pub const fn name(self) -> &'static str {
        match self {
            IstSlot::DoubleFault => "DoubleFault",
            IstSlot::StackFault => "StackFault",
            IstSlot::GeneralProtection => "GeneralProtection",
            IstSlot::PageFault => "PageFault",
            IstSlot::KeyboardIrq => "KeyboardIrq",
            IstSlot::MouseIrq => "MouseIrq",
            IstSlot::Reserved7 => "Reserved7",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_selector_values() {
        assert_eq!(SegmentSelector::KERNEL_CODE.bits(), 0x08);
        assert_eq!(SegmentSelector::KERNEL_DATA.bits(), 0x10);
        assert_eq!(SegmentSelector::USER_DATA.bits(), 0x1B);
        assert_eq!(SegmentSelector::USER_CODE.bits(), 0x23);
        assert_eq!(SegmentSelector::TSS.bits(), 0x28);
    }

    #[test]
    fn segment_selector_decomposition() {
        let sel = SegmentSelector::USER_CODE;
        assert_eq!(sel.index(), 4);
        assert_eq!(sel.rpl(), 3);
        assert!(!sel.is_ldt());
    }

    #[test]
    fn ist_slot_indexing() {
        assert_eq!(IstSlot::DoubleFault.as_index(), 1);
        assert_eq!(IstSlot::PageFault.as_index(), 4);
        assert_eq!(IstSlot::Reserved7.as_index(), 7);
        assert_eq!(IstSlot::DoubleFault.as_tss_offset(), 0);
        assert_eq!(IstSlot::Reserved7.as_tss_offset(), 6);
    }
}
