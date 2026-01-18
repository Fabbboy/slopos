//! Secure ELF loader with comprehensive validation.
//!
//! This module provides type-safe ELF parsing and validation for loading
//! user-space binaries. It implements defense-in-depth against malicious
//! ELF files with:
//!
//! - Bounds checking on all header fields
//! - Integer overflow prevention using checked arithmetic
//! - Segment overlap detection
//! - Address space validation (preventing kernel address loading)
//! - Alignment requirements enforcement
//!
//! # Security Model
//!
//! The ELF loader assumes the input is untrusted. All fields are validated
//! before use, and the loader fails safely on any validation error.

use core::fmt;

use crate::mm_constants::{PAGE_SIZE_4KB, USER_SPACE_END_VA, USER_SPACE_START_VA};

// =============================================================================
// ELF Constants
// =============================================================================

/// ELF magic bytes: 0x7f 'E' 'L' 'F'
pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// ELF class: 64-bit
pub const ELFCLASS64: u8 = 2;

/// ELF data encoding: Little-endian
pub const ELFDATA2LSB: u8 = 1;

/// ELF version: Current
pub const EV_CURRENT: u8 = 1;

/// ELF OS/ABI: System V
pub const ELFOSABI_NONE: u8 = 0;

/// ELF type: Executable
pub const ET_EXEC: u16 = 2;

/// ELF type: Shared object (position-independent executable)
pub const ET_DYN: u16 = 3;

/// ELF machine: x86-64
pub const EM_X86_64: u16 = 0x3E;

/// Program header type: Loadable segment
pub const PT_LOAD: u32 = 1;

/// Program header type: Dynamic linking info
pub const PT_DYNAMIC: u32 = 2;

/// Program header type: Interpreter path
pub const PT_INTERP: u32 = 3;

/// Program header type: Note section
pub const PT_NOTE: u32 = 4;

/// Program header type: Program header table
pub const PT_PHDR: u32 = 6;

/// Program header type: Thread-local storage
pub const PT_TLS: u32 = 7;

/// Program header type: GNU stack
pub const PT_GNU_STACK: u32 = 0x6474_e551;

/// Program header type: GNU relro
pub const PT_GNU_RELRO: u32 = 0x6474_e552;

/// Segment flag: Executable
pub const PF_X: u32 = 0x1;

/// Segment flag: Writable
pub const PF_W: u32 = 0x2;

/// Segment flag: Readable
pub const PF_R: u32 = 0x4;

/// Maximum number of program headers we'll process (DoS protection)
pub const MAX_PROGRAM_HEADERS: usize = 128;

/// Maximum number of PT_LOAD segments we'll process
pub const MAX_LOAD_SEGMENTS: usize = 16;

/// Maximum total mapped size (256 MB - DoS protection)
pub const MAX_TOTAL_MAPPED_SIZE: u64 = 256 * 1024 * 1024;

/// Minimum ELF header size
pub const MIN_ELF_SIZE: usize = 64;

// =============================================================================
// Error Types
// =============================================================================

/// Errors that can occur during ELF validation and loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Input buffer is too small to contain an ELF header
    BufferTooSmall,
    /// Invalid ELF magic bytes
    InvalidMagic,
    /// Not a 64-bit ELF file
    Not64Bit,
    /// Not little-endian
    NotLittleEndian,
    /// Invalid ELF version
    InvalidVersion,
    /// Not an executable or shared object
    NotExecutable,
    /// Not for x86-64 architecture
    WrongArchitecture,
    /// Program header offset is invalid
    InvalidPhdrOffset,
    /// Program header size is invalid
    InvalidPhdrSize,
    /// Too many program headers
    TooManyProgramHeaders,
    /// Program header table extends beyond file
    PhdrTableOverflow,
    /// Segment offset is invalid (extends beyond file)
    InvalidSegmentOffset,
    /// Segment file size larger than memory size
    FileSizeExceedsMemSize,
    /// Segment size overflow (vaddr + memsz wraps)
    SegmentSizeOverflow,
    /// Segment alignment is invalid (not power of two or zero)
    InvalidAlignment,
    /// Segment maps to kernel address space
    KernelAddressViolation,
    /// Segment maps outside user address space
    AddressOutOfBounds,
    /// Two segments overlap in virtual address space
    SegmentOverlap,
    /// Total mapped size exceeds limit
    TotalSizeExceeded,
    /// Entry point is outside any loaded segment
    EntryPointInvalid,
    /// Too many PT_LOAD segments
    TooManyLoadSegments,
    /// No PT_LOAD segments found
    NoLoadSegments,
    /// Null pointer passed
    NullPointer,
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooSmall => write!(f, "buffer too small for ELF header"),
            Self::InvalidMagic => write!(f, "invalid ELF magic bytes"),
            Self::Not64Bit => write!(f, "not a 64-bit ELF"),
            Self::NotLittleEndian => write!(f, "not little-endian"),
            Self::InvalidVersion => write!(f, "invalid ELF version"),
            Self::NotExecutable => write!(f, "not an executable or shared object"),
            Self::WrongArchitecture => write!(f, "not x86-64 architecture"),
            Self::InvalidPhdrOffset => write!(f, "invalid program header offset"),
            Self::InvalidPhdrSize => write!(f, "invalid program header size"),
            Self::TooManyProgramHeaders => write!(f, "too many program headers"),
            Self::PhdrTableOverflow => write!(f, "program header table overflow"),
            Self::InvalidSegmentOffset => write!(f, "segment offset overflow"),
            Self::FileSizeExceedsMemSize => write!(f, "segment file size > memory size"),
            Self::SegmentSizeOverflow => write!(f, "segment size overflow"),
            Self::InvalidAlignment => write!(f, "invalid segment alignment"),
            Self::KernelAddressViolation => write!(f, "segment maps to kernel space"),
            Self::AddressOutOfBounds => write!(f, "segment outside user address space"),
            Self::SegmentOverlap => write!(f, "overlapping segments"),
            Self::TotalSizeExceeded => write!(f, "total mapped size exceeded"),
            Self::EntryPointInvalid => write!(f, "entry point outside loaded segments"),
            Self::TooManyLoadSegments => write!(f, "too many PT_LOAD segments"),
            Self::NoLoadSegments => write!(f, "no PT_LOAD segments found"),
            Self::NullPointer => write!(f, "null pointer"),
        }
    }
}

/// Result type for ELF operations
pub type ElfResult<T> = Result<T, ElfError>;

// =============================================================================
// ELF Header Structures (validated wrappers)
// =============================================================================

/// Validated 64-bit ELF header.
///
/// This struct is created only after all header fields have been validated.
/// It provides safe accessors to header data.
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    /// ELF type (ET_EXEC or ET_DYN)
    pub e_type: u16,
    /// Target architecture (should be EM_X86_64)
    pub e_machine: u16,
    /// ELF version
    pub e_version: u32,
    /// Entry point virtual address
    pub e_entry: u64,
    /// Program header table offset
    pub e_phoff: u64,
    /// Section header table offset
    pub e_shoff: u64,
    /// Processor-specific flags
    pub e_flags: u32,
    /// ELF header size
    pub e_ehsize: u16,
    /// Program header entry size
    pub e_phentsize: u16,
    /// Number of program headers
    pub e_phnum: u16,
    /// Section header entry size
    pub e_shentsize: u16,
    /// Number of section headers
    pub e_shnum: u16,
    /// Section name string table index
    pub e_shstrndx: u16,
}

impl Elf64Header {
    /// Expected program header entry size
    pub const PHDR_SIZE: u16 = 56;

    /// Parse and validate an ELF header from raw bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure `data` points to valid memory of at least
    /// `MIN_ELF_SIZE` bytes.
    pub fn parse(data: &[u8]) -> ElfResult<Self> {
        if data.len() < MIN_ELF_SIZE {
            return Err(ElfError::BufferTooSmall);
        }

        // Validate magic bytes
        if data[0..4] != ELF_MAGIC {
            return Err(ElfError::InvalidMagic);
        }

        // Validate ELF class (64-bit)
        if data[4] != ELFCLASS64 {
            return Err(ElfError::Not64Bit);
        }

        // Validate data encoding (little-endian)
        if data[5] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian);
        }

        // Validate ELF version
        if data[6] != EV_CURRENT {
            return Err(ElfError::InvalidVersion);
        }

        // Parse header fields (little-endian)
        let e_type = u16::from_le_bytes([data[16], data[17]]);
        let e_machine = u16::from_le_bytes([data[18], data[19]]);
        let e_version = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let e_entry = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let e_phoff = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let e_shoff = u64::from_le_bytes([
            data[40], data[41], data[42], data[43], data[44], data[45], data[46], data[47],
        ]);
        let e_flags = u32::from_le_bytes([data[48], data[49], data[50], data[51]]);
        let e_ehsize = u16::from_le_bytes([data[52], data[53]]);
        let e_phentsize = u16::from_le_bytes([data[54], data[55]]);
        let e_phnum = u16::from_le_bytes([data[56], data[57]]);
        let e_shentsize = u16::from_le_bytes([data[58], data[59]]);
        let e_shnum = u16::from_le_bytes([data[60], data[61]]);
        let e_shstrndx = u16::from_le_bytes([data[62], data[63]]);

        // Validate ELF type (must be executable or shared object)
        if e_type != ET_EXEC && e_type != ET_DYN {
            return Err(ElfError::NotExecutable);
        }

        // Validate architecture (must be x86-64)
        if e_machine != EM_X86_64 {
            return Err(ElfError::WrongArchitecture);
        }

        // Validate program header offset
        if e_phoff == 0 {
            return Err(ElfError::InvalidPhdrOffset);
        }

        // Validate program header entry size
        if e_phentsize < Self::PHDR_SIZE {
            return Err(ElfError::InvalidPhdrSize);
        }

        // Validate number of program headers
        if e_phnum == 0 {
            return Err(ElfError::NoLoadSegments);
        }
        if e_phnum as usize > MAX_PROGRAM_HEADERS {
            return Err(ElfError::TooManyProgramHeaders);
        }

        Ok(Self {
            e_type,
            e_machine,
            e_version,
            e_entry,
            e_phoff,
            e_shoff,
            e_flags,
            e_ehsize,
            e_phentsize,
            e_phnum,
            e_shentsize,
            e_shnum,
            e_shstrndx,
        })
    }

    /// Calculate the total size of the program header table.
    pub fn phdr_table_size(&self) -> usize {
        self.e_phnum as usize * self.e_phentsize as usize
    }

    /// Check if the program header table fits within the file.
    pub fn validate_phdr_table(&self, file_size: usize) -> ElfResult<()> {
        let phdr_end = self
            .e_phoff
            .checked_add(self.phdr_table_size() as u64)
            .ok_or(ElfError::PhdrTableOverflow)?;

        if phdr_end > file_size as u64 {
            return Err(ElfError::PhdrTableOverflow);
        }

        Ok(())
    }

    /// Returns true if this is a position-independent executable.
    pub fn is_pie(&self) -> bool {
        self.e_type == ET_DYN
    }
}

/// Validated 64-bit program header.
#[derive(Debug, Clone, Copy)]
pub struct Elf64Phdr {
    /// Segment type
    pub p_type: u32,
    /// Segment flags (PF_R, PF_W, PF_X)
    pub p_flags: u32,
    /// Offset in file
    pub p_offset: u64,
    /// Virtual address in memory
    pub p_vaddr: u64,
    /// Physical address (unused in user space)
    pub p_paddr: u64,
    /// Size in file
    pub p_filesz: u64,
    /// Size in memory
    pub p_memsz: u64,
    /// Alignment (must be power of 2)
    pub p_align: u64,
}

impl Elf64Phdr {
    /// Parse a program header from raw bytes.
    pub fn parse(data: &[u8]) -> ElfResult<Self> {
        if data.len() < 56 {
            return Err(ElfError::BufferTooSmall);
        }

        let p_type = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let p_flags = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let p_offset = u64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let p_vaddr = u64::from_le_bytes([
            data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
        ]);
        let p_paddr = u64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);
        let p_filesz = u64::from_le_bytes([
            data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
        ]);
        let p_memsz = u64::from_le_bytes([
            data[40], data[41], data[42], data[43], data[44], data[45], data[46], data[47],
        ]);
        let p_align = u64::from_le_bytes([
            data[48], data[49], data[50], data[51], data[52], data[53], data[54], data[55],
        ]);

        Ok(Self {
            p_type,
            p_flags,
            p_offset,
            p_vaddr,
            p_paddr,
            p_filesz,
            p_memsz,
            p_align,
        })
    }

    /// Check if this is a loadable segment.
    pub fn is_load(&self) -> bool {
        self.p_type == PT_LOAD
    }

    /// Check if segment is readable.
    pub fn is_readable(&self) -> bool {
        (self.p_flags & PF_R) != 0
    }

    /// Check if segment is writable.
    pub fn is_writable(&self) -> bool {
        (self.p_flags & PF_W) != 0
    }

    /// Check if segment is executable.
    pub fn is_executable(&self) -> bool {
        (self.p_flags & PF_X) != 0
    }

    /// Calculate the end address (vaddr + memsz) with overflow checking.
    pub fn end_address(&self) -> ElfResult<u64> {
        self.p_vaddr
            .checked_add(self.p_memsz)
            .ok_or(ElfError::SegmentSizeOverflow)
    }

    /// Calculate the file end offset (offset + filesz) with overflow checking.
    pub fn file_end(&self) -> ElfResult<u64> {
        self.p_offset
            .checked_add(self.p_filesz)
            .ok_or(ElfError::InvalidSegmentOffset)
    }

    /// Get the page-aligned start address.
    pub fn aligned_start(&self) -> u64 {
        self.p_vaddr & !(PAGE_SIZE_4KB - 1)
    }

    /// Get the page-aligned end address.
    pub fn aligned_end(&self) -> ElfResult<u64> {
        let end = self.end_address()?;
        Ok((end + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1))
    }
}

// =============================================================================
// Validated Load Segment
// =============================================================================

/// A fully validated PT_LOAD segment ready for mapping.
///
/// This struct is created only after comprehensive validation, including:
/// - Bounds checking within file
/// - Address space validation
/// - Alignment validation
/// - Size validation
#[derive(Debug, Clone, Copy)]
pub struct ValidatedSegment {
    /// Page-aligned virtual address start
    pub vaddr_start: u64,
    /// Page-aligned virtual address end
    pub vaddr_end: u64,
    /// File offset for segment data
    pub file_offset: u64,
    /// Size of data in file
    pub file_size: u64,
    /// Original virtual address (before alignment)
    pub original_vaddr: u64,
    /// Original memory size
    pub mem_size: u64,
    /// Segment flags
    pub flags: u32,
}

impl ValidatedSegment {
    /// Calculate the total number of pages this segment requires.
    pub fn page_count(&self) -> u64 {
        (self.vaddr_end - self.vaddr_start) / PAGE_SIZE_4KB
    }

    /// Check if this segment has a true overlap with another.
    ///
    /// Adjacent/contiguous segments are allowed (common ELF optimization).
    /// Only truly overlapping segments (where one contains addresses inside another)
    /// are considered invalid.
    pub fn has_conflicting_overlap(&self, other: &ValidatedSegment) -> bool {
        let self_start = self.original_vaddr;
        let self_end = self.original_vaddr.saturating_add(self.mem_size);
        let other_start = other.original_vaddr;
        let other_end = other.original_vaddr.saturating_add(other.mem_size);

        // True overlap: one segment's interior intersects another's interior
        // Adjacent segments (self_end == other_start) are NOT overlaps
        self_start < other_end
            && self_end > other_start
            && self_start != other_end
            && self_end != other_start
    }
}

// =============================================================================
// ELF Validator
// =============================================================================

/// Comprehensive ELF validator and parser.
///
/// This validator performs all security checks on an ELF file before
/// returning validated structures that are safe to use for loading.
pub struct ElfValidator<'a> {
    /// Raw ELF file data
    data: &'a [u8],
    /// Validated ELF header
    header: Elf64Header,
    /// Base address for loading (for PIE/relocation)
    load_base: u64,
}

impl<'a> ElfValidator<'a> {
    /// Create a new ELF validator for the given data.
    ///
    /// This immediately validates the ELF header.
    pub fn new(data: &'a [u8]) -> ElfResult<Self> {
        let header = Elf64Header::parse(data)?;
        header.validate_phdr_table(data.len())?;

        Ok(Self {
            data,
            header,
            load_base: 0,
        })
    }

    /// Set the load base address for PIE binaries.
    pub fn with_load_base(mut self, base: u64) -> Self {
        self.load_base = base;
        self
    }

    /// Get the validated ELF header.
    pub fn header(&self) -> &Elf64Header {
        &self.header
    }

    /// Parse and validate all PT_LOAD segments.
    ///
    /// Returns a vector of validated segments ready for loading.
    /// Performs overlap detection between all segments.
    pub fn validate_load_segments(
        &self,
    ) -> ElfResult<([ValidatedSegment; MAX_LOAD_SEGMENTS], usize)> {
        let mut segments = [ValidatedSegment {
            vaddr_start: 0,
            vaddr_end: 0,
            file_offset: 0,
            file_size: 0,
            original_vaddr: 0,
            mem_size: 0,
            flags: 0,
        }; MAX_LOAD_SEGMENTS];
        let mut count = 0;
        let mut total_size: u64 = 0;

        // First pass: validate each segment individually
        for i in 0..self.header.e_phnum as usize {
            let phdr = self.get_program_header(i)?;

            if !phdr.is_load() {
                continue;
            }

            if count >= MAX_LOAD_SEGMENTS {
                return Err(ElfError::TooManyLoadSegments);
            }

            let validated = self.validate_segment(&phdr)?;

            // Track total size
            let segment_size = validated.vaddr_end - validated.vaddr_start;
            total_size = total_size
                .checked_add(segment_size)
                .ok_or(ElfError::TotalSizeExceeded)?;

            if total_size > MAX_TOTAL_MAPPED_SIZE {
                return Err(ElfError::TotalSizeExceeded);
            }

            segments[count] = validated;
            count += 1;
        }

        if count == 0 {
            return Err(ElfError::NoLoadSegments);
        }

        for i in 0..count {
            for j in (i + 1)..count {
                if segments[i].has_conflicting_overlap(&segments[j]) {
                    return Err(ElfError::SegmentOverlap);
                }
            }
        }

        Ok((segments, count))
    }

    /// Validate the entry point address.
    ///
    /// The entry point must fall within one of the loaded segments.
    pub fn validate_entry_point(&self, segments: &[ValidatedSegment]) -> ElfResult<u64> {
        let entry = self.adjusted_entry_point();

        // Entry point must be within a loaded segment
        let valid = segments
            .iter()
            .any(|seg| entry >= seg.vaddr_start && entry < seg.vaddr_end);

        if !valid {
            return Err(ElfError::EntryPointInvalid);
        }

        Ok(entry)
    }

    /// Get the entry point adjusted for load base.
    pub fn adjusted_entry_point(&self) -> u64 {
        if self.header.is_pie() {
            self.load_base.wrapping_add(self.header.e_entry)
        } else {
            self.header.e_entry
        }
    }

    /// Get a program header by index.
    fn get_program_header(&self, index: usize) -> ElfResult<Elf64Phdr> {
        if index >= self.header.e_phnum as usize {
            return Err(ElfError::InvalidPhdrOffset);
        }

        let offset = self.header.e_phoff as usize + index * self.header.e_phentsize as usize;
        let end = offset + 56;

        if end > self.data.len() {
            return Err(ElfError::PhdrTableOverflow);
        }

        Elf64Phdr::parse(&self.data[offset..end])
    }

    /// Validate a single segment comprehensively.
    fn validate_segment(&self, phdr: &Elf64Phdr) -> ElfResult<ValidatedSegment> {
        // 1. Validate file bounds: p_offset + p_filesz must fit in file
        let file_end = phdr.file_end()?;
        if file_end > self.data.len() as u64 {
            return Err(ElfError::InvalidSegmentOffset);
        }

        // 2. Validate sizes: p_filesz <= p_memsz
        if phdr.p_filesz > phdr.p_memsz {
            return Err(ElfError::FileSizeExceedsMemSize);
        }

        // 3. Validate memory bounds: vaddr + memsz must not overflow
        let vaddr_end = phdr.end_address()?;

        // 4. Validate alignment (if non-zero, must be power of 2)
        if phdr.p_align != 0 && !phdr.p_align.is_power_of_two() {
            return Err(ElfError::InvalidAlignment);
        }

        // 5. Calculate actual addresses (apply load base for PIE)
        let vaddr = if self.header.is_pie() {
            self.load_base.wrapping_add(phdr.p_vaddr)
        } else {
            phdr.p_vaddr
        };

        let mem_end = if self.header.is_pie() {
            self.load_base.wrapping_add(vaddr_end)
        } else {
            vaddr_end
        };

        // 6. Validate address space: must be in user space
        // Check for kernel address space (high canonical addresses)
        const KERNEL_BASE: u64 = 0xFFFF_8000_0000_0000;
        if vaddr >= KERNEL_BASE || mem_end > KERNEL_BASE {
            return Err(ElfError::KernelAddressViolation);
        }

        // Check user space bounds
        if vaddr < USER_SPACE_START_VA || mem_end > USER_SPACE_END_VA {
            return Err(ElfError::AddressOutOfBounds);
        }

        // 7. Calculate page-aligned boundaries
        let aligned_start = vaddr & !(PAGE_SIZE_4KB - 1);
        let aligned_end = (mem_end + PAGE_SIZE_4KB - 1) & !(PAGE_SIZE_4KB - 1);

        // Ensure alignment didn't cause overflow
        if aligned_end < aligned_start {
            return Err(ElfError::SegmentSizeOverflow);
        }

        Ok(ValidatedSegment {
            vaddr_start: aligned_start,
            vaddr_end: aligned_end,
            file_offset: phdr.p_offset,
            file_size: phdr.p_filesz,
            original_vaddr: vaddr,
            mem_size: phdr.p_memsz,
            flags: phdr.p_flags,
        })
    }

    /// Get raw access to the file data for a segment.
    ///
    /// Returns the slice of file data corresponding to the segment's file content.
    /// This has already been bounds-checked during validation.
    pub fn segment_data(&self, segment: &ValidatedSegment) -> &[u8] {
        let start = segment.file_offset as usize;
        let end = start + segment.file_size as usize;
        &self.data[start..end]
    }
}
