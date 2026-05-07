//! Compile-time safe per-CPU data access.
//!
//! This module provides `CpuLocal<T>` for declaring per-CPU variables and
//! `CpuPinned<'a, T>` as an RAII guard that prevents CPU migration while
//! accessing per-CPU data.
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

use crate::cpu::preempt::PreemptGuard;
use crate::cpu::x86_64::pcr::{MAX_CPUS, get_current_cpu};

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

    /// # Safety
    /// `cpu_id` must be a valid CPU index (< MAX_CPUS) and the caller must
    /// guarantee no concurrent mutation of that CPU's slot.
    #[inline]
    pub unsafe fn get_for_cpu(&self, cpu_id: usize) -> &T {
        debug_assert!(cpu_id < MAX_CPUS);
        // SAFETY: caller certifies cpu_id is in range and no concurrent mutation.
        unsafe { &(*self.data.get()).get_unchecked(cpu_id).0 }
    }

    /// Mutable slot access for shutdown-only / single-threaded callers.
    ///
    /// # Safety
    /// `cpu_id` must be < `MAX_CPUS`, and the caller must guarantee that no
    /// other thread is simultaneously borrowing slot `cpu_id` (typical use:
    /// shutdown drain, where SMP has already been quiesced).
    #[inline]
    pub unsafe fn get_for_cpu_mut(&self, cpu_id: usize) -> &mut T {
        debug_assert!(cpu_id < MAX_CPUS);
        // SAFETY: caller certifies cpu_id is in range and exclusive.
        unsafe { &mut (*self.data.get()).get_unchecked_mut(cpu_id).0 }
    }

    /// Visit every CPU's slot exactly once with mutable access.
    ///
    /// # Safety
    /// SMP must be quiesced — typically called only from `boot/shutdown.rs`'s
    /// drain pass, after the kernel has stopped scheduling and IPIs have
    /// been disabled. The closure receives one `&mut T` per CPU index in
    /// `[0, MAX_CPUS)`.
    pub unsafe fn for_each_mut(&self, mut f: impl FnMut(usize, &mut T)) {
        for cpu in 0..MAX_CPUS {
            // SAFETY: caller certifies SMP is quiesced.
            let slot = unsafe { self.get_for_cpu_mut(cpu) };
            f(cpu, slot);
        }
    }

    /// Safe wrapper around [`Self::for_each_mut`] for **single-threaded
    /// drain windows**: pre-SMP boot init and post-shutdown drain. The
    /// caller's contract is identical to `for_each_mut`'s — SMP must be
    /// either not yet active (early boot) or cooperatively quiesced
    /// (shutdown drain). The single internal `unsafe` reborrow is the
    /// only one in this code path.
    ///
    /// Use from per-CPU drain helpers (kernel heap, page allocator,
    /// stack-VA allocator) so the consumer side can stay safe.
    pub fn for_each_mut_at_shutdown(&self, f: impl FnMut(usize, &mut T)) {
        // SAFETY: caller guarantees the call site is single-threaded
        // (early boot or quiesced shutdown); the contract of
        // `for_each_mut` is satisfied.
        unsafe { self.for_each_mut(f) };
    }

    /// Mutable slot access for a caller that already holds a `PreemptGuard`.
    /// `cpu` must equal the current CPU — debug-asserted at entry.
    ///
    /// Use this from amortised hot paths that pin once via `PreemptGuard::new()`
    /// and then dispatch through the same cache multiple times. Cheaper than
    /// `get_mut()` because no per-call guard is created.
    #[inline]
    pub fn get_pinned_mut(&self, cpu: usize) -> &mut T {
        debug_assert!(
            PreemptGuard::is_active(),
            "CpuLocal::get_pinned_mut requires a held PreemptGuard"
        );
        debug_assert!(cpu < MAX_CPUS);
        debug_assert_eq!(
            cpu,
            get_current_cpu(),
            "CpuLocal::get_pinned_mut: cpu argument must match current CPU"
        );
        // SAFETY: PreemptGuard pins this thread to `cpu`; no other thread
        // can hold a borrow of the same slot at the same time.
        unsafe { &mut (*self.data.get()).get_unchecked_mut(cpu).0 }
    }

    /// Read-only slot access for any CPU. Suitable for cross-CPU diagnostic
    /// snapshots — `T` is responsible for its own atomicity (e.g. atomic
    /// counters on the cache stats; benign races on plain integer fields).
    #[inline]
    pub fn snapshot_for_cpu(&self, cpu: usize) -> Option<&T> {
        if cpu >= MAX_CPUS {
            return None;
        }
        // SAFETY: read-only borrow; the caller is responsible for tolerating
        // benign races on non-atomic fields. No mutation is possible through
        // this reference.
        Some(unsafe { &(*self.data.get()).get_unchecked(cpu).0 })
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
        $vis static $NAME: $crate::sync::cpu_local::CpuLocal<$ty> = {
            const INIT: $crate::sync::cpu_local::CacheAligned<$ty> =
                $crate::sync::cpu_local::CacheAligned($init);
            $crate::sync::cpu_local::CpuLocal::new_with([INIT; ::slopos_ostd::cpu::x86_64::pcr::MAX_CPUS])
        };
    };
}
