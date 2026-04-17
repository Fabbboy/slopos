//! Kernel-wide allocation surface.
//!
//! This crate is the only kernel crate that depends on `alloc` directly
//! (via `extern crate alloc;`). Every other kernel crate must route heap
//! allocation through the primitives re-exported here. The wrappers exist
//! so that large structs cannot materialise on a caller's stack: the only
//! public constructor for `PinBox<T>` takes a `PinInit<T>`, and the only
//! constructors for `KBox<T>` / `KVec<T>` require `T: Zeroable` and
//! initialise the heap slot in place.
//!
//! Two `unsafe` blocks live in this module: `boxed_zeroed` and
//! `KVec::zeroed`. Both are guarded by a `T: Zeroable` bound that
//! certifies an all-zero bit pattern is a valid `T`.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ops::{Deref, DerefMut};
use core::pin::Pin;

use alloc::boxed::Box;

pub use alloc::alloc::AllocError;
pub use pinned_init::{
    InPlaceInit, Init, MaybeZeroable, PinInit, Zeroable, init_zeroed, pin_data, pinned_drop,
};

/// Kernel-wide pinned heap cell. The sole public constructor takes a
/// `PinInit<T>`, so a `T` value never materialises on a caller's stack.
pub struct PinBox<T: ?Sized> {
    inner: Pin<Box<T>>,
}

impl<T> PinBox<T> {
    /// Heap-allocate and pin-initialise a `T` in place.
    pub fn pin_init<E>(init: impl PinInit<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        Box::try_pin_init(init).map(|inner| Self { inner })
    }
}

impl<T: Zeroable> PinBox<T> {
    /// Heap-allocate a zero-initialised `T`. Safe because `T: Zeroable`
    /// certifies an all-zero bit pattern is a valid `T`.
    pub fn zeroed() -> Result<Self, AllocError> {
        boxed_zeroed()
    }
}

impl<T: ?Sized> Deref for PinBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized + Unpin> DerefMut for PinBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        Pin::get_mut(self.inner.as_mut())
    }
}

impl<T: ?Sized> PinBox<T> {
    /// Borrow the wrapped `Pin<&mut T>` without unpinning.
    pub fn as_pin_mut(&mut self) -> Pin<&mut T> {
        self.inner.as_mut()
    }
}

/// Heap-direct zeroed allocation of any `T: Zeroable`.
pub fn boxed_zeroed<T: Zeroable>() -> Result<PinBox<T>, AllocError> {
    let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit()?;
    // SAFETY: `T: Zeroable` ⇒ an all-zero bit pattern is a valid `T`.
    // `write_bytes` zeroes the whole allocation; `assume_init` is then
    // sound. `Box::into_pin` pins the result.
    let init = unsafe {
        let mut boxed = boxed;
        core::ptr::write_bytes(boxed.as_mut_ptr(), 0, 1);
        boxed.assume_init()
    };
    Ok(PinBox {
        inner: Box::into_pin(init),
    })
}

/// Kernel-blessed boxed slot. Fallible, zeroable-only.
pub struct KBox<T: Zeroable> {
    inner: Box<T>,
}

impl<T: Zeroable> KBox<T> {
    /// Heap-allocate and zero-initialise. `T: Zeroable` required.
    pub fn zeroed() -> Result<Self, AllocError> {
        let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit()?;
        // SAFETY: see `boxed_zeroed` above; same invariant.
        let inner = unsafe {
            let mut boxed = boxed;
            core::ptr::write_bytes(boxed.as_mut_ptr(), 0, 1);
            boxed.assume_init()
        };
        Ok(Self { inner })
    }
}

impl<T: Zeroable> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Zeroable> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

/// Kernel-blessed fallible `Vec<T>`. No by-value construction surface;
/// the only constructor takes an explicit `len` and zeroes the backing
/// memory.
pub struct KVec<T: Zeroable> {
    inner: alloc::vec::Vec<T>,
}

impl<T: Zeroable> KVec<T> {
    /// Allocate `len` zeroed elements. Fails with `AllocError` if the
    /// allocation cannot be satisfied.
    pub fn zeroed(len: usize) -> Result<Self, AllocError> {
        let mut v: alloc::vec::Vec<T> = alloc::vec::Vec::new();
        v.try_reserve_exact(len).map_err(|_| AllocError)?;
        // SAFETY: capacity ≥ len (just reserved). `T: Zeroable` ⇒ the
        // zeroed backing memory is a valid sequence of `T`s; we commit
        // that fact via `set_len` after zeroing.
        unsafe {
            core::ptr::write_bytes(v.as_mut_ptr(), 0, len);
            v.set_len(len);
        }
        Ok(Self { inner: v })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.inner
    }
}

impl<T: Zeroable> Deref for KVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.inner
    }
}

impl<T: Zeroable> DerefMut for KVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.inner
    }
}
