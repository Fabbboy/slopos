//! Typed wrapper for opaque `*mut c_void` callback contexts.
//!
//! Many kernel-half iteration helpers take a callback shaped like
//! `fn(item: &Foo, ctx: *mut c_void)`. The caller normally stashes a
//! `&mut MyCollector` in `ctx` and reborrows it inside the callback
//! with `unsafe { &mut *(ctx as *mut MyCollector) }`. [`CallbackCtx`]
//! centralises that unsafe reborrow so consumers stay fully safe.

use core::ffi::c_void;
use core::marker::PhantomData;

/// Type-erased mutable borrow into a caller-owned context.
///
/// Construct with [`CallbackCtx::from_raw`] inside a callback closure;
/// reach the typed reference with [`CallbackCtx::try_borrow`]. The
/// type parameter `T` is the *expected* concrete type — the caller is
/// responsible for matching the type at construction and use sites.
///
/// SAFETY contract: the `*mut c_void` must originate from the callback
/// invoker's typed `&mut T` cast (or a null pointer). Mismatched `T`
/// is undefined behavior at the `try_borrow` call.
pub struct CallbackCtx<'a, T> {
    ptr: *mut c_void,
    _phantom: PhantomData<&'a mut T>,
}

impl<'a, T> CallbackCtx<'a, T> {
    /// Wrap a raw `*mut c_void` for typed access.
    ///
    /// SAFETY (caller): the pointer must either be null or carry a
    /// live `&mut T` borrow that outlives `'a`. Mismatching `T`
    /// against the original cast is undefined behavior.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self {
            ptr,
            _phantom: PhantomData,
        }
    }

    /// Null-checked typed reborrow.
    ///
    /// The returned `&mut T` borrows from `&mut self`, so successive
    /// calls cannot alias.
    #[inline]
    pub fn try_borrow(&mut self) -> Option<&mut T> {
        if self.ptr.is_null() {
            return None;
        }
        // SAFETY: invariant from `from_raw`'s SAFETY contract — the
        // pointer carries a live `&mut T` whose lifetime covers `'a`,
        // and `&mut self` prevents aliasing within this scope.
        Some(unsafe { &mut *(self.ptr as *mut T) })
    }
}
