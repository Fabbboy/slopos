# Plan 001: Page Table Infrastructure Refactoring

**Status**: Draft  
**Priority**: High  
**Scope**: `mm/src/paging.rs`, `abi/src/arch/x86_64/`  
**Estimated Effort**: 3-4 phases over multiple sessions  

---

## 1. Problem Statement

The current `mm/src/paging.rs` (850 lines) contains severe DRY violations. Six functions implement nearly identical 4-level page table traversal logic:

| Function | Lines | Purpose |
|----------|-------|---------|
| `virt_to_phys_for_dir()` | 181-235 | Translate virtual to physical address |
| `map_page_in_directory()` | 255-399 | Create a new page mapping |
| `unmap_page_in_directory()` | 439-535 | Remove an existing mapping |
| `paging_is_user_accessible()` | 811-849 | Check if address has user permissions |
| `paging_mark_range_user()` | 732-810 | Mark range as user-accessible |
| `get_page_size()` | 704-731 | Determine page size at address |

Each function manually walks PML4 -> PDPT -> PD -> PT with copy-pasted patterns for:
- Checking entry presence
- Handling huge pages (1GB at L3, 2MB at L2)
- Resolving physical addresses via HHDM
- Index calculation from virtual addresses

This duplication creates maintenance burden and risk of divergent bugs.

---

## 2. Research Summary

### 2.1 Linux Kernel Approach

Linux uses a **callback-driven walker** with level folding for architecture portability.

**Key patterns**:
```c
// mm/pagewalk.c
struct mm_walk_ops {
    int (*pgd_entry)(pgd_t *pgd, unsigned long addr, ...);
    int (*p4d_entry)(p4d_t *p4d, unsigned long addr, ...);
    int (*pud_entry)(pud_t *pud, unsigned long addr, ...);
    int (*pmd_entry)(pmd_t *pmd, unsigned long addr, ...);
    int (*pte_entry)(pte_t *pte, unsigned long addr, ...);
    int (*pte_hole)(unsigned long addr, unsigned long next, int depth, ...);
};

enum page_walk_action {
    ACTION_SUBTREE,   // Descend to next level
    ACTION_CONTINUE,  // Skip this entry's subtree
    ACTION_AGAIN,     // Retry this entry
};
```

**Applicable insights**:
- Callback at each level allows flexible operations
- Action enum controls traversal without early returns
- Hole detection for unmapped ranges
- Level folding (`PTRS_PER_P?D = 1`) not needed for x86_64-only kernel

### 2.2 Rust x86_64 Crate Approach

The widely-used `rust-osdev/x86_64` crate provides type-safe abstractions.

**Key patterns**:
```rust
// PageTableLevel enum with runtime level tracking
pub enum PageTableLevel {
    One = 1,   // PT
    Two = 2,   // PD
    Three = 3, // PDPT
    Four = 4,  // PML4
}

impl PageTableLevel {
    pub const fn next_lower_level(self) -> Option<Self>;
    pub const fn table_address_space_alignment(self) -> u64;
}

// PageTableWalker for single-step traversal
struct PageTableWalker<P: PageTableFrameMapping> {
    page_table_frame_mapping: P,
}

impl PageTableWalker<P> {
    fn next_table(&self, entry: &PageTableEntry) -> Result<&PageTable, WalkError>;
    fn next_table_mut(&self, entry: &mut PageTableEntry) -> Result<&mut PageTable, WalkError>;
    fn create_next_table(&self, entry: &mut PageTableEntry, flags: PageTableFlags, 
                         allocator: &mut A) -> Result<&mut PageTable, CreateError>;
}

// Mapper trait generic over page sizes
pub trait Mapper<S: PageSize> {
    unsafe fn map_to_with_table_flags<A>(...) -> Result<MapperFlush<S>, MapToError<S>>;
    fn unmap(&mut self, page: Page<S>) -> Result<(PhysFrame<S>, MapperFlush<S>), UnmapError>;
    fn translate_page(&self, page: Page<S>) -> Result<PhysFrame<S>, TranslateError>;
}
```

**Applicable insights**:
- `PageTableLevel` enum for type-safe level representation
- `PageTableWalker` encapsulates phys->virt translation
- `PageTableFrameMapping` trait abstracts HHDM (we already have `PhysAddrHhdm`)
- Separate error types for different operations (Walk, Map, Unmap, Translate)
- Generic `Mapper<S: PageSize>` trait for different page sizes

### 2.3 Rust OS Ecosystem Standard Patterns

From analyzing zCore, hvisor, ArceOS, and other production Rust OS projects:

**Error handling consensus**:
```rust
/// Standard paging error type (used by zCore, hvisor, ArceOS, RVM)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingError {
    /// Cannot allocate memory for page tables
    NoMemory,
    /// The address is not properly aligned
    NotAligned,
    /// The mapping does not exist
    NotMapped,
    /// A mapping already exists at this address
    AlreadyMapped,
    /// Cannot traverse because parent entry is a huge page
    MappedToHugePage,
}

/// Standard result type alias
pub type PagingResult<T = ()> = Result<T, PagingError>;

/// Conversion to higher-level error types
impl From<PagingError> for KernelError { ... }
```

**Key conventions**:
- `PagingError` enum with 4-6 variants covering all cases
- `PagingResult<T = ()>` type alias with default unit type
- `From` implementations for error propagation
- `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` for ergonomics
- No `#[non_exhaustive]` - paging errors are well-defined

---

## 3. Design Goals

### 3.1 Must Have
- **Single traversal implementation**: One walk function, many operations
- **Type-safe level tracking**: `PageTableLevel` enum, not magic numbers
- **Proper error types**: `PagingResult<T>` replacing `c_int` return codes
- **Zero-cost abstractions**: Compile to same machine code as current hand-written loops
- **Huge page support**: 1GB, 2MB, 4KB unified handling
- **Composable operations**: Callback/visitor pattern for extensibility

### 3.2 Should Have
- **Testability**: Walker is pure, easy to unit test with mock tables
- **Documentation**: Rustdoc with examples for all public APIs
- **Gradual migration path**: Old and new can coexist during transition

### 3.3 Won't Have (for now)
- **5-level paging**: x86_64 LA57 support (no current use case)
- **Architecture abstraction**: Targeting x86_64 only
- **Runtime level folding**: Linux-style compile-time level removal

---

## 4. Architecture Design

### 4.1 Module Structure

```
abi/src/arch/x86_64/
├── mod.rs
├── paging.rs              # Existing: PageFlags, constants
├── page_table.rs          # NEW: PageTableLevel, PageTableEntry, PageTable
└── memory.rs              # Existing: Layout constants

mm/src/paging/
├── mod.rs                 # Re-exports, init_paging()
├── entry.rs               # NEW: Entry operations, flag manipulation
├── walker.rs              # NEW: PageTableWalker, WalkResult
├── mapper.rs              # NEW: PageMapper for map/unmap
├── translate.rs           # NEW: virt_to_phys and related
├── permissions.rs         # NEW: is_user_accessible, mark_range_user
├── directory.rs           # NEW: ProcessPageDir management
├── error.rs               # NEW: PagingError, PagingResult
└── legacy.rs              # TEMPORARY: Old code during migration
```

### 4.2 Core Types (abi layer)

These go in `abi/` because they're shared between kernel and potentially userland debugging tools.

```rust
// abi/src/arch/x86_64/page_table.rs

/// Page table hierarchy level (4 = PML4, 1 = PT)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PageTableLevel {
    /// Level 4: Page Map Level 4 (PML4)
    Four = 4,
    /// Level 3: Page Directory Pointer Table (PDPT) - can have 1GB huge pages
    Three = 3,
    /// Level 2: Page Directory (PD) - can have 2MB huge pages
    Two = 2,
    /// Level 1: Page Table (PT) - always 4KB pages
    One = 1,
}

impl PageTableLevel {
    /// Get the next lower level, or `None` if at level 1.
    #[inline]
    pub const fn next_lower(self) -> Option<Self> {
        match self {
            Self::Four => Some(Self::Three),
            Self::Three => Some(Self::Two),
            Self::Two => Some(Self::One),
            Self::One => None,
        }
    }

    /// Get the next higher level, or `None` if at level 4.
    #[inline]
    pub const fn next_higher(self) -> Option<Self> {
        match self {
            Self::One => Some(Self::Two),
            Self::Two => Some(Self::Three),
            Self::Three => Some(Self::Four),
            Self::Four => None,
        }
    }

    /// Get the page size if this level supports huge pages.
    /// Level 3 = 1GB, Level 2 = 2MB, Level 1 = 4KB, Level 4 = None
    #[inline]
    pub const fn page_size(self) -> Option<u64> {
        match self {
            Self::Three => Some(PAGE_SIZE_1GB),
            Self::Two => Some(PAGE_SIZE_2MB),
            Self::One => Some(PAGE_SIZE_4KB),
            Self::Four => None, // PML4 entries cannot be leaf entries
        }
    }

    /// Check if this level can contain huge page mappings.
    #[inline]
    pub const fn supports_huge_pages(self) -> bool {
        matches!(self, Self::Three | Self::Two)
    }

    /// Extract the 9-bit index for this level from a virtual address.
    #[inline]
    pub const fn index_of(self, vaddr: VirtAddr) -> usize {
        let shift = 12 + ((self as u8 - 1) * 9);
        ((vaddr.as_u64() >> shift) & 0x1FF) as usize
    }

    /// Get the address space covered by one entry at this level.
    #[inline]
    pub const fn entry_size(self) -> u64 {
        1u64 << (12 + ((self as u8 - 1) * 9))
    }

    /// Get the alignment mask for addresses at this level.
    #[inline]
    pub const fn align_mask(self) -> u64 {
        !(self.entry_size() - 1)
    }
}

/// A 64-bit page table entry.
/// 
/// This is a transparent wrapper around `u64` for zero-cost abstraction.
/// The actual bit layout is defined by x86_64 architecture.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    /// An empty (not present) entry.
    pub const EMPTY: Self = Self(0);

    /// Create an entry from a raw u64 value.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Get the raw u64 value.
    #[inline]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Check if the entry is present (P bit set).
    #[inline]
    pub const fn is_present(&self) -> bool {
        self.0 & PageFlags::PRESENT.bits() != 0
    }

    /// Check if this is a huge page entry (PS bit set).
    #[inline]
    pub const fn is_huge(&self) -> bool {
        self.0 & PageFlags::HUGE.bits() != 0
    }

    /// Check if this entry is user-accessible (U/S bit set).
    #[inline]
    pub const fn is_user(&self) -> bool {
        self.0 & PageFlags::USER.bits() != 0
    }

    /// Check if this entry is writable (R/W bit set).
    #[inline]
    pub const fn is_writable(&self) -> bool {
        self.0 & PageFlags::WRITABLE.bits() != 0
    }

    /// Check if the entry is unused (all bits zero).
    #[inline]
    pub const fn is_unused(&self) -> bool {
        self.0 == 0
    }

    /// Get the physical address from this entry (bits 12-51).
    #[inline]
    pub const fn address(&self) -> PhysAddr {
        PhysAddr::new(self.0 & PageFlags::ADDRESS_MASK)
    }

    /// Get the flags from this entry.
    #[inline]
    pub const fn flags(&self) -> PageFlags {
        PageFlags::from_bits_truncate(self.0)
    }

    /// Set the entry to point to a physical address with given flags.
    #[inline]
    pub fn set(&mut self, addr: PhysAddr, flags: PageFlags) {
        self.0 = addr.as_u64() | flags.bits();
    }

    /// Set only the flags, preserving the address.
    #[inline]
    pub fn set_flags(&mut self, flags: PageFlags) {
        self.0 = (self.0 & PageFlags::ADDRESS_MASK) | flags.bits();
    }

    /// Add flags to the existing flags.
    #[inline]
    pub fn add_flags(&mut self, flags: PageFlags) {
        self.0 |= flags.bits();
    }

    /// Remove flags from the existing flags.
    #[inline]
    pub fn remove_flags(&mut self, flags: PageFlags) {
        self.0 &= !flags.bits();
    }

    /// Clear the entry (set to zero).
    #[inline]
    pub fn clear(&mut self) {
        self.0 = 0;
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageTableEntry")
            .field("raw", &format_args!("{:#018x}", self.0))
            .field("present", &self.is_present())
            .field("address", &format_args!("{:#x}", self.address().as_u64()))
            .field("flags", &self.flags())
            .finish()
    }
}

/// A 512-entry page table, aligned to 4KB.
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; 512],
}

impl PageTable {
    /// An empty page table with all entries zeroed.
    pub const EMPTY: Self = Self {
        entries: [PageTableEntry::EMPTY; 512],
    };

    /// Create a new empty page table.
    #[inline]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Get a reference to an entry by index.
    #[inline]
    pub fn entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }

    /// Get a mutable reference to an entry by index.
    #[inline]
    pub fn entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }

    /// Check if all entries are unused.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| e.is_unused())
    }

    /// Count the number of present entries.
    pub fn present_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_present()).count()
    }

    /// Zero all entries.
    pub fn zero(&mut self) {
        self.entries.fill(PageTableEntry::EMPTY);
    }

    /// Iterate over all entries.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &PageTableEntry> {
        self.entries.iter()
    }

    /// Iterate over all entries mutably.
    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut PageTableEntry> {
        self.entries.iter_mut()
    }

    /// Iterate over entries with their indices.
    #[inline]
    pub fn iter_enumerated(&self) -> impl Iterator<Item = (usize, &PageTableEntry)> {
        self.entries.iter().enumerate()
    }
}

impl core::ops::Index<usize> for PageTable {
    type Output = PageTableEntry;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl core::ops::IndexMut<usize> for PageTable {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.entries[index]
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::EMPTY
    }
}
```

### 4.3 Error Types (mm layer)

```rust
// mm/src/paging/error.rs

use core::fmt;

/// Errors that can occur during page table operations.
///
/// This follows the Rust OS ecosystem convention (zCore, hvisor, ArceOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PagingError {
    /// Cannot allocate memory for a new page table.
    NoMemory,

    /// The address is not properly aligned for the requested operation.
    NotAligned {
        /// The misaligned address.
        address: u64,
        /// The required alignment.
        required: u64,
    },

    /// The virtual address is not mapped.
    NotMapped {
        /// The unmapped address.
        address: u64,
        /// The level where the walk stopped.
        level: PageTableLevel,
    },

    /// A mapping already exists at this virtual address.
    AlreadyMapped {
        /// The already-mapped address.
        address: u64,
    },

    /// Cannot descend past a huge page mapping.
    ///
    /// The target address is within a huge page (1GB or 2MB),
    /// so individual 4KB pages within it cannot be addressed.
    MappedToHugePage {
        /// The level where the huge page was found.
        level: PageTableLevel,
    },

    /// The page table pointer is invalid (null or unmappable).
    InvalidPageTable,

    /// The physical address for HHDM translation is invalid.
    InvalidPhysicalAddress {
        /// The problematic address.
        address: u64,
    },
}

impl fmt::Display for PagingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory for page table allocation"),
            Self::NotAligned { address, required } => {
                write!(f, "address {:#x} not aligned to {:#x}", address, required)
            }
            Self::NotMapped { address, level } => {
                write!(f, "address {:#x} not mapped (stopped at level {:?})", address, level)
            }
            Self::AlreadyMapped { address } => {
                write!(f, "address {:#x} already mapped", address)
            }
            Self::MappedToHugePage { level } => {
                write!(f, "cannot traverse huge page at level {:?}", level)
            }
            Self::InvalidPageTable => write!(f, "invalid page table pointer"),
            Self::InvalidPhysicalAddress { address } => {
                write!(f, "invalid physical address {:#x}", address)
            }
        }
    }
}

/// Result type for paging operations.
///
/// Default type parameter is `()` for operations that only need to report success/failure.
pub type PagingResult<T = ()> = Result<T, PagingError>;

// Conversion to c_int for legacy API compatibility during migration
impl PagingError {
    /// Convert to a legacy c_int error code.
    ///
    /// This is for compatibility during the migration period.
    /// New code should use `PagingResult` directly.
    #[deprecated(note = "use PagingResult instead")]
    pub fn to_c_int(self) -> core::ffi::c_int {
        -1 // All errors become -1 for legacy compatibility
    }
}

/// Convert a PagingResult to legacy c_int.
#[deprecated(note = "use PagingResult instead")]
pub fn result_to_c_int(result: PagingResult) -> core::ffi::c_int {
    match result {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
```

### 4.4 Walker Infrastructure

```rust
// mm/src/paging/walker.rs

use crate::hhdm::PhysAddrHhdm;
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::arch::x86_64::page_table::{PageTable, PageTableEntry, PageTableLevel};
use slopos_abi::arch::x86_64::paging::{PAGE_SIZE_1GB, PAGE_SIZE_2MB, PAGE_SIZE_4KB};

use super::error::{PagingError, PagingResult};

/// Control what the walker does after visiting each level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkAction {
    /// Continue descending to the next level (default behavior).
    Descend,
    /// Stop the walk here; the current entry is the result.
    Stop,
    /// Skip this subtree entirely (treat as not mapped).
    Skip,
}

impl Default for WalkAction {
    fn default() -> Self {
        Self::Descend
    }
}

/// Result of successfully walking to a virtual address.
#[derive(Debug, Clone, Copy)]
pub struct WalkResult {
    /// The final page table entry found.
    pub entry: PageTableEntry,
    /// The level at which we stopped.
    pub level: PageTableLevel,
    /// The physical address (with page offset applied).
    pub phys_addr: PhysAddr,
    /// The size of the page at this mapping.
    pub page_size: u64,
}

impl WalkResult {
    /// Check if this is a huge page mapping (1GB or 2MB).
    #[inline]
    pub fn is_huge_page(&self) -> bool {
        self.page_size > PAGE_SIZE_4KB
    }
}

/// Trait for translating physical page table frames to virtual pointers.
///
/// This abstracts over the HHDM so the walker doesn't depend on a
/// specific translation mechanism.
///
/// # Safety
///
/// Implementors must ensure that `phys_to_table_ptr` returns a valid
/// pointer to a `PageTable` for any valid physical page table frame.
pub unsafe trait PageTableFrameMapping {
    /// Convert a physical address of a page table to a virtual pointer.
    ///
    /// Returns `None` if the address is null or cannot be mapped.
    fn phys_to_table_ptr(&self, phys: PhysAddr) -> Option<*mut PageTable>;
}

/// Default implementation using HHDM translation.
pub struct HhdmMapping;

unsafe impl PageTableFrameMapping for HhdmMapping {
    #[inline]
    fn phys_to_table_ptr(&self, phys: PhysAddr) -> Option<*mut PageTable> {
        if phys.is_null() {
            return None;
        }
        Some(phys.to_virt().as_mut_ptr())
    }
}

/// The core page table walker.
///
/// This encapsulates all page table traversal logic in one place.
/// All operations that need to walk page tables should use this.
///
/// # Type Parameter
///
/// - `M`: The mapping strategy for converting physical addresses to virtual.
///   Defaults to `HhdmMapping` which uses the Higher Half Direct Map.
///
/// # Example
///
/// ```ignore
/// let walker = PageTableWalker::new();
/// let result = walker.walk(pml4, vaddr)?;
/// println!("Physical address: {:?}", result.phys_addr);
/// ```
pub struct PageTableWalker<M: PageTableFrameMapping = HhdmMapping> {
    mapping: M,
}

impl PageTableWalker<HhdmMapping> {
    /// Create a new walker using HHDM translation.
    #[inline]
    pub fn new() -> Self {
        Self { mapping: HhdmMapping }
    }
}

impl Default for PageTableWalker<HhdmMapping> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: PageTableFrameMapping> PageTableWalker<M> {
    /// Create a walker with a custom mapping strategy.
    #[inline]
    pub fn with_mapping(mapping: M) -> Self {
        Self { mapping }
    }

    /// Get a reference to the next level table from an entry.
    ///
    /// # Errors
    ///
    /// - `NotMapped` if the entry is not present
    /// - `MappedToHugePage` if the entry is a huge page (at L3 or L2)
    /// - `InvalidPageTable` if the physical address cannot be translated
    #[inline]
    pub fn next_table<'a>(
        &self,
        entry: &PageTableEntry,
        level: PageTableLevel,
    ) -> PagingResult<&'a PageTable> {
        if !entry.is_present() {
            return Err(PagingError::NotMapped {
                address: entry.address().as_u64(),
                level,
            });
        }
        if entry.is_huge() && level.supports_huge_pages() {
            return Err(PagingError::MappedToHugePage { level });
        }

        let phys = entry.address();
        let ptr = self
            .mapping
            .phys_to_table_ptr(phys)
            .ok_or(PagingError::InvalidPageTable)?;

        // SAFETY: The mapping implementation guarantees valid pointers
        Ok(unsafe { &*ptr })
    }

    /// Get a mutable reference to the next level table from an entry.
    ///
    /// # Errors
    ///
    /// Same as `next_table`.
    #[inline]
    pub fn next_table_mut<'a>(
        &self,
        entry: &PageTableEntry,
        level: PageTableLevel,
    ) -> PagingResult<&'a mut PageTable> {
        if !entry.is_present() {
            return Err(PagingError::NotMapped {
                address: entry.address().as_u64(),
                level,
            });
        }
        if entry.is_huge() && level.supports_huge_pages() {
            return Err(PagingError::MappedToHugePage { level });
        }

        let phys = entry.address();
        let ptr = self
            .mapping
            .phys_to_table_ptr(phys)
            .ok_or(PagingError::InvalidPageTable)?;

        // SAFETY: The mapping implementation guarantees valid pointers
        Ok(unsafe { &mut *ptr })
    }

    /// Walk from PML4 to find the mapping for a virtual address.
    ///
    /// This traverses the page table hierarchy, handling huge pages automatically.
    /// Returns the final entry and physical address with page offset applied.
    ///
    /// # Errors
    ///
    /// - `NotMapped` if any level is not present
    /// - `InvalidPageTable` if translation fails
    pub fn walk(&self, pml4: &PageTable, vaddr: VirtAddr) -> PagingResult<WalkResult> {
        let mut current_table = pml4;
        let mut level = PageTableLevel::Four;

        loop {
            let index = level.index_of(vaddr);
            let entry = current_table[index];

            if !entry.is_present() {
                return Err(PagingError::NotMapped {
                    address: vaddr.as_u64(),
                    level,
                });
            }

            // Check for huge page at L3 (1GB) or L2 (2MB)
            if entry.is_huge() && level.supports_huge_pages() {
                let page_size = level.page_size().unwrap();
                let offset = vaddr.as_u64() & (page_size - 1);
                return Ok(WalkResult {
                    entry,
                    level,
                    phys_addr: entry.address().offset(offset),
                    page_size,
                });
            }

            // Try to descend to next level
            match level.next_lower() {
                Some(next_level) => {
                    current_table = self.next_table(&entry, level)?;
                    level = next_level;
                }
                None => {
                    // At L1 (PT level), we have a 4KB page
                    let offset = vaddr.as_u64() & (PAGE_SIZE_4KB - 1);
                    return Ok(WalkResult {
                        entry,
                        level,
                        phys_addr: entry.address().offset(offset),
                        page_size: PAGE_SIZE_4KB,
                    });
                }
            }
        }
    }

    /// Walk with a callback at each level.
    ///
    /// This is the most flexible walking method, inspired by Linux's `walk_page_range`.
    /// The callback receives the current level and entry, and returns a `WalkAction`
    /// to control traversal.
    ///
    /// # Callback
    ///
    /// The callback `F` receives:
    /// - `PageTableLevel`: Current level (4 = PML4, 1 = PT)
    /// - `&PageTableEntry`: The entry at this level for the target address
    ///
    /// Return values:
    /// - `WalkAction::Descend`: Continue to next level (default)
    /// - `WalkAction::Stop`: Stop here, return current entry
    /// - `WalkAction::Skip`: Treat as not mapped
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Check if all levels have USER bit set
    /// let mut all_user = true;
    /// walker.walk_with(pml4, vaddr, |level, entry| {
    ///     if !entry.is_user() {
    ///         all_user = false;
    ///     }
    ///     WalkAction::Descend
    /// })?;
    /// ```
    pub fn walk_with<F>(
        &self,
        pml4: &PageTable,
        vaddr: VirtAddr,
        mut callback: F,
    ) -> PagingResult<WalkResult>
    where
        F: FnMut(PageTableLevel, &PageTableEntry) -> WalkAction,
    {
        let mut current_table = pml4;
        let mut level = PageTableLevel::Four;

        loop {
            let index = level.index_of(vaddr);
            let entry = &current_table[index];

            match callback(level, entry) {
                WalkAction::Stop => {
                    let page_size = level.page_size().unwrap_or(PAGE_SIZE_4KB);
                    let offset = vaddr.as_u64() & (page_size - 1);
                    return Ok(WalkResult {
                        entry: *entry,
                        level,
                        phys_addr: entry.address().offset(offset),
                        page_size,
                    });
                }
                WalkAction::Skip => {
                    return Err(PagingError::NotMapped {
                        address: vaddr.as_u64(),
                        level,
                    });
                }
                WalkAction::Descend => {
                    if !entry.is_present() {
                        return Err(PagingError::NotMapped {
                            address: vaddr.as_u64(),
                            level,
                        });
                    }

                    // Handle huge pages
                    if entry.is_huge() && level.supports_huge_pages() {
                        let page_size = level.page_size().unwrap();
                        let offset = vaddr.as_u64() & (page_size - 1);
                        return Ok(WalkResult {
                            entry: *entry,
                            level,
                            phys_addr: entry.address().offset(offset),
                            page_size,
                        });
                    }

                    // Descend to next level
                    match level.next_lower() {
                        Some(next_level) => {
                            current_table = self.next_table(entry, level)?;
                            level = next_level;
                        }
                        None => {
                            let offset = vaddr.as_u64() & (PAGE_SIZE_4KB - 1);
                            return Ok(WalkResult {
                                entry: *entry,
                                level,
                                phys_addr: entry.address().offset(offset),
                                page_size: PAGE_SIZE_4KB,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Walk to find entries at all levels for a virtual address.
    ///
    /// Returns a tuple of optional entries at each level (L4, L3, L2, L1).
    /// Stops at the first non-present entry or huge page.
    ///
    /// This is useful for operations that need to examine or modify
    /// entries at multiple levels.
    pub fn walk_levels(
        &self,
        pml4: &PageTable,
        vaddr: VirtAddr,
    ) -> (
        Option<PageTableEntry>,
        Option<PageTableEntry>,
        Option<PageTableEntry>,
        Option<PageTableEntry>,
    ) {
        let l4_idx = PageTableLevel::Four.index_of(vaddr);
        let l4_entry = pml4[l4_idx];

        if !l4_entry.is_present() {
            return (Some(l4_entry), None, None, None);
        }

        let l3 = match self.next_table(&l4_entry, PageTableLevel::Four) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), None, None, None),
        };

        let l3_idx = PageTableLevel::Three.index_of(vaddr);
        let l3_entry = l3[l3_idx];

        if !l3_entry.is_present() || l3_entry.is_huge() {
            return (Some(l4_entry), Some(l3_entry), None, None);
        }

        let l2 = match self.next_table(&l3_entry, PageTableLevel::Three) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), Some(l3_entry), None, None),
        };

        let l2_idx = PageTableLevel::Two.index_of(vaddr);
        let l2_entry = l2[l2_idx];

        if !l2_entry.is_present() || l2_entry.is_huge() {
            return (Some(l4_entry), Some(l3_entry), Some(l2_entry), None);
        }

        let l1 = match self.next_table(&l2_entry, PageTableLevel::Two) {
            Ok(t) => t,
            Err(_) => return (Some(l4_entry), Some(l3_entry), Some(l2_entry), None),
        };

        let l1_idx = PageTableLevel::One.index_of(vaddr);
        let l1_entry = l1[l1_idx];

        (Some(l4_entry), Some(l3_entry), Some(l2_entry), Some(l1_entry))
    }
}
```

### 4.5 High-Level Operations

```rust
// mm/src/paging/translate.rs

use super::walker::{PageTableWalker, WalkResult};
use super::error::PagingResult;
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::arch::x86_64::page_table::PageTable;

/// Translate a virtual address to physical address.
///
/// Returns `None` if the address is not mapped.
///
/// This replaces the old `virt_to_phys_for_dir` function.
#[inline]
pub fn virt_to_phys(pml4: &PageTable, vaddr: VirtAddr) -> Option<PhysAddr> {
    let walker = PageTableWalker::new();
    walker.walk(pml4, vaddr).ok().map(|r| r.phys_addr)
}

/// Translate a virtual address, returning full walk result.
///
/// This provides more information than `virt_to_phys`, including
/// the page size and level.
#[inline]
pub fn translate(pml4: &PageTable, vaddr: VirtAddr) -> PagingResult<WalkResult> {
    let walker = PageTableWalker::new();
    walker.walk(pml4, vaddr)
}

/// Check if a virtual address is mapped.
#[inline]
pub fn is_mapped(pml4: &PageTable, vaddr: VirtAddr) -> bool {
    virt_to_phys(pml4, vaddr).is_some()
}

/// Get the page size at a virtual address.
///
/// Returns `None` if the address is not mapped.
#[inline]
pub fn get_page_size(pml4: &PageTable, vaddr: VirtAddr) -> Option<u64> {
    let walker = PageTableWalker::new();
    walker.walk(pml4, vaddr).ok().map(|r| r.page_size)
}
```

```rust
// mm/src/paging/permissions.rs

use super::walker::{PageTableWalker, WalkAction};
use super::error::{PagingError, PagingResult};
use slopos_abi::addr::VirtAddr;
use slopos_abi::arch::x86_64::page_table::{PageTable, PageTableLevel};

/// Check if a virtual address is accessible from user mode.
///
/// For an address to be user-accessible, ALL levels of the page table
/// hierarchy must have the USER bit set.
///
/// This replaces the old `paging_is_user_accessible` function.
pub fn is_user_accessible(pml4: &PageTable, vaddr: VirtAddr) -> bool {
    let walker = PageTableWalker::new();
    let mut all_user = true;

    let result = walker.walk_with(pml4, vaddr, |_level, entry| {
        if entry.is_present() && !entry.is_user() {
            all_user = false;
        }
        WalkAction::Descend
    });

    result.is_ok() && all_user
}

/// Check if a virtual address is writable.
///
/// For an address to be writable, the final page entry must have
/// the WRITABLE bit set.
pub fn is_writable(pml4: &PageTable, vaddr: VirtAddr) -> bool {
    let walker = PageTableWalker::new();
    walker
        .walk(pml4, vaddr)
        .map(|r| r.entry.is_writable())
        .unwrap_or(false)
}
```

---

## 5. Migration Plan

### Phase 1: Foundation (Non-Breaking)

**Goal**: Add new types without changing existing code.

**Tasks**:
1. Create `abi/src/arch/x86_64/page_table.rs` with:
   - `PageTableLevel` enum
   - `PageTableEntry` struct
   - `PageTable` struct
2. Create `mm/src/paging/error.rs` with:
   - `PagingError` enum
   - `PagingResult` type alias
3. Create `mm/src/paging/walker.rs` with:
   - `PageTableWalker` struct
   - `WalkAction` enum
   - `WalkResult` struct
4. Add module structure to `mm/src/paging/mod.rs`
5. Ensure all existing code still compiles

**Verification**:
- `make build` succeeds
- `make test` passes
- No changes to existing function signatures

### Phase 2: Parallel Implementation

**Goal**: Implement new versions of core functions alongside old ones.

**Tasks**:
1. Implement `mm/src/paging/translate.rs`:
   - `virt_to_phys()` using walker
   - Add `virt_to_phys_v2()` as temporary export
2. Implement `mm/src/paging/permissions.rs`:
   - `is_user_accessible()` using walker
3. Add comparison tests:
   - Test that `virt_to_phys()` == old `virt_to_phys_for_dir()`
   - Test that `is_user_accessible()` == old `paging_is_user_accessible()`

**Verification**:
- New and old functions produce identical results
- Benchmark shows no performance regression

### Phase 3: Mapper Implementation

**Goal**: Add map/unmap operations using the walker infrastructure.

**Tasks**:
1. Create `mm/src/paging/mapper.rs`:
   - `ensure_table()` - allocate intermediate tables
   - `map_4kb()`, `map_2mb()`, `map_1gb()`
   - `unmap()` with table cleanup
2. Add `map_page_v2()` as temporary parallel implementation
3. Extensive testing of edge cases:
   - Already-mapped addresses
   - Huge page boundaries
   - Memory exhaustion

### Phase 4: Gradual Cutover

**Goal**: Replace old functions with new implementations.

**Tasks**:
1. Update `virt_to_phys_for_dir()` to call new `virt_to_phys()`
2. Update `paging_is_user_accessible()` to call new `is_user_accessible()`
3. Update `map_page_in_directory()` to call new mapper
4. Update `unmap_page_in_directory()` to call new mapper
5. Mark old standalone functions as `#[deprecated]`
6. Update all callers to use new APIs with `PagingResult`

### Phase 5: Cleanup

**Goal**: Remove deprecated code and finalize.

**Tasks**:
1. Remove `mm/src/paging/legacy.rs` (if created)
2. Remove deprecated functions after all callers updated
3. Final documentation pass
4. Performance benchmarking vs original

---

## 6. Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_table_level_index_calculation() {
        let vaddr = VirtAddr::new(0x0000_7F80_0020_1234);
        
        assert_eq!(PageTableLevel::Four.index_of(vaddr), 0xFF);
        assert_eq!(PageTableLevel::Three.index_of(vaddr), 0x00);
        assert_eq!(PageTableLevel::Two.index_of(vaddr), 0x01);
        assert_eq!(PageTableLevel::One.index_of(vaddr), 0x01);
    }

    #[test]
    fn page_table_level_page_sizes() {
        assert_eq!(PageTableLevel::Three.page_size(), Some(PAGE_SIZE_1GB));
        assert_eq!(PageTableLevel::Two.page_size(), Some(PAGE_SIZE_2MB));
        assert_eq!(PageTableLevel::One.page_size(), Some(PAGE_SIZE_4KB));
        assert_eq!(PageTableLevel::Four.page_size(), None);
    }

    #[test]
    fn page_table_entry_flags() {
        let mut entry = PageTableEntry::EMPTY;
        assert!(!entry.is_present());
        
        entry.set(PhysAddr::new(0x1000), PageFlags::PRESENT | PageFlags::WRITABLE);
        assert!(entry.is_present());
        assert!(entry.is_writable());
        assert!(!entry.is_user());
        assert_eq!(entry.address(), PhysAddr::new(0x1000));
    }
}
```

### Integration Tests

The existing boot test (`make test`) will validate that:
- Kernel still boots with new paging code
- Memory operations work correctly
- No regressions in functionality

### Comparison Tests

During migration, add tests that run both old and new implementations
and assert they produce identical results for various inputs.

---

## 7. Success Criteria

### Code Quality
- [ ] Zero duplication in page table traversal logic
- [ ] All operations use `PageTableWalker`
- [ ] `PagingResult` used instead of `c_int` everywhere
- [ ] Full rustdoc documentation

### Correctness
- [ ] All existing tests pass
- [ ] Boot test succeeds
- [ ] Comparison tests show identical behavior

### Performance
- [ ] No measurable regression in `virt_to_phys` performance
- [ ] Page mapping operations same speed or faster

### Maintainability
- [ ] Adding new page table operation requires only callback
- [ ] Single place to fix bugs in traversal logic
- [ ] Clear error types with actionable information

---

## 8. Open Questions

1. **Should `PageTableLevel` be in `abi` or `mm`?**
   - Current plan: `abi` for sharing with debug tools
   - Alternative: `mm` only if no external use case

2. **Should we use the `x86_64` crate directly?**
   - Pro: Battle-tested, feature-rich
   - Con: Dependency, may not match our exact needs
   - Decision: Learn from it, but implement ourselves for control

3. **How to handle the existing `ProcessPageDir` abstraction?**
   - Keep it as-is initially, refactor in separate plan
   - New walker takes `&PageTable`, callers extract PML4 from ProcessPageDir

---

## 9. References

- [Linux mm/pagewalk.c](https://github.com/torvalds/linux/blob/master/mm/pagewalk.c)
- [rust-osdev/x86_64 paging](https://github.com/rust-osdev/x86_64/tree/master/src/structures/paging)
- [ArceOS page_table crate](https://github.com/arceos-org/page_table_multiarch)
- [hvisor paging implementation](https://github.com/syswonder/hvisor)
- [Phil Opp's Blog OS paging](https://os.phil-opp.com/paging-introduction/)
