use core::fmt;
use slopos_abi::arch::x86_64::PageTableLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PagingError {
    NoMemory,
    NotAligned { address: u64, required: u64 },
    NotMapped { address: u64, level: PageTableLevel },
    AlreadyMapped { address: u64 },
    MappedToHugePage { level: PageTableLevel },
    InvalidPageTable,
    InvalidPhysicalAddress { address: u64 },
}

impl fmt::Display for PagingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory for page table allocation"),
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
            Self::InvalidPageTable => write!(f, "invalid page table pointer"),
            Self::InvalidPhysicalAddress { address } => {
                write!(f, "invalid physical address {:#x}", address)
            }
        }
    }
}

pub type PagingResult<T = ()> = Result<T, PagingError>;

impl PagingError {
    #[deprecated(note = "use PagingResult instead")]
    pub fn to_c_int(self) -> core::ffi::c_int {
        -1
    }
}

#[deprecated(note = "use PagingResult instead")]
pub fn result_to_c_int(result: PagingResult) -> core::ffi::c_int {
    match result {
        Ok(()) => 0,
        Err(_) => -1,
    }
}
