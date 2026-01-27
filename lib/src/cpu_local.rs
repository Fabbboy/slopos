//! Compile-time safe per-CPU data access.
//!
//! This module provides `CpuLocal<T>` for declaring per-CPU variables and
//! `CpuPinned<'a, T>` as an RAII guard that prevents CPU migration while
//! accessing per-CPU data.
//!
//! # Design Rationale
//!
//! Per-CPU data must only be accessed while "pinned" to the current CPU.
//! Migration during access would cause data corruption. This module enforces
//! this at compile-time by:
//!
//! 1. `CpuLocal<T>` stores one `T` per CPU but provides no direct access
//! 2. Access requires calling `.get()` which returns `CpuPinned<T>`
//! 3. `CpuPinned<T>` disables preemption (preventing migration) while held
//! 4. When `CpuPinned<T>` drops, preemption is re-enabled
//!
//! # Example
//!
//! ```ignore
//! cpu_local! {
//!     static MY_COUNTER: AtomicU64 = AtomicU64::new(0);
//! }
//!
//! fn increment() {
//!     let pinned = MY_COUNTER.get();
//!     pinned.fetch_add(1, Ordering::Relaxed);
//! }
//! ```

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::Deref;

use crate::percpu::{MAX_CPUS, get_current_cpu};
use crate::preempt::PreemptGuard;

#[repr(C, align(64))]
pub struct CacheAligned<T>(pub T);

impl<T: Copy> Copy for CacheAligned<T> {}
impl<T: Clone> Clone for CacheAligned<T> {
    fn clone(&self) -> Self {
        CacheAligned(self.0.clone())
    }
}

pub struct CpuLocal<T> {
    data: UnsafeCell<[CacheAligned<T>; MAX_CPUS]>,
}

// SAFETY: Each CPU accesses only its own slot while pinned.
// The CpuPinned guard ensures no migration occurs during access.
unsafe impl<T: Send> Sync for CpuLocal<T> {}

impl<T> CpuLocal<T> {
    pub const fn new_with(init: [CacheAligned<T>; MAX_CPUS]) -> Self {
        Self {
            data: UnsafeCell::new(init),
        }
    }

    #[inline]
    pub fn get(&self) -> CpuPinned<'_, T> {
        let guard = PreemptGuard::new();
        let cpu_id = get_current_cpu();
        // SAFETY: We hold PreemptGuard so we can't migrate.
        // cpu_id is always < MAX_CPUS.
        let ptr = unsafe { (*self.data.get()).get_unchecked(cpu_id) };
        CpuPinned {
            data: &ptr.0,
            _guard: guard,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn get_mut(&self) -> CpuPinnedMut<'_, T> {
        let guard = PreemptGuard::new();
        let cpu_id = get_current_cpu();
        // SAFETY: We hold PreemptGuard so we can't migrate.
        // cpu_id is always < MAX_CPUS. Mutable access is safe because
        // each CPU can only access its own slot while pinned.
        let ptr = unsafe { (*self.data.get()).get_unchecked_mut(cpu_id) };
        CpuPinnedMut {
            data: &mut ptr.0,
            _guard: guard,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub unsafe fn get_for_cpu(&self, cpu_id: usize) -> &T {
        debug_assert!(cpu_id < MAX_CPUS);
        &(*self.data.get()).get_unchecked(cpu_id).0
    }
}

pub struct CpuPinned<'a, T> {
    data: &'a T,
    _guard: PreemptGuard,
    _marker: PhantomData<*mut ()>,
}

impl<T> Deref for CpuPinned<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<T> CpuPinned<'_, T> {
    #[inline]
    pub fn cpu_id(&self) -> usize {
        get_current_cpu()
    }
}

pub struct CpuPinnedMut<'a, T> {
    data: &'a mut T,
    _guard: PreemptGuard,
    _marker: PhantomData<*mut ()>,
}

impl<T> Deref for CpuPinnedMut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<T> core::ops::DerefMut for CpuPinnedMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}

impl<T> CpuPinnedMut<'_, T> {
    #[inline]
    pub fn cpu_id(&self) -> usize {
        get_current_cpu()
    }
}

#[macro_export]
macro_rules! cpu_local {
    ($vis:vis static $NAME:ident: $ty:ty = $init:expr;) => {
        $vis static $NAME: $crate::cpu_local::CpuLocal<$ty> = {
            const INIT: $crate::cpu_local::CacheAligned<$ty> =
                $crate::cpu_local::CacheAligned($init);
            $crate::cpu_local::CpuLocal::new_with([INIT; $crate::percpu::MAX_CPUS])
        };
    };
}
