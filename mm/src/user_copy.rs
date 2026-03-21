use core::ptr;
use core::sync::atomic::Ordering;

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr;
use slopos_sync::{InitFlag, IrqMutex};

use crate::memory_layout_defs::KERNEL_HEAP_VBASE;
use crate::paging::paging_is_user_accessible;
use crate::process_vm::process_vm_get_page_dir;
use crate::user_ptr::{UserBytes, UserPtr, UserPtrError, UserVirtAddr};

static KERNEL_GUARD_CHECKED: InitFlag = InitFlag::new();
static CURRENT_TASK_PROVIDER: IrqMutex<Option<fn() -> u32>> = IrqMutex::new(None);

/// Reads the syscall PID from per-CPU storage (SMP-safe).
fn syscall_process_id_provider() -> u32 {
    // SAFETY: Accessing atomic field on current CPU's PCR.
    unsafe { pcr::current_pcr() }
        .syscall_pid
        .load(Ordering::Acquire)
}

pub fn register_current_task_provider(provider: fn() -> u32) {
    *CURRENT_TASK_PROVIDER.lock() = Some(provider);
}

/// RAII guard that restores the previous task provider on drop.
/// Ensures the provider cannot leak across syscall boundaries, even if
/// the handler panics or takes an early-return path.
pub struct SyscallProviderGuard {
    original: Option<fn() -> u32>,
}

impl Drop for SyscallProviderGuard {
    fn drop(&mut self) {
        let mut guard = CURRENT_TASK_PROVIDER.lock();
        *guard = self.original;
    }
}

/// Sets the syscall PID in per-CPU storage (SMP-safe).
/// Returns an RAII guard that restores the previous provider on drop.
pub fn set_syscall_process_id(pid: u32) -> SyscallProviderGuard {
    // SAFETY: Accessing atomic field on current CPU's PCR.
    unsafe { pcr::current_pcr() }
        .syscall_pid
        .store(pid, Ordering::Release);
    let mut guard = CURRENT_TASK_PROVIDER.lock();
    let original = *guard;
    *guard = Some(syscall_process_id_provider);
    SyscallProviderGuard { original }
}

/// Restore the task provider explicitly. Prefer using the RAII guard from
/// `set_syscall_process_id` instead; this exists for backward compatibility.
pub fn restore_task_provider(provider: Option<fn() -> u32>) {
    let mut guard = CURRENT_TASK_PROVIDER.lock();
    *guard = provider;
}

fn current_process_id() -> u32 {
    let guard = CURRENT_TASK_PROVIDER.lock();
    if let Some(cb) = *guard {
        cb()
    } else {
        slopos_abi::task::INVALID_PROCESS_ID
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

    // Overflow-checked end computation: reject ranges that wrap around u64.
    let end = start
        .checked_add(len as u64)
        .ok_or(UserPtrError::Overflow)?;

    // Reject ranges that extend into or past the kernel/user boundary.
    if end > crate::memory_layout_defs::USER_SPACE_END_VA {
        return Err(UserPtrError::OutOfUserRange);
    }

    let page_size = crate::paging_defs::PAGE_SIZE_4KB;
    let mut page = start & !(page_size - 1);

    while page < end {
        if paging_is_user_accessible(dir, VirtAddr(page)) == 0 {
            return Err(UserPtrError::NotMapped);
        }
        // Saturating add prevents infinite loop on page near u64::MAX.
        page = match page.checked_add(page_size) {
            Some(next) => next,
            None => break,
        };
    }

    Ok(())
}

pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(src.addr(), core::mem::size_of::<T>(), dir)?;
    // SAFETY: Byte-wise copy avoids alignment requirements on the user pointer.
    // This mirrors Linux's copy_from_user which treats userspace as unaligned.
    unsafe {
        let mut val = core::mem::MaybeUninit::<T>::uninit();
        ptr::copy_nonoverlapping(
            src.as_ptr() as *const u8,
            val.as_mut_ptr() as *mut u8,
            core::mem::size_of::<T>(),
        );
        Ok(val.assume_init())
    }
}

pub fn copy_to_user<T: Copy>(dst: UserPtr<T>, value: &T) -> Result<(), UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(dst.addr(), core::mem::size_of::<T>(), dir)?;
    // SAFETY: Byte-wise copy avoids alignment requirements on the user pointer.
    // This mirrors Linux's copy_to_user which treats userspace as unaligned.
    unsafe {
        ptr::copy_nonoverlapping(
            value as *const T as *const u8,
            dst.as_mut_ptr() as *mut u8,
            core::mem::size_of::<T>(),
        );
    }
    Ok(())
}

pub fn copy_bytes_from_user(src: UserBytes, dst: &mut [u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }

    let dir = current_process_dir();
    validate_user_pages(src.base(), copy_len, dir)?;

    unsafe {
        ptr::copy_nonoverlapping(src.base().as_ptr(), dst.as_mut_ptr(), copy_len);
    }
    Ok(copy_len)
}

pub fn copy_bytes_to_user(dst: UserBytes, src: &[u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }

    let dir = current_process_dir();
    validate_user_pages(dst.base(), copy_len, dir)?;

    unsafe {
        ptr::copy_nonoverlapping(src.as_ptr(), dst.base().as_mut_ptr(), copy_len);
    }
    Ok(copy_len)
}
