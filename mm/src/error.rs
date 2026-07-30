//! Unified error types for the memory management subsystem.
//!
//! This module consolidates error types that were previously scattered across
//! `paging/error.rs`, `cow.rs`, and `demand.rs`. Those modules had significant
//! variant overlap (AllocationFailed, MappingFailed, InvalidAddress, NullPageDir
//! appeared in 2-3 of them).
//!
//! Domain-specific error types (`ElfError`, `UserPtrError`) remain in their own
//! modules — they have no overlapping variants and are well-contained.

use crate::paging::page_table_defs::PageTableLevel;
use core::fmt;

/// Unified memory management error.
///
/// Covers paging, copy-on-write, demand paging, and general VM operations.
/// Variants are organized by the subsystem that typically produces them,
/// but any MM operation may return any variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmError {
    NoMemory,
    MappingFailed,
    InvalidAddress,
    NullPageDir,
    NotAligned { address: u64, required: u64 },
    NotMapped { address: u64, level: PageTableLevel },
    AlreadyMapped { address: u64 },
    MappedToHugePage { level: PageTableLevel },
    InvalidPageTable,
    InvalidPhysicalAddress { address: u64 },
    NotCowPage,
    NoVma,
    NotDemandPaged,
    PermissionDenied,
}

impl fmt::Display for MmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory for page allocation"),
            Self::MappingFailed => write!(f, "page mapping operation failed"),
            Self::InvalidAddress => write!(f, "invalid address"),
            Self::NullPageDir => write!(f, "null page directory"),
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
        }
    }
}

/// Convenience result type for memory management operations.
pub type MmResult<T = ()> = Result<T, MmError>;
