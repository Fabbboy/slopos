//! Safe accessors over `*mut Task` / `*const Task` raw pointers.
//!
//! The kernel's exception/IRQ glue receives task pointers from the
//! scheduler/dispatcher without carrying a reference's lifetime
//! information. These helpers absorb the unsafe deref into a single
//! crate so call-site files (`boot/`, `mm/`) stay in safe Rust.
//!
//! Each accessor null-checks the pointer; `Task` field reads use
//! [`core::ptr::read_unaligned`] for fields that the legacy
//! `task_struct` does not annotate as `repr(C, packed)` but where
//! callers historically used unaligned reads to be safe against
//! mid-update tearing on x86_64. Plain field reads (`(*p).field`) are
//! used where the field is a naturally-aligned scalar.
//!
//! All helpers return `Option<T>`; the caller threads the `None` case
//! through their existing diagnostics.

use slopos_abi::task::{TaskExitReason, TaskFaultReason};

use super::Task;
use super::task_pointer_is_valid;

/// Validate the pointer through [`task_pointer_is_valid`]. Wraps the
/// downstream null/whitelist check so callers can short-circuit
/// before touching any field.
#[inline]
pub fn task_validate(task: *const Task) -> Option<*const Task> {
    if task.is_null() {
        None
    } else if task_pointer_is_valid(task) {
        Some(task)
    } else {
        None
    }
}

/// Read the task's stable `task_id`.
#[inline]
pub fn task_id_of(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).task_id })
}

/// Read the task's `process_id`.
#[inline]
pub fn task_process_id(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).process_id })
}

/// Read the task's `flags` bitfield.
#[inline]
pub fn task_flags(task: *const Task) -> Option<u16> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u16.
    Some(unsafe { (*task).flags })
}

/// Read the task's user-mode `entry_point` virtual address.
#[inline]
pub fn task_entry_point(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u64.
    Some(unsafe { (*task).entry_point })
}

/// Read the task's kernel-stack `(base, top)` pair.
#[inline]
pub fn task_kernel_stack_bounds(task: *const Task) -> Option<(u64, u64)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; both fields are u64.
    let base = unsafe { (*task).kernel_stack_base };
    let top = unsafe { (*task).kernel_stack_top };
    Some((base, top))
}

/// Read the task's saved CR3 from its `TaskContext`. Uses
/// [`core::ptr::read_unaligned`] for parity with existing callers.
#[inline]
pub fn task_context_cr3(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; addr_of! produces a valid pointer
    // to the cr3 field; read_unaligned is safe regardless of the
    // surrounding context's alignment.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.cr3)) })
}

/// Read the task's saved instruction pointer from its `TaskContext`.
#[inline]
pub fn task_context_rip(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; read_unaligned is safe.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.rip)) })
}

/// Read the task's saved stack pointer from its `TaskContext`.
#[inline]
pub fn task_context_rsp(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; read_unaligned is safe.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.rsp)) })
}

/// Borrow the raw task-name byte array. The bytes are 0-padded; the
/// caller usually slices on the first NUL with `iter().position(|&b|
/// b == 0)`.
#[inline]
pub fn task_name_bytes<'a>(task: *const Task) -> Option<&'a [u8]> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `name` is a fixed-length byte
    // array embedded in `Task`. The returned slice's lifetime is
    // bounded by the caller's frame.
    Some(unsafe { &(*task).name })
}

/// Record a user-mode-fault exit on `task`: sets `exit_reason`,
/// `fault_reason`, and `exit_code`, then returns the task's id so the
/// caller can drive `task_terminate(tid)`.
///
/// Returns `None` if `task` is null. Used by the user-fault retire
/// path in `boot/src/user_fault.rs`.
#[inline]
pub fn task_record_user_fault_exit(task: *mut Task, reason: TaskFaultReason) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated `task`; the per-task spinlock that
    // gates context-switch is not held here, but the user-fault path
    // is single-threaded for the faulting CPU and the task is not
    // dispatchable while we mutate its exit state.
    unsafe {
        (*task).exit_reason = TaskExitReason::UserFault;
        (*task).fault_reason = reason;
        (*task).exit_code = 1;
        Some((*task).task_id)
    }
}
