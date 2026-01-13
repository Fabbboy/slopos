use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use slopos_lib::IrqMutex;

use slopos_abi::addr::VirtAddr;

use crate::memory_layout::mm_get_kernel_heap_start;
use crate::paging::paging_is_user_accessible;
use crate::process_vm::process_vm_get_page_dir;
use crate::user_ptr::{UserBytes, UserPtr, UserPtrError, UserVirtAddr};

static KERNEL_GUARD_CHECKED: AtomicBool = AtomicBool::new(false);
static CURRENT_TASK_PROVIDER: IrqMutex<Option<fn() -> u32>> = IrqMutex::new(None);

static mut SYSCALL_CURRENT_PID: u32 = crate::mm_constants::INVALID_PROCESS_ID;

fn syscall_process_id_provider() -> u32 {
    unsafe { SYSCALL_CURRENT_PID }
}

pub fn register_current_task_provider(provider: fn() -> u32) {
    *CURRENT_TASK_PROVIDER.lock() = Some(provider);
}

pub fn set_syscall_process_id(pid: u32) -> Option<fn() -> u32> {
    unsafe {
        SYSCALL_CURRENT_PID = pid;
    }
    let mut guard = CURRENT_TASK_PROVIDER.lock();
    let original = *guard;
    *guard = Some(syscall_process_id_provider);
    original
}

pub fn restore_task_provider(provider: Option<fn() -> u32>) {
    let mut guard = CURRENT_TASK_PROVIDER.lock();
    *guard = provider;
}

fn current_process_id() -> u32 {
    let guard = CURRENT_TASK_PROVIDER.lock();
    if let Some(cb) = *guard {
        cb()
    } else {
        crate::mm_constants::INVALID_PROCESS_ID
    }
}

fn current_process_dir() -> *mut crate::paging::ProcessPageDir {
    let pid = current_process_id();
    if pid == crate::mm_constants::INVALID_PROCESS_ID {
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

    if !KERNEL_GUARD_CHECKED.load(Ordering::Acquire) {
        let kernel_probe = mm_get_kernel_heap_start();
        if paging_is_user_accessible(dir, VirtAddr::new(kernel_probe)) != 0 {
            return Err(UserPtrError::NotMapped);
        }
        KERNEL_GUARD_CHECKED.store(true, Ordering::Release);
    }

    let start = user_addr.as_u64();
    let end = start + len as u64;
    let mut page = start & !(crate::mm_constants::PAGE_SIZE_4KB - 1);

    while page < end {
        if paging_is_user_accessible(dir, VirtAddr(page)) == 0 {
            return Err(UserPtrError::NotMapped);
        }
        page = page.wrapping_add(crate::mm_constants::PAGE_SIZE_4KB);
    }

    Ok(())
}

pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(src.addr(), core::mem::size_of::<T>(), dir)?;

    unsafe { Ok(ptr::read(src.as_ptr())) }
}

pub fn copy_to_user<T: Copy>(dst: UserPtr<T>, value: &T) -> Result<(), UserPtrError> {
    let dir = current_process_dir();
    validate_user_pages(dst.addr(), core::mem::size_of::<T>(), dir)?;

    unsafe {
        ptr::write(dst.as_mut_ptr(), *value);
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
