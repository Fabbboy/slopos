# User Pointer Type Safety Plan

**Status:** Draft
**Author:** Codex
**Date:** 2026-01-09

---

## Goal

Prevent user-controlled pointers from ever reaching kernel-only address constructors (like `VirtAddr::new`) by enforcing validated user-pointer types and crate boundaries. The intent is to make invalid user pointers a recoverable error rather than a kernel panic, and to prevent userland from accessing kernel-only address types at all.

---

## Design Overview

### 1) Kernel-only types

Introduce a kernel-only newtype that represents a validated user virtual address.

```rust
/// Kernel-only: validated user virtual address.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UserVirtAddr(VirtAddr);

#[derive(Copy, Clone, Debug)]
pub enum UserPtrError {
    NonCanonical,
    OutOfUserRange,
    Overflow,
}

impl UserVirtAddr {
    pub fn try_new(addr: u64, len: usize) -> Result<Self, UserPtrError> {
        let vaddr = VirtAddr::try_new(addr).ok_or(UserPtrError::NonCanonical)?;
        let end = addr.checked_add(len as u64).ok_or(UserPtrError::Overflow)?;
        if end < addr {
            return Err(UserPtrError::Overflow);
        }
        if !is_user_range(addr, end) {
            return Err(UserPtrError::OutOfUserRange);
        }
        Ok(Self(vaddr))
    }

    #[inline]
    pub fn as_vaddr(self) -> VirtAddr {
        self.0
    }
}
```

Add `is_user_range(start, end)` based on a single source of truth (e.g. `USER_VIRT_MAX`, `KERNEL_VIRT_BASE`, and HHDM base) so all checks are consistent.

### 2) API boundary

Change user-copy helpers to accept `UserVirtAddr` or `UserPtr<T>` rather than raw `u64`/`*const T`.

Example:
```rust
pub fn user_copy_from_user(
    kernel_dst: *mut c_void,
    user_src: UserVirtAddr,
    len: usize,
) -> c_int;
```

### 3) Crate separation / feature gating

Ensure userland cannot import kernel-only types.

Options:
- Move `UserVirtAddr` (and any `VirtAddr` constructors) into kernel-only crate (e.g., `slopos_mm`).
- Keep in `slopos_abi` but gate behind `cfg(feature = "kernel")`.

Userland should only depend on `slopos_abi` without the kernel feature.

---

## Proposed Work

### Phase A: Add kernel-only user pointer type
1. Define `UserVirtAddr` + `UserPtrError` in a kernel-only module.
2. Add `is_user_range` helper with canonical checks and bounds.
3. Add unit tests (if desired) for canonical/overflow/user-range behavior.

### Phase B: Convert user-copy APIs
1. Update `validate_user_buffer` to accept `UserVirtAddr` or to build it internally via `try_new`.
2. Replace `VirtAddr::new` usages on user-provided pointers with validated conversions.

### Phase C: Enforce crate boundary
1. Ensure userland builds cannot access kernel-only address types.
2. Update docs and comments to explain that user pointers must be validated types.

---

## Risks / Notes

- Requires minor API adjustments in syscalls or user-copy call sites.
- Needs clear user range constants (e.g., `USER_VIRT_MAX`).
- Avoids panics from malformed user pointers, matching Linux `access_ok` semantics.

---

## References

- Linux `access_ok` / `__range_ok` and `copy_from_user` semantics.
- Rust OS dev patterns for typed user pointers (`UserPtr<T>` / `UserAddress`).
