use core::arch::global_asm;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr;
use slopos_ostd::sync::InitFlag;

use crate::memory_layout_defs::KERNEL_HEAP_VBASE;
use crate::paging::paging_is_user_accessible;
use crate::process_vm::process_vm_get_page_dir;
use crate::user_ptr::{UserBytes, UserPtr, UserPtrError, UserVirtAddr};

static KERNEL_GUARD_CHECKED: InitFlag = InitFlag::new();

// =============================================================================
// Assembly usercopy — Redox-style fault-recoverable byte copy
//
// The `raw_usercopy` function uses `rep movsb` between two labeled symbols.
// If a page fault occurs while RIP is within `__usercopy_start..__usercopy_end`,
// the page fault handler in `boot/src/idt.rs` redirects execution to
// `__usercopy_fault` which returns the remaining byte count (nonzero = error).
//
// This makes copy_from_user / copy_to_user safe against concurrent munmap
// on SMP — the kernel never panics on a user-space address fault.
// =============================================================================

global_asm!(
    // fn raw_usercopy(dst: *mut u8 [rdi], src: *const u8 [rsi], len: usize [rdx]) -> usize
    // Returns 0 on success, >0 (remaining bytes) on fault.
    //
    // SMAP: `stac` opens the user-page access window; `clac` closes it on
    // every exit path (success + fault-recovery). Hardware clears AC on
    // exception entry, so nested kernel code running between the fault and
    // IRETQ sees SMAP enforced normally; IRETQ restores AC from the saved
    // RFLAGS, so we resume inside the stac window.
    ".global raw_usercopy",
    ".global __usercopy_start",
    ".global __usercopy_end",
    ".global __usercopy_fault",
    "raw_usercopy:",
    "   mov rcx, rdx", // len → rcx for rep prefix
    "   stac",
    "__usercopy_start:",
    "   rep movsb", // copy [rsi] → [rdi], rcx bytes
    "__usercopy_end:",
    "   clac",
    "   xor eax, eax", // return 0 = success
    "   ret",
    "__usercopy_fault:",
    "   clac",
    "   mov rax, rcx", // return remaining byte count
    "   ret",
);

unsafe extern "C" {
    /// Byte-copy with fault recovery.  Returns 0 on success, or the
    /// number of remaining (un-copied) bytes if a page fault occurred.
    fn raw_usercopy(dst: *mut u8, src: *const u8, len: usize) -> usize;

    /// Start of the faultable instruction region.
    fn __usercopy_start();
    /// End of the faultable instruction region.
    fn __usercopy_end();
    /// Fault recovery entry point — jumped to by the page fault handler.
    fn __usercopy_fault();
}

/// Returns `true` if `rip` falls within the faultable usercopy region.
///
/// Called by the page fault handler (`boot/src/idt.rs`) for kernel-mode
/// faults.  If this returns `true`, the handler should redirect RIP to
/// `usercopy_fault_ip()` instead of panicking.
#[inline]
pub fn is_usercopy_ip(rip: u64) -> bool {
    let start = __usercopy_start as *const () as u64;
    let end = __usercopy_end as *const () as u64;
    rip >= start && rip < end
}

/// The RIP value the page fault handler should set to recover from a
/// fault during usercopy.
#[inline]
pub fn usercopy_fault_ip() -> u64 {
    __usercopy_fault as *const () as u64
}

// =============================================================================
// Rust wrappers
// =============================================================================

#[inline]
fn current_process_id() -> u32 {
    unsafe { pcr::current_pcr() }
        .syscall_pid
        .load(Ordering::Acquire)
}

pub fn set_test_process_id(pid: u32) {
    unsafe {
        pcr::current_pcr().syscall_pid.store(pid, Ordering::Release);
    }
}

fn current_process_dir() -> *mut crate::paging::ProcessPageDir {
    let pid = current_process_id();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return ptr::null_mut();
    }
    process_vm_get_page_dir(pid)
}

fn validate_user_pages(
    user_addr: UserVirtAddr,
    len: usize,
    dir: *mut crate::paging::ProcessPageDir,
) -> Result<(), UserPtrError> {
    if len == 0 {
        return Ok(());
    }
    if dir.is_null() {
        return Err(UserPtrError::NotMapped);
    }

    if !KERNEL_GUARD_CHECKED.is_set() {
        let kernel_probe = KERNEL_HEAP_VBASE;
        if paging_is_user_accessible(dir, VirtAddr::new(kernel_probe)) != 0 {
            return Err(UserPtrError::NotMapped);
        }
        KERNEL_GUARD_CHECKED.mark_set();
    }

    let start = user_addr.as_u64();

    let end = start
        .checked_add(len as u64)
        .ok_or(UserPtrError::Overflow)?;

    if end > crate::memory_layout_defs::USER_SPACE_END_VA {
        return Err(UserPtrError::OutOfUserRange);
    }

    let page_size = crate::paging_defs::PAGE_SIZE_4KB;
    let mut page = start & !(page_size - 1);

    while page < end {
        if paging_is_user_accessible(dir, VirtAddr(page)) == 0 {
            return Err(UserPtrError::NotMapped);
        }
        page = match page.checked_add(page_size) {
            Some(next) => next,
            None => break,
        };
    }

    Ok(())
}

/// Perform a fault-recoverable copy via the assembly usercopy function.
///
/// Returns `Ok(())` on success, `Err(Fault)` if the copy faulted.
/// The page-validation step is an optimistic fast path — the assembly
/// function handles the actual fault if validation is stale.
#[inline]
unsafe fn do_usercopy(dst: *mut u8, src: *const u8, len: usize) -> Result<(), UserPtrError> {
    let remaining = unsafe { raw_usercopy(dst, src, len) };
    if remaining == 0 {
        Ok(())
    } else {
        Err(UserPtrError::CopyFailed)
    }
}

/// Copy a `T` from user space into kernel space.
///
/// Safe against concurrent munmap: if the pages are unmapped between
/// validation and the copy, the assembly usercopy function faults and
/// the page fault handler returns an error instead of panicking.
pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(src.addr(), core::mem::size_of::<T>(), dir)?;
    unsafe {
        let mut val = core::mem::MaybeUninit::<T>::uninit();
        do_usercopy(
            val.as_mut_ptr() as *mut u8,
            src.as_ptr() as *const u8,
            core::mem::size_of::<T>(),
        )?;
        Ok(val.assume_init())
    }
}

/// Copy a `T` from kernel space into user space.
pub fn copy_to_user<T: Copy>(dst: UserPtr<T>, value: &T) -> Result<(), UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(dst.addr(), core::mem::size_of::<T>(), dir)?;
    unsafe {
        do_usercopy(
            dst.as_mut_ptr() as *mut u8,
            value as *const T as *const u8,
            core::mem::size_of::<T>(),
        )
    }
}

/// Copy raw bytes from user space.
pub fn copy_bytes_from_user(src: UserBytes, dst: &mut [u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }

    let dir = current_process_dir();
    validate_user_pages(src.base(), copy_len, dir)?;

    unsafe {
        do_usercopy(dst.as_mut_ptr(), src.base().as_ptr(), copy_len)?;
    }
    Ok(copy_len)
}

/// Copy raw bytes to user space.
pub fn copy_bytes_to_user(dst: UserBytes, src: &[u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }

    let dir = current_process_dir();
    validate_user_pages(dst.base(), copy_len, dir)?;

    unsafe {
        do_usercopy(dst.base().as_mut_ptr(), src.as_ptr(), copy_len)?;
    }
    Ok(copy_len)
}
