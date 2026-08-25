//! Unified error type for the memory management subsystem. `ElfError` and
//! `UserPtrError` stay in their own modules; they share no variants with this
//! one.

use crate::paging::page_table_defs::PageTableLevel;
use core::fmt;

/// Covers paging, copy-on-write, demand paging and general VM operations;
/// variants are grouped by producing subsystem, but any MM operation may
/// return any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmError {
    NoMemory,
    MappingFailed,
    InvalidAddress,
    NoAddressSpace,
    NotAligned {
        address: u64,
        required: u64,
    },
    NotMapped {
        address: u64,
        level: PageTableLevel,
    },
    AlreadyMapped {
        address: u64,
    },
    MappedToHugePage {
        level: PageTableLevel,
    },
    InvalidPageTable,
    InvalidPhysicalAddress {
        address: u64,
    },
    NotCowPage,
    NoVma,
    NotDemandPaged,
    PermissionDenied,
    /// Exclusive access to the address space was unavailable; transient.
    Retry,
}

impl fmt::Display for MmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory for page allocation"),
            Self::MappingFailed => write!(f, "page mapping operation failed"),
            Self::InvalidAddress => write!(f, "invalid address"),
            Self::NoAddressSpace => write!(f, "process has no address space"),
            Self::NotAligned { address, required } => {
                write!(f, "address {:#x} not aligned to {:#x}", address, required)
            }
            Self::NotMapped { address, level } => {
                write!(
                    f,
                    "address {:#x} not mapped (stopped at level {})",
                    address, level
                )
            }
            Self::AlreadyMapped { address } => {
                write!(f, "address {:#x} already mapped", address)
            }
            Self::MappedToHugePage { level } => {
                write!(f, "cannot traverse huge page at level {}", level)
            }
            Self::InvalidPageTable => write!(f, "null page table frame address"),
            Self::InvalidPhysicalAddress { address } => {
                write!(f, "invalid physical address {:#x}", address)
            }
            Self::NotCowPage => write!(f, "page is not copy-on-write"),
            Self::NoVma => write!(f, "no VMA covers the faulting address"),
            Self::NotDemandPaged => write!(f, "page is not demand-paged"),
            Self::PermissionDenied => write!(f, "VMA permissions deny this access"),
            Self::Retry => write!(f, "address space temporarily not exclusive"),
        }
    }
}

pub type MmResult<T = ()> = Result<T, MmError>;
